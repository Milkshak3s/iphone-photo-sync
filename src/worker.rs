//! The background thread: watches for iPhones, imports their photos and checks
//! for updates. The UI talks to it with [`Cmd`]s and reads [`Status`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui;

use crate::config::{Config, data_dir};
use crate::library::{Cancelled, Destinations, Library};
use crate::shell::ShellPhone;
use crate::transfer::Heartbeat;
use crate::update;
use crate::walk::{self, WalkProgress, WalkReport};
use crate::wpd::{self, DeviceInfo, Manager};

const POLL: Duration = Duration::from_secs(3);
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(60);
/// Upper bound on walks per connection.
const MAX_PASSES: usize = 5;
/// Consecutive passes without progress before giving up until the next connection.
const IDLE_PASSES: usize = 2;
const PASS_PAUSE: Duration = Duration::from_secs(3);
/// Silence from the phone after which a session is considered hung. A large
/// folder can legitimately take ~40 s to list on a healthy phone.
const HUNG_AFTER: Duration = Duration::from_secs(120);
/// Pause before listing a folder again after it failed.
const LIST_RETRY_PAUSE: Duration = Duration::from_secs(2);
const FIRST_UPDATE_CHECK: Duration = Duration::from_secs(20);
const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const LOG_LINES: usize = 200;

pub enum Cmd {
    SyncNow,
    Config(Config),
    CheckUpdate,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    NeedsFolder,
    Waiting,
    Locked {
        device: String,
    },
    Scanning {
        device: String,
    },
    Importing {
        device: String,
        folder: String,
        folders_done: usize,
        folders_known: usize,
        copied: usize,
    },
    Done {
        device: String,
        copied: usize,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateState {
    Disabled,
    NotChecked,
    Checking,
    UpToDate,
    Installing(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Status {
    pub phase: Phase,
    pub update: UpdateState,
    pub log: VecDeque<String>,
}

pub struct Shared {
    status: Mutex<Status>,
    pub cancel: AtomicBool,
    /// Set once an update is installed; the UI then restarts the app.
    pub restart: AtomicBool,
    /// Set when another process asks this one to quit (`--quit`).
    pub quit: AtomicBool,
    ctx: egui::Context,
}

impl Shared {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            status: Mutex::new(Status {
                phase: Phase::Waiting,
                update: UpdateState::NotChecked,
                log: VecDeque::new(),
            }),
            cancel: AtomicBool::new(false),
            restart: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            ctx,
        }
    }

    pub fn request_quit(&self) {
        self.quit.store(true, Ordering::Relaxed);
        self.cancel.store(true, Ordering::Relaxed);
        self.ctx.request_repaint();
    }

    pub fn status(&self) -> MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_phase(&self, phase: Phase) {
        let mut s = self.status();
        if s.phase != phase {
            s.phase = phase;
            drop(s);
            self.ctx.request_repaint();
        }
    }

    fn set_update(&self, state: UpdateState) {
        self.status().update = state;
        self.ctx.request_repaint();
    }

    fn log(&self, line: String) {
        log::info!("{line}");
        let mut s = self.status();
        let stamp = clock();
        s.log.push_front(format!("{stamp}  {line}"));
        s.log.truncate(LOG_LINES);
        drop(s);
        self.ctx.request_repaint();
    }
}

fn clock() -> String {
    // SAFETY: GetLocalTime only fills in a struct it returns by value.
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:02}:{:02}", t.wHour, t.wMinute)
}

pub fn run(shared: Arc<Shared>, rx: Receiver<Cmd>, mut config: Config, exited: Sender<()>) {
    if let Err(e) = wpd::init_com() {
        shared.set_phase(Phase::Error(format!("{e:#}")));
        return;
    }
    let mut worker = Worker {
        shared,
        manager: None,
        synced: HashSet::new(),
        retry_at: HashMap::new(),
        next_update_check: Instant::now() + FIRST_UPDATE_CHECK,
    };
    worker.refresh_update_state(&config);

    loop {
        match rx.recv_timeout(POLL) {
            Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Cmd::SyncNow) => {
                worker.synced.clear();
                worker.retry_at.clear();
            }
            Ok(Cmd::Config(c)) => {
                if c.dest != config.dest || c.video_dest != config.video_dest {
                    worker.synced.clear();
                    worker.retry_at.clear();
                }
                config = c;
                worker.refresh_update_state(&config);
            }
            Ok(Cmd::CheckUpdate) => worker.next_update_check = Instant::now(),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if worker.shared.cancel.load(Ordering::Relaxed) {
            break;
        }

        worker.poll_devices(&config);
        if worker.shared.cancel.load(Ordering::Relaxed) {
            break;
        }
        worker.maybe_update(&config);
        if worker.shared.restart.load(Ordering::Relaxed) {
            break;
        }
    }
    let _ = exited.send(());
}

struct Worker {
    shared: Arc<Shared>,
    manager: Option<Manager>,
    /// Devices fully imported since they were plugged in.
    synced: HashSet<String>,
    retry_at: HashMap<String, Instant>,
    next_update_check: Instant,
}

impl Worker {
    fn poll_devices(&mut self, config: &Config) {
        let Some(photos) = config.dest.clone() else {
            self.shared.set_phase(Phase::NeedsFolder);
            return;
        };
        let dest = Destinations {
            photos,
            videos: config.video_dest.clone(),
        };
        if self.manager.is_none() {
            match Manager::new() {
                Ok(m) => self.manager = Some(m),
                Err(e) => return self.shared.set_phase(Phase::Error(format!("{e:#}"))),
            }
        }
        let devices = match self.manager.as_ref().map(Manager::apple_devices) {
            Some(Ok(d)) => d,
            Some(Err(e)) => return self.shared.set_phase(Phase::Error(format!("{e:#}"))),
            None => return,
        };

        // Forget unplugged devices so they sync again next time.
        self.synced.retain(|id| devices.iter().any(|d| &d.id == id));
        self.retry_at
            .retain(|id, _| devices.iter().any(|d| &d.id == id));

        let pending: Vec<_> = devices
            .iter()
            .filter(|d| !self.synced.contains(&d.id))
            .filter(|d| {
                self.retry_at
                    .get(&d.id)
                    .is_none_or(|t| Instant::now() >= *t)
            })
            .cloned()
            .collect();
        if devices.is_empty() {
            let phase = self.shared.status().phase.clone();
            if !matches!(phase, Phase::Done { .. } | Phase::Error(_)) {
                self.shared.set_phase(Phase::Waiting);
            }
        }

        for device in pending {
            match self.sync(&device, &dest) {
                Ok(true) => {
                    self.synced.insert(device.id.clone());
                }
                Ok(false) => {}
                Err(e) if e.is::<Cancelled>() => return,
                Err(e) => {
                    self.shared.log(format!("{}: {e:#}", device.name));
                    self.shared.set_phase(Phase::Error(format!("{e:#}")));
                    self.retry_at
                        .insert(device.id.clone(), Instant::now() + RETRY_AFTER_ERROR);
                }
            }
        }
    }

    /// Imports everything missing from `device`, walking it again (up to a
    /// few times) while walks keep making progress on a phone that drops the
    /// connection. `Ok(false)` means the phone isn't readable yet (locked, or
    /// waiting for "Trust this computer").
    fn sync(&mut self, device: &DeviceInfo, dest: &Destinations) -> Result<bool> {
        let name = device.name.clone();
        let (mut copied, mut duplicates) = (0, 0);
        let mut idle_passes = 0;
        let mut last: Option<WalkReport> = None;

        for pass in 1..=MAX_PASSES {
            let outcome = self.run_session(device, dest, pass)?;
            copied += outcome.copied;
            duplicates += outcome.duplicates;
            match outcome.end {
                SessionEnd::NotReadable if pass == 1 => {
                    self.shared.set_phase(Phase::Locked { device: name });
                    return Ok(false);
                }
                SessionEnd::NotReadable => {}
                SessionEnd::Hung => self.shared.log(format!(
                    "{name}: the phone stopped responding; reconnecting"
                )),
                SessionEnd::Finished(report) => {
                    log::info!("{name}: walk {pass}: {report:?}");
                    let complete = report.complete();
                    last = Some(report);
                    if complete {
                        break;
                    }
                }
            }
            if outcome.copied + outcome.duplicates == 0 {
                idle_passes += 1;
            } else {
                idle_passes = 0;
            }
            if idle_passes >= IDLE_PASSES || !self.pause(PASS_PAUSE) {
                break;
            }
        }
        if self.shared.cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }

        let mut summary = format!("{name}: copied {copied} new");
        if duplicates > 0 {
            summary += &format!(", {duplicates} already on this PC");
        }
        self.shared.log(summary);
        match &last {
            None => self.shared.log(format!(
                "{name}: couldn't read the phone; it will be tried again when reconnected"
            )),
            Some(r) => {
                if r.unreadable > 0 {
                    self.shared.log(format!(
                        "{name}: {} items couldn't be read (often photos kept only in iCloud)",
                        r.unreadable
                    ));
                }
                if r.skipped_videos > 0 {
                    self.shared.log(format!(
                        "{name}: {} videos skipped (no videos folder chosen)",
                        r.skipped_videos
                    ));
                }
                if r.folders_failed > 0 {
                    self.shared.log(format!(
                        "{name}: {} folders couldn't be listed",
                        r.folders_failed
                    ));
                }
                if r.missing > 0 || r.session_lost {
                    self.shared.log(format!(
                        "{name}: not everything could be copied this time; the rest will be \
                         tried again next time the phone is connected"
                    ));
                }
            }
        }
        self.shared.set_phase(Phase::Done {
            device: name,
            copied,
        });
        Ok(true)
    }

    /// One walk on its own thread. A Shell or WPD call can hang indefinitely, so
    /// if the phone goes quiet for [`HUNG_AFTER`] the session is abandoned (and
    /// told to write nothing more).
    fn run_session(
        &self,
        device: &DeviceInfo,
        dest: &Destinations,
        pass: usize,
    ) -> Result<SessionOutcome> {
        let abort = Arc::new(AtomicBool::new(false));
        let heartbeat = Heartbeat::new();
        heartbeat.beat();
        let counts = Arc::new(Mutex::new(WalkProgress::default()));
        let (tx, rx) = mpsc::channel();
        {
            let (device, dest) = (device.clone(), dest.clone());
            let (abort, heartbeat, counts) = (abort.clone(), heartbeat.clone(), counts.clone());
            let shared = self.shared.clone();
            thread::Builder::new()
                .name(format!("session-{pass}"))
                .spawn(move || {
                    let end = session(&shared, &device, &dest, &abort, heartbeat, &counts);
                    let _ = tx.send(end);
                })?;
        }

        let end = loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(end)) => break end,
                Ok(Err(e)) => {
                    abort.store(true, Ordering::Relaxed);
                    if e.is::<Cancelled>() {
                        return Err(e);
                    }
                    log::warn!("{}: walk {pass}: {e:#}", device.name);
                    break SessionEnd::NotReadable;
                }
                Err(RecvTimeoutError::Disconnected) => break SessionEnd::NotReadable,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if self.shared.cancel.load(Ordering::Relaxed) {
                abort.store(true, Ordering::Relaxed);
                return Err(Cancelled.into());
            }
            if heartbeat.idle() > HUNG_AFTER {
                log::warn!(
                    "{}: walk {pass}: no response for {:?}, abandoning the session",
                    device.name,
                    heartbeat.idle()
                );
                abort.store(true, Ordering::Relaxed);
                break SessionEnd::Hung;
            }
        };
        let counts = counts.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Ok(SessionOutcome {
            copied: counts.copied,
            duplicates: counts.duplicates,
            end,
        })
    }

    /// Waits between passes. Returns false if the app is quitting.
    fn pause(&self, duration: Duration) -> bool {
        let until = Instant::now() + duration;
        while Instant::now() < until {
            if self.shared.cancel.load(Ordering::Relaxed) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        true
    }

    fn refresh_update_state(&self, config: &Config) {
        let enabled = config.auto_update && !cfg!(debug_assertions);
        let current = self.shared.status().update.clone();
        if !enabled {
            self.shared.set_update(UpdateState::Disabled);
        } else if current == UpdateState::Disabled {
            self.shared.set_update(UpdateState::NotChecked);
        }
    }

    fn maybe_update(&mut self, config: &Config) {
        if Instant::now() < self.next_update_check {
            return;
        }
        self.next_update_check = Instant::now() + UPDATE_INTERVAL;
        if !config.auto_update || cfg!(debug_assertions) {
            return;
        }
        self.shared.set_update(UpdateState::Checking);
        match update::check() {
            Ok(None) => self.shared.set_update(UpdateState::UpToDate),
            Ok(Some(u)) => {
                let version = u.version.to_string();
                self.shared
                    .set_update(UpdateState::Installing(version.clone()));
                match update::install(&u) {
                    Ok(()) => {
                        self.shared
                            .log(format!("installed update {version}, restarting"));
                        self.shared.restart.store(true, Ordering::Relaxed);
                        self.shared.ctx.request_repaint();
                    }
                    Err(e) => {
                        self.shared.log(format!("update {version} failed: {e:#}"));
                        self.shared
                            .set_update(UpdateState::Failed(format!("{e:#}")));
                    }
                }
            }
            Err(e) => {
                log::warn!("update check failed: {e:#}");
                self.shared
                    .set_update(UpdateState::Failed(format!("{e:#}")));
            }
        }
    }
}

struct SessionOutcome {
    copied: usize,
    duplicates: usize,
    end: SessionEnd,
}

enum SessionEnd {
    /// Couldn't open or list the phone (locked, untrusted, or misbehaving).
    NotReadable,
    /// The phone stopped answering; the session was abandoned.
    Hung,
    Finished(WalkReport),
}

/// Body of a session thread. It owns its own COM objects and library handle,
/// so an abandoned session can't interfere with the next one: once `abort` is
/// set it stores nothing and doesn't save the import state.
fn session(
    shared: &Shared,
    device: &DeviceInfo,
    dest: &Destinations,
    abort: &AtomicBool,
    heartbeat: Heartbeat,
    counts: &Mutex<WalkProgress>,
) -> Result<SessionEnd> {
    wpd::init_com()?;
    let name = &device.name;
    shared.set_phase(Phase::Scanning {
        device: name.clone(),
    });
    let mut phone = match ShellPhone::open(name, heartbeat) {
        Ok(p) => p,
        Err(e) => {
            log::info!("{name}: {e:#}");
            return Ok(SessionEnd::NotReadable);
        }
    };
    let mut library = Library::open(dest, &data_dir().join("library.json"))?;
    let result = walk::walk_and_copy(
        &mut library,
        &mut phone,
        abort,
        LIST_RETRY_PAUSE,
        &mut |p| {
            *counts.lock().unwrap_or_else(|e| e.into_inner()) = p.clone();
            shared.set_phase(Phase::Importing {
                device: name.clone(),
                folder: p.folder.clone(),
                folders_done: p.folders_done,
                folders_known: p.folders_known,
                copied: p.copied,
            });
        },
    );
    if abort.load(Ordering::Relaxed) {
        // Abandoned: the next session owns the import state now.
        return Err(Cancelled.into());
    }
    let report = match result {
        Ok(r) => r,
        Err(e) if e.is::<Cancelled>() => return Err(e),
        Err(e) => {
            log::info!("{name}: can't read the phone: {e:#}");
            return Ok(SessionEnd::NotReadable);
        }
    };
    library.finish()?;
    // A locked phone shows an empty "Internal Storage".
    if report.files_seen == 0 && report.unreadable == 0 && !report.session_lost {
        return Ok(SessionEnd::NotReadable);
    }
    Ok(SessionEnd::Finished(report))
}

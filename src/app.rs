use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};

use crate::config::Config;
use crate::tray::{Action, Tray};
use crate::worker::{self, Cmd, Phase, Shared, UpdateState};
use crate::{autostart, update};

pub const TITLE: &str = "iPhone Photo Sync";

pub struct App {
    config: Config,
    shared: Arc<Shared>,
    cmd: Sender<Cmd>,
    worker_exited: Receiver<()>,
    tray: Tray,
    quitting: bool,
    /// First-run choices, before they are saved.
    setup_dest: Option<PathBuf>,
    setup_video_dest: Option<PathBuf>,
    setup_autostart: bool,
    error: Option<String>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, config: Config) -> anyhow::Result<Self> {
        let ctx = cc.egui_ctx.clone();
        let tray = Tray::new(&ctx)?;
        let shared = Arc::new(Shared::new(ctx));
        {
            let shared = shared.clone();
            crate::instance::watch_quit(move || shared.request_quit());
        }
        let (cmd, rx) = mpsc::channel();
        let (exited_tx, worker_exited) = mpsc::channel();
        {
            let (shared, config) = (shared.clone(), config.clone());
            thread::Builder::new()
                .name("worker".into())
                .spawn(move || worker::run(shared, rx, config, exited_tx))?;
        }
        Ok(Self {
            setup_dest: dirs::picture_dir().map(|p| p.join("iPhone")),
            setup_video_dest: dirs::video_dir().map(|p| p.join("iPhone")),
            setup_autostart: config.start_on_login,
            config,
            shared,
            cmd,
            worker_exited,
            tray,
            quitting: false,
            error: None,
        })
    }

    fn apply(&mut self, config: Config) {
        if (config.start_on_login != self.config.start_on_login || self.config.dest.is_none())
            && let Err(e) = autostart::set(config.start_on_login)
        {
            self.error = Some(format!("Couldn't change the startup setting: {e:#}"));
        }
        if let Err(e) = config.save() {
            self.error = Some(format!("Couldn't save settings: {e:#}"));
        }
        self.config = config.clone();
        let _ = self.cmd.send(Cmd::Config(config));
    }

    fn show_window(ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Stops the worker (letting it finish or abandon the current file) and closes.
    fn shut_down(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        self.shared.cancel.store(true, Ordering::Relaxed);
        let _ = self.cmd.send(Cmd::Quit);
        let _ = self.worker_exited.recv_timeout(Duration::from_secs(5));
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn tooltip(&self) -> String {
        let phase = self.shared.status().phase.clone();
        let detail = match phase {
            Phase::NeedsFolder => "choose a folder to get started".into(),
            Phase::Waiting => "waiting for an iPhone".into(),
            Phase::Locked { device } => format!("unlock {device} and tap Trust"),
            Phase::Scanning { device } => format!("reading {device}…"),
            Phase::Importing {
                folders_done,
                folders_known,
                copied,
                ..
            } => format!("folder {folders_done}/{folders_known}, {copied} new"),
            Phase::Done { copied, .. } => format!("up to date ({copied} new)"),
            Phase::Error(_) => "something went wrong".into(),
        };
        format!("{TITLE}: {detail}")
    }

    fn setup_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.heading("Welcome to iPhone Photo Sync");
        ui.add_space(6.0);
        ui.label(
            "Plug your iPhone into this PC with a USB cable and its photos (and, if you \
             like, videos) are copied to folders of your choice. Nothing is ever overwritten \
             or deleted, and anything already in those folders is not copied twice.",
        );
        ui.add_space(14.0);

        ui.label(RichText::new("Copy photos to").strong());
        ui.horizontal(|ui| {
            let text = self
                .setup_dest
                .as_deref()
                .map_or("No folder chosen".into(), |p| p.display().to_string());
            ui.add(egui::Label::new(RichText::new(text).monospace()).truncate());
        });
        if ui.button("Choose folder…").clicked()
            && let Some(dir) = pick_folder(self.setup_dest.as_deref())
        {
            self.setup_dest = Some(dir);
        }
        ui.add_space(10.0);

        ui.label(RichText::new("Copy videos to").strong());
        let text = self
            .setup_video_dest
            .as_deref()
            .map_or("Don't copy videos".into(), |p| p.display().to_string());
        ui.add(egui::Label::new(RichText::new(text).monospace()).truncate());
        ui.horizontal(|ui| {
            if ui.button("Choose folder…").clicked()
                && let Some(dir) = pick_folder(self.setup_video_dest.as_deref())
            {
                self.setup_video_dest = Some(dir);
            }
            if self.setup_video_dest.is_some() && ui.button("Don't copy videos").clicked() {
                self.setup_video_dest = None;
            }
        });
        ui.add_space(10.0);
        ui.checkbox(&mut self.setup_autostart, "Start when I sign in to Windows");
        ui.add_space(14.0);

        let ready = self.setup_dest.is_some();
        if ui
            .add_enabled(
                ready,
                egui::Button::new(RichText::new("Start syncing").strong()),
            )
            .clicked()
        {
            let config = Config {
                dest: self.setup_dest.clone(),
                video_dest: self.setup_video_dest.clone(),
                start_on_login: self.setup_autostart,
                ..self.config.clone()
            };
            self.apply(config);
        }
    }

    fn main_ui(&mut self, ui: &mut egui::Ui) {
        let status = self.shared.status().clone();
        ui.add_space(4.0);
        ui.heading(TITLE);
        ui.add_space(8.0);

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let (headline, detail, color) = describe(&status.phase);
            ui.label(RichText::new(headline).size(16.0).strong().color(color));
            if let Some(detail) = detail {
                ui.label(detail);
            }
            if let Phase::Importing {
                folders_done,
                folders_known,
                ..
            } = status.phase
            {
                let fraction = folders_done as f32 / folders_known.max(1) as f32;
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .text(format!("folder {folders_done} of {folders_known}")),
                );
            }
            ui.horizontal(|ui| {
                if ui.button("Sync now").clicked() {
                    let _ = self.cmd.send(Cmd::SyncNow);
                }
            });
        });
        ui.add_space(10.0);

        ui.label(RichText::new("Photos folder").strong());
        let dest = self.config.dest.clone().unwrap_or_default();
        ui.add(egui::Label::new(RichText::new(dest.display().to_string()).monospace()).truncate());
        ui.horizontal(|ui| {
            if ui.button("Open").clicked() {
                open_folder(&dest);
            }
            if ui.button("Change…").clicked()
                && let Some(dir) = pick_folder(Some(&dest))
            {
                let config = Config {
                    dest: Some(dir),
                    ..self.config.clone()
                };
                self.apply(config);
            }
        });
        ui.add_space(8.0);

        ui.label(RichText::new("Videos folder").strong());
        match self.config.video_dest.clone() {
            Some(videos) => {
                ui.add(
                    egui::Label::new(RichText::new(videos.display().to_string()).monospace())
                        .truncate(),
                );
                ui.horizontal(|ui| {
                    if ui.button("Open").clicked() {
                        open_folder(&videos);
                    }
                    if ui.button("Change…").clicked()
                        && let Some(dir) = pick_folder(Some(&videos))
                    {
                        let config = Config {
                            video_dest: Some(dir),
                            ..self.config.clone()
                        };
                        self.apply(config);
                    }
                    if ui.button("Stop copying videos").clicked() {
                        let config = Config {
                            video_dest: None,
                            ..self.config.clone()
                        };
                        self.apply(config);
                    }
                });
            }
            None => {
                ui.label(RichText::new("Not set: videos are skipped.").weak());
                if ui.button("Choose folder…").clicked()
                    && let Some(dir) = pick_folder(dirs::video_dir().as_deref())
                {
                    let config = Config {
                        video_dest: Some(dir),
                        ..self.config.clone()
                    };
                    self.apply(config);
                }
            }
        }
        ui.add_space(10.0);

        let mut config = self.config.clone();
        ui.checkbox(
            &mut config.start_on_login,
            "Start when I sign in to Windows",
        );
        ui.checkbox(&mut config.auto_update, "Install updates automatically");
        if config != self.config {
            self.apply(config);
        }
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "Version {} · {}",
                    update::current_version(),
                    describe_update(&status.update)
                ))
                .weak(),
            );
            if self.config.auto_update
                && !matches!(status.update, UpdateState::Disabled | UpdateState::Checking)
                && ui.small_button("Check now").clicked()
            {
                let _ = self.cmd.send(Cmd::CheckUpdate);
            }
        });

        if let Some(err) = &self.error {
            ui.add_space(6.0);
            ui.colored_label(Color32::from_rgb(220, 80, 60), err);
        }

        ui.add_space(10.0);
        ui.separator();
        ui.label(RichText::new("Activity").strong());
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                if status.log.is_empty() {
                    ui.label(RichText::new("Nothing yet.").weak());
                }
                for line in &status.log {
                    ui.label(RichText::new(line).monospace().size(12.0));
                }
            });
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let actions: Vec<_> = self.tray.actions().collect();
        for action in actions {
            match action {
                Action::Show => Self::show_window(ctx),
                Action::SyncNow => {
                    let _ = self.cmd.send(Cmd::SyncNow);
                }
                Action::OpenFolder => match &self.config.dest {
                    Some(dest) => open_folder(dest),
                    None => Self::show_window(ctx),
                },
                Action::Quit => self.shut_down(ctx),
            }
        }

        // Closing the window keeps the app running in the tray.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        if self.shared.quit.load(Ordering::Relaxed) && !self.quitting {
            self.shut_down(ctx);
        }

        if self.shared.restart.load(Ordering::Relaxed) && !self.quitting {
            self.shut_down(ctx);
            if let Err(e) = update::relaunch() {
                log::error!("restarting after update: {e:#}");
            }
        }

        let tooltip = self.tooltip();
        self.tray.set_tooltip(&tooltip);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Frame::central_panel(ui.style())
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                if self.config.dest.is_none() {
                    self.setup_ui(ui);
                } else {
                    self.main_ui(ui);
                }
            });
    }
}

fn describe(phase: &Phase) -> (String, Option<String>, Color32) {
    let ok = Color32::from_rgb(60, 170, 90);
    let warn = Color32::from_rgb(230, 150, 40);
    let busy = Color32::from_rgb(60, 140, 230);
    match phase {
        Phase::NeedsFolder => ("Choose a folder".into(), None, warn),
        Phase::Waiting => (
            "Waiting for an iPhone".into(),
            Some("Connect your iPhone with a USB cable.".into()),
            Color32::GRAY,
        ),
        Phase::Locked { device } => (
            format!("Unlock {device}"),
            Some("Unlock the phone and tap \"Trust\" if it asks about this computer.".into()),
            warn,
        ),
        Phase::Scanning { device } => (format!("Reading {device}…"), None, busy),
        Phase::Importing {
            device,
            folder,
            copied,
            ..
        } => (
            format!("Copying from {device}…"),
            Some(match copied {
                1 => format!("1 new item so far · reading {folder}"),
                n => format!("{n} new items so far · reading {folder}"),
            }),
            busy,
        ),
        Phase::Done { device, copied } => (
            "Up to date".into(),
            Some(match copied {
                0 => format!("No new photos on {device}."),
                1 => format!("Copied 1 new item from {device}."),
                n => format!("Copied {n} new items from {device}."),
            }),
            ok,
        ),
        Phase::Error(e) => ("Something went wrong".into(), Some(e.clone()), warn),
    }
}

fn describe_update(state: &UpdateState) -> String {
    match state {
        UpdateState::Disabled => "automatic updates off".into(),
        UpdateState::NotChecked => "not checked for updates yet".into(),
        UpdateState::Checking => "checking for updates…".into(),
        UpdateState::UpToDate => "up to date".into(),
        UpdateState::Installing(v) => format!("installing {v}…"),
        UpdateState::Failed(_) => "update check failed".into(),
    }
}

fn pick_folder(start: Option<&Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title("Choose where to copy photos");
    if let Some(dir) = start.and_then(|p| p.ancestors().find(|a| a.is_dir())) {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder()
}

fn open_folder(path: &Path) {
    let _ = std::fs::create_dir_all(path);
    let _ = Command::new("explorer").arg(path).spawn();
}

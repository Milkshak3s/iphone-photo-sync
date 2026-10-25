//! Reading the iPhone through the Windows Shell: the same route Explorer takes
//! (This PC > Apple iPhone > Internal Storage > 202403_a > IMG_….HEIC). On a
//! flaky phone it proved far steadier than talking to WPD directly.
//!
//! Everything here runs on one thread with COM initialised multithreaded
//! (`wpd::init_com`): a single-threaded apartment would need a message loop,
//! and the Shell can block waiting for one.

use std::collections::HashMap;
use std::io::{self, Read};
use std::time::Instant;

use anyhow::{Context, Result};
use windows::Win32::Storage::EnhancedStorage::{PKEY_FileName, PKEY_Size};
use windows::Win32::System::Com::IStream;
use windows::Win32::System::SystemServices::{SFGAO_FOLDER, SFGAO_STREAM};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, BHID_Stream, FOLDERID_ComputerFolder, IEnumShellItems, IShellItem, IShellItem2,
    KF_FLAG_DEFAULT, SHGetKnownFolderItem, SIGDN_NORMALDISPLAY,
};
use windows::core::Interface;

use crate::library::{
    Listing, PhoneFolder, PhoneItem, PhoneSource, SessionLost, month_from_folder,
};
use crate::transfer::{Heartbeat, looks_like};
use crate::wpd::{self, Manager, take};

/// HRESULTs that mean the phone's connection is gone and it must be reopened.
const SESSION_LOST: [u32; 6] = [
    0x802A_0002, // "Shutdown was already called"
    0x8007_001F, // "A device attached to the system is not functioning"
    0x8007_048F, // "The device is not connected"
    0x8007_0015, // "The device is not ready"
    0x8007_001E, // "The system cannot read from the specified device"
    0x8007_0491, // "There was no match for the specified key"
];

fn is_session_lost(e: &windows::core::Error) -> bool {
    SESSION_LOST.contains(&(e.code().0 as u32))
}

/// Converts a Shell error, marking the ones that mean the connection is gone.
fn shell_error(e: windows::core::Error) -> anyhow::Error {
    if is_session_lost(&e) {
        SessionLost(e.message()).into()
    } else {
        e.into()
    }
}

fn display_name(item: &IShellItem) -> String {
    // SAFETY: the returned string is freed by `take`.
    unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY) }
        .map(take)
        .unwrap_or_default()
}

fn children(item: &IShellItem) -> windows::core::Result<Vec<IShellItem>> {
    // SAFETY: COM calls on interfaces we hold; `Next` fills at most `buf.len()`
    // entries and reports how many.
    unsafe {
        let items: IEnumShellItems = item.BindToHandler(None, &BHID_EnumItems)?;
        let mut out = Vec::new();
        loop {
            let mut buf: [Option<IShellItem>; 32] = Default::default();
            let mut fetched = 0u32;
            let r = items.Next(&mut buf, Some(&mut fetched));
            out.extend(buf.into_iter().take(fetched as usize).flatten());
            r?;
            if fetched == 0 {
                break;
            }
        }
        Ok(out)
    }
}

/// The phone, opened through the Shell.
pub struct ShellPhone {
    root: IShellItem,
    root_name: String,
    /// Handles given out as `PhoneFolder::id` / `PhoneItem::id`.
    handles: HashMap<String, IShellItem>,
    next: u64,
    heartbeat: Heartbeat,
}

impl ShellPhone {
    /// Finds the phone under This PC. `device_name` (from WPD) picks between
    /// several connected Apple devices.
    pub fn open(device_name: &str, heartbeat: Heartbeat) -> Result<Self> {
        heartbeat.beat();
        // SAFETY: COM call returning an interface we then own.
        let pc: IShellItem =
            unsafe { SHGetKnownFolderItem(&FOLDERID_ComputerFolder, KF_FLAG_DEFAULT, None) }
                .context("opening This PC")?;
        let mut candidates: Vec<(String, IShellItem)> = children(&pc)
            .context("listing This PC")?
            .into_iter()
            .map(|i| (display_name(&i), i))
            .filter(|(n, _)| {
                let n = n.to_lowercase();
                n.contains("iphone") || n.contains("ipad") || n.contains("apple")
            })
            .collect();
        heartbeat.beat();
        let pick = candidates
            .iter()
            .position(|(n, _)| n == device_name)
            .unwrap_or(0);
        if candidates.is_empty() {
            anyhow::bail!("{device_name} doesn't appear under This PC yet");
        }
        let (root_name, root) = candidates.swap_remove(pick);
        Ok(Self {
            root,
            root_name,
            handles: HashMap::new(),
            next: 0,
            heartbeat,
        })
    }

    fn keep(&mut self, item: IShellItem) -> String {
        self.next += 1;
        let id = format!("h{}", self.next);
        self.handles.insert(id.clone(), item);
        id
    }

    fn handle(&self, id: &str) -> Result<&IShellItem> {
        if id == "root" {
            return Ok(&self.root);
        }
        self.handles
            .get(id)
            .with_context(|| format!("unknown handle {id}"))
    }
}

impl PhoneSource for ShellPhone {
    fn root(&mut self) -> Result<PhoneFolder> {
        Ok(PhoneFolder {
            id: "root".into(),
            name: self.root_name.clone(),
        })
    }

    fn list(&mut self, folder: &PhoneFolder) -> Result<Listing> {
        self.heartbeat.beat();
        let items = children(self.handle(&folder.id)?).map_err(shell_error)?;
        self.heartbeat.beat();
        let month = month_from_folder(&folder.name);
        let mut listing = Listing::default();
        for item in items {
            // SAFETY: COM calls on an interface we hold; strings freed by `take`.
            let attrs = unsafe { item.GetAttributes(SFGAO_FOLDER | SFGAO_STREAM) };
            let Ok(attrs) = attrs else {
                listing.unreadable += 1;
                continue;
            };
            let is_folder = attrs.0 & SFGAO_FOLDER.0 != 0 && attrs.0 & SFGAO_STREAM.0 == 0;
            if is_folder {
                let name = display_name(&item);
                let id = self.keep(item);
                listing.folders.push(PhoneFolder { id, name });
                continue;
            }
            let props = item.cast::<IShellItem2>().and_then(|i2| unsafe {
                let size = i2.GetUInt64(&PKEY_Size)?;
                let name = take(i2.GetString(&PKEY_FileName)?);
                Ok((name, size))
            });
            match props {
                Ok((name, size)) if !name.is_empty() => {
                    let id = self.keep(item);
                    listing.files.push(PhoneItem {
                        id,
                        name,
                        size,
                        month,
                        video: false,
                    });
                }
                // Typically photos kept only in iCloud.
                _ => listing.unreadable += 1,
            }
            self.heartbeat.beat();
        }
        Ok(listing)
    }

    fn open(&mut self, item: &PhoneItem) -> Result<Box<dyn Read + '_>> {
        self.heartbeat.beat();
        let handle = self.handle(&item.id)?;
        // SAFETY: COM call returning an interface we then own.
        let stream: IStream =
            unsafe { handle.BindToHandler(None, &BHID_Stream) }.map_err(shell_error)?;
        self.heartbeat.beat();
        Ok(Box::new(StreamReader(stream, self.heartbeat.clone())))
    }
}

struct StreamReader(IStream, Heartbeat);

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(u32::MAX as usize) as u32;
        let mut read = 0u32;
        // SAFETY: `buf` is valid for `len` bytes of writes.
        unsafe { self.0.Read(buf.as_mut_ptr().cast(), len, Some(&mut read)) }
            .ok()
            .map_err(|e| {
                if is_session_lost(&e) {
                    io::Error::other(SessionLost(e.message()))
                } else {
                    io::Error::other(e)
                }
            })?;
        self.1.beat();
        Ok(read as usize)
    }
}

/// `--diagnose`: walks the whole phone (reading nothing but the folder tree),
/// printing a line per folder, then reads three files into memory to check
/// they match their listed size and format. Writes nothing.
pub fn diagnose() -> Result<()> {
    wpd::init_com()?;
    let device = Manager::new()?
        .apple_devices()?
        .into_iter()
        .next()
        .context("no iPhone connected")?;
    let mut phone = ShellPhone::open(&device.name, Heartbeat::new())?;
    let started = Instant::now();
    let say = |line: String| {
        println!("{line}");
        log::info!("diagnose: {line}");
    };
    say(format!("found {}", device.name));

    let mut queue = std::collections::VecDeque::from([phone.root()?]);
    let (mut folders, mut failed, mut files, mut unreadable, mut bytes) = (0, 0, 0, 0, 0u64);
    let mut sample = Vec::new();
    while let Some(folder) = queue.pop_front() {
        let t = Instant::now();
        match phone.list(&folder) {
            Ok(listing) => {
                folders += 1;
                files += listing.files.len();
                unreadable += listing.unreadable;
                bytes += listing.files.iter().map(|f| f.size).sum::<u64>();
                say(format!(
                    "{:6.1}s  {}: {} files, {} folders, {} unreadable ({:.1?})",
                    started.elapsed().as_secs_f32(),
                    folder.name,
                    listing.files.len(),
                    listing.folders.len(),
                    listing.unreadable,
                    t.elapsed()
                ));
                if !listing.files.is_empty() {
                    sample = listing.files.clone();
                }
                queue.extend(listing.folders);
            }
            Err(e) => {
                failed += 1;
                say(format!("{}: listing failed: {e:#}", folder.name));
            }
        }
    }
    say(format!(
        "{folders} folders ({failed} failed), {files} files ({:.1} GB), {unreadable} unreadable, in {:.0?}",
        bytes as f64 / 1e9,
        started.elapsed()
    ));
    for item in sample.iter().take(3) {
        let mut data = Vec::new();
        let verdict = match phone
            .open(item)
            .and_then(|mut r| Ok(r.read_to_end(&mut data)?))
        {
            Err(e) => format!("read failed: {e:#}"),
            Ok(n) if n as u64 != item.size => format!("WRONG: listed {} bytes, got {n}", item.size),
            Ok(_) if !looks_like(&item.name, &data[..data.len().min(32)]) => {
                "WRONG: not the format its name says".into()
            }
            Ok(_) => "ok".into(),
        };
        say(format!("read check {}: {verdict}", item.name));
    }
    Ok(())
}

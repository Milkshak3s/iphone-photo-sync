//! A minimal file logger: `%LOCALAPPDATA%\iPhonePhotoSync\log.txt`, rolled over
//! to `log.old.txt` at 2 MB.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

use log::{LevelFilter, Log, Metadata, Record};

use crate::config::data_dir;

const MAX_BYTES: u64 = 2 * 1024 * 1024;

struct FileLogger(Mutex<Option<File>>);

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.target().starts_with(env!("CARGO_CRATE_NAME"))
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // SAFETY: GetLocalTime only fills in a struct it returns by value.
        let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        let line = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {:5} {}\n",
            t.wYear,
            t.wMonth,
            t.wDay,
            t.wHour,
            t.wMinute,
            t.wSecond,
            record.level(),
            record.args()
        );
        if cfg!(debug_assertions) {
            eprint!("{line}");
        }
        if let Ok(mut file) = self.0.lock()
            && let Some(f) = file.as_mut()
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.0.lock()
            && let Some(f) = file.as_mut()
        {
            let _ = f.flush();
        }
    }
}

pub fn init() {
    let dir = data_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("log.txt");
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = fs::rename(&path, dir.join("log.old.txt"));
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    let logger = Box::leak(Box::new(FileLogger(Mutex::new(file))));
    if log::set_logger(logger).is_ok() {
        // `IPS_LOG=debug` for troubleshooting.
        let level = match std::env::var("IPS_LOG").as_deref() {
            Ok("debug") => LevelFilter::Debug,
            _ => LevelFilter::Info,
        };
        log::set_max_level(level);
    }
}

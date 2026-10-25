//! One running copy per user. A second launch brings the first one's window up.

use std::time::Duration;

use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent,
    WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
};
use windows::core::{HSTRING, w};

/// Takes the per-user lock, waiting up to `wait` for a previous copy to exit
/// (used right after a self-update). Returns false if another copy holds it.
pub fn acquire(wait: Duration) -> bool {
    // SAFETY: plain Win32 calls; the mutex handle is deliberately leaked so the
    // lock is held until the process exits.
    unsafe {
        let Ok(mutex) = CreateMutexW(None, false, w!("Local\\iPhonePhotoSync-single-instance"))
        else {
            return true;
        };
        let r = WaitForSingleObject(mutex, wait.as_millis().min(u32::MAX as u128) as u32);
        r == WAIT_OBJECT_0 || r == WAIT_ABANDONED
    }
}

/// Shows the window of the copy that is already running.
pub fn show_existing(title: &str) {
    // SAFETY: plain Win32 calls on a window handle we just looked up.
    unsafe {
        if let Ok(hwnd) = FindWindowW(None, &HSTRING::from(title)) {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

const QUIT_EVENT: windows::core::PCWSTR = w!(r"Local\iPhonePhotoSync-quit");

/// Asks the running copy to quit (`--quit`). Returns false if none is running.
pub fn request_quit() -> bool {
    // SAFETY: plain Win32 calls on a handle we just opened.
    unsafe {
        match OpenEventW(EVENT_MODIFY_STATE, false, QUIT_EVENT) {
            Ok(event) => SetEvent(event).is_ok(),
            Err(_) => false,
        }
    }
}

/// Calls `on_quit` on a background thread when another process runs `--quit`.
pub fn watch_quit(on_quit: impl FnOnce() + Send + 'static) {
    // SAFETY: the event handle is deliberately leaked; it lives as long as the process.
    let Ok(event) = (unsafe { CreateEventW(None, true, false, QUIT_EVENT) }) else {
        return;
    };
    let event = event.0 as usize;
    std::thread::spawn(move || {
        let event = windows::Win32::Foundation::HANDLE(event as *mut _);
        // SAFETY: waiting on our own, still-open event handle.
        if unsafe { WaitForSingleObject(event, INFINITE) } == WAIT_OBJECT_0 {
            on_quit();
        }
    });
}

//! Noticing iPhones: Windows Portable Devices (WPD) lists connected devices
//! without opening a session with them, which makes it a cheap, harmless poll.
//! Reading the phone goes through the Shell instead (see `shell.rs`).
//!
//! COM must be initialised on the calling thread ([`init_com`]).

use anyhow::{Context, Result};
use windows::Win32::Devices::PortableDevices::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::core::{HSTRING, PWSTR};

pub fn init_com() -> Result<()> {
    // SAFETY: plain COM initialisation for the calling thread.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("initialising COM")
}

/// Takes ownership of a COM-allocated string.
pub(crate) fn take(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: `p` is a NUL-terminated string allocated with CoTaskMemAlloc that
    // we now own; it is freed exactly once here.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// PnP device path; stable for a given phone and USB port.
    pub id: String,
    pub name: String,
}

pub struct Manager(IPortableDeviceManager);

impl Manager {
    pub fn new() -> Result<Self> {
        // SAFETY: COM is initialised on this thread (see module docs).
        let manager =
            unsafe { CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER) }
                .context("creating the portable device manager")?;
        Ok(Self(manager))
    }

    /// Connected Apple devices (iPhone / iPad).
    pub fn apple_devices(&self) -> Result<Vec<DeviceInfo>> {
        // SAFETY: the buffers passed to GetDevices are sized from its own count,
        // and every returned string is freed by `take`.
        let ids = unsafe {
            self.0.RefreshDeviceList()?;
            let mut count = 0u32;
            self.0.GetDevices(std::ptr::null_mut(), &mut count)?;
            let mut ids = vec![PWSTR::null(); count as usize];
            if count > 0 {
                self.0.GetDevices(ids.as_mut_ptr(), &mut count)?;
            }
            ids.truncate(count as usize);
            ids.into_iter().map(take).collect::<Vec<_>>()
        };

        let mut out = Vec::new();
        for id in ids {
            let hid = HSTRING::from(id.as_str());
            // SAFETY: each call gets a buffer of the length it asked for.
            let (manufacturer, friendly, description) = unsafe {
                (
                    device_string(|b, n| self.0.GetDeviceManufacturer(&hid, b, n)),
                    device_string(|b, n| self.0.GetDeviceFriendlyName(&hid, b, n)),
                    device_string(|b, n| self.0.GetDeviceDescription(&hid, b, n)),
                )
            };
            if is_apple(&manufacturer, &friendly, &description) {
                let name = [friendly, description]
                    .into_iter()
                    .find(|s| !s.is_empty())
                    .unwrap_or_else(|| "iPhone".into());
                out.push(DeviceInfo { id, name });
            }
        }
        Ok(out)
    }
}

fn is_apple(manufacturer: &str, friendly: &str, description: &str) -> bool {
    manufacturer.to_ascii_lowercase().contains("apple")
        || [friendly, description].iter().any(|s| {
            let s = s.to_ascii_lowercase();
            s.contains("iphone") || s.contains("ipad")
        })
}

/// Calls a WPD "fill this string buffer" function twice: once for the length,
/// once for the text.
unsafe fn device_string(f: impl Fn(PWSTR, *mut u32) -> windows::core::Result<()>) -> String {
    let mut len = 0u32;
    let _ = f(PWSTR::null(), &mut len);
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize];
    if f(PWSTR(buf.as_mut_ptr()), &mut len).is_err() {
        return String::new();
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_apple_devices() {
        assert!(is_apple("Apple Inc.", "Apple iPhone", ""));
        assert!(is_apple("", "My iPhone", ""));
        assert!(!is_apple("SAMSUNG", "Galaxy S24", "SM-S921"));
    }
}

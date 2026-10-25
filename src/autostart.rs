use std::io;

use anyhow::Result;
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "iPhonePhotoSync";

/// Adds or removes the per-user "run at login" entry. The entry starts the app
/// hidden in the tray. Re-running `set(true)` refreshes the path if the exe moved.
pub fn set(enabled: bool) -> Result<()> {
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)?;
    if enabled {
        let exe = std::env::current_exe()?;
        key.set_value(VALUE, &format!("\"{}\" --minimized", exe.display()))?;
    } else {
        match key.delete_value(VALUE) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            other => other?,
        }
    }
    Ok(())
}

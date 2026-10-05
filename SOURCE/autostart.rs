use std::env;
use std::fs;
use std::path::PathBuf;

use crate::{ create_shortcut_with_shell_link, resolve_shortcut_target};

const SHORTCUT_NAME: &str = "Flash Launch.lnk";

pub(crate) fn startup_folder() -> Result<PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_Startup};
    unsafe {
        let mut path = std::ptr::null_mut();
        let result = SHGetKnownFolderPath(&FOLDERID_Startup, 0, std::ptr::null_mut(), &mut path);
        if result < 0 || path.is_null() {
            return Err(format!("Could not resolve Startup folder: 0x{:08X}", result as u32));
        }
        let mut length = 0;
        while *path.add(length) != 0 { length += 1; }
        let folder = PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(path, length)));
        CoTaskMemFree(path as _);
        Ok(folder)
    }
}

pub(crate) fn shortcut_path() -> Result<PathBuf, String> {
    Ok(startup_folder()?.join(SHORTCUT_NAME))
}

pub(crate) fn is_enabled() -> bool {
    let Ok(shortcut) = shortcut_path() else { return false; };
    let Ok(current) = env::current_exe() else { return false; };
    let Some(target) = resolve_shortcut_target(&shortcut) else { return false; };
    normalize(&target) == normalize(&current)
}

pub(crate) fn set_enabled(enabled: bool) -> Result<(), String> {
    let shortcut = shortcut_path()?;
    if !enabled {
        if shortcut.exists() {
            fs::remove_file(&shortcut).map_err(|e| format!("Could not remove startup shortcut: {e}"))?;
        }
        return Ok(());
    }
    let target = env::current_exe().map_err(|e| format!("Could not resolve executable: {e}"))?;
    let folder = shortcut.parent().ok_or_else(|| "Invalid startup folder.".to_string())?;
    fs::create_dir_all(folder).map_err(|e| format!("Could not create Startup folder: {e}"))?;
    unsafe { create_shortcut_with_shell_link(&shortcut, &target)?; }
    if !is_enabled() { return Err("Startup shortcut target verification failed.".to_string()); }
    Ok(())
}

fn normalize(path: &std::path::Path) -> String {
    crate::fold_text(&fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy())
}

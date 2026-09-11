use std::env;
use std::fs;
use std::path::PathBuf;

use crate::{app_dir, create_shortcut_with_shell_link, resolve_shortcut_target};

const SHORTCUT_NAME: &str = "Flash Launch.lnk";

pub(crate) fn startup_folder() -> Result<PathBuf, String> {
    let appdata = env::var_os("APPDATA").ok_or_else(|| "APPDATA is unavailable.".to_string())?;
    Ok(PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs\\Startup"))
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

fn normalize(path: &std::path::Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

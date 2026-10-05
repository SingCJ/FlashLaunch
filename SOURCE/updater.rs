use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::{Mutex, atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering}};
use crate::*;

static STAGE: AtomicUsize = AtomicUsize::new(0);
static NOTIFY: AtomicBool = AtomicBool::new(false);
static BUSY: AtomicBool = AtomicBool::new(false);
static LOCK: AtomicIsize = AtomicIsize::new(0);
static PENDING: Mutex<Option<(AppLanguage, Release)>> = Mutex::new(None);
static RESULT: Mutex<Option<(AppLanguage, bool, Result<Event, String>)>> = Mutex::new(None);
enum Event { Checked(Option<Release>), Prepared(std::path::PathBuf) }
struct Release { version: String, url: String, digest: String }

// The launcher is normally hidden; use the visible Settings window as owner.
// Keep update dialogs above other windows and activate them when they appear.
unsafe fn update_message_box(
    hwnd: HWND,
    message: *const u16,
    title: *const u16,
    flags: u32,
) -> i32 {
    let settings = FindWindowW(wide(CONFIG_CLASS_NAME).as_ptr(), std::ptr::null());
    let owner = if !settings.is_null() && IsWindowVisible(settings) != 0 {
        settings
    } else if !hwnd.is_null() && IsWindowVisible(hwnd) != 0 {
        hwnd
    } else {
        std::ptr::null_mut()
    };
    MessageBoxW(owner, message, title, flags | MB_SETFOREGROUND | MB_TOPMOST)
}

fn update_error(hwnd: HWND, message: &str) {
    unsafe {
        update_message_box(hwnd, wide(message).as_ptr(), wide(APP_NAME).as_ptr(), MB_OK | MB_ICONERROR);
    }
}

pub(crate) fn busy() -> bool { BUSY.load(Ordering::Acquire) }
pub(crate) unsafe fn sync_ui(hwnd: HWND, language: AppLanguage, stage: usize) {
    STAGE.store(stage, Ordering::Release);
    EnableWindow(GetDlgItem(hwnd, ID_CFG_CHECK_UPDATE), if stage == 0 { TRUE } else { FALSE });
    set_window_text(GetDlgItem(hwnd, ID_CFG_CHECK_UPDATE), label(language));
}
pub(crate) fn label(language: AppLanguage) -> &'static str {
    match STAGE.load(Ordering::Acquire) {
        1 => localized(language, "Checking for updates..."),
        2 => localized(language, "Downloading update..."),
        _ => localized(language, "Check for updates"),
    }
}
unsafe fn progress(hwnd: HWND, language: AppLanguage, stage: usize) {
    STAGE.store(stage, Ordering::Release);
    set_window_text(update_button(hwnd), label(language));
    let settings = FindWindowW(wide(CONFIG_CLASS_NAME).as_ptr(), std::ptr::null());
    if !settings.is_null() { PostMessageW(settings, WM_UPDATE_UI_STATE, stage, 0); }
    let mut data: windows_sys::Win32::UI::Shell::NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of_val(&data) as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_UID;
    data.uFlags = windows_sys::Win32::UI::Shell::NIF_TIP;
    let tip = if stage == 0 { format!("{} {}", APP_NAME, APP_VERSION) } else { format!("{}: {}", APP_NAME, label(language)) };
    set_notify_icon_tip(&mut data, &tip);
    windows_sys::Win32::UI::Shell::Shell_NotifyIconW(windows_sys::Win32::UI::Shell::NIM_MODIFY, &data);
}
fn powershell() -> Command {
    let path = std::env::var_os("SystemRoot").map(std::path::PathBuf::from)
        .unwrap_or_else(|| "C:\\Windows".into()).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut command = Command::new(path);
    command.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass"])
        .creation_flags(CREATE_NO_WINDOW);
    command
}
fn run(script: &str, env: &[(&str, String)]) -> Result<String, String> {
    let output = powershell().args(["-Command", script]).envs(env.iter().cloned()).output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
fn version(value: &str) -> Option<[u64; 3]> {
    let values = value.trim().trim_start_matches('v').split('.').map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>().ok()?;
    values.try_into().ok()
}
fn check() -> Result<Event, String> {
    let text = run(include_str!("update_check.ps1"), &[("FLASH_ARCH", if cfg!(target_arch="x86") { "x86" } else { "x64" }.into())])?;
    let fields: Vec<_> = text.split('\t').collect();
    if fields.len() != 3 { return Err("Invalid release metadata.".into()); }
    let remote = version(fields[0]).ok_or("Invalid release version.")?;
    if remote <= version(APP_VERSION).ok_or("Invalid application version.")? { return Ok(Event::Checked(None)); }
    if !fields[1].starts_with("https://github.com/SingCJ/FlashLaunch/releases/download/") {
        return Err("No compatible release archive is available.".into());
    }
    Ok(Event::Checked(Some(Release { version: fields[0].into(), url: fields[1].into(), digest: fields[2].into() })))
}
unsafe fn update_button(hwnd: HWND) -> HWND {
    let settings = FindWindowW(wide(CONFIG_CLASS_NAME).as_ptr(), std::ptr::null());
    GetDlgItem(if settings.is_null() { hwnd } else { settings }, ID_CFG_CHECK_UPDATE)
}
fn finish(hwnd: HWND) {
    BUSY.store(false, Ordering::Release);
    unsafe {
        let lock = LOCK.swap(0, Ordering::AcqRel) as HANDLE;
        if !lock.is_null() { ReleaseSemaphore(lock, 1, std::ptr::null_mut()); CloseHandle(lock); }
        EnableWindow(update_button(hwnd), TRUE);
        let language = with_app(|app| app.language).unwrap_or_else(default_language);
        progress(hwnd, language, 0);
    }
}
fn worker(hwnd: HWND, language: AppLanguage, manual: bool, work: impl FnOnce() -> Result<Event, String> + Send + 'static) {
    let window = hwnd as isize;
    std::thread::spawn(move || {
        let result = work();
        *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some((language, manual, result));
        unsafe {
            if PostMessageW(window as HWND, WM_UPDATE_READY, 0, 0) == 0 { finish(window as HWND); }
        }
    });
}
pub(crate) unsafe fn start(hwnd: HWND, language: AppLanguage, manual: bool) {
    let settings_mode = with_app(|app| app.settings_process_mode).unwrap_or(false);
    if settings_mode {
        let main = FindWindowW(wide(CLASS_NAME).as_ptr(), std::ptr::null());
        if !main.is_null() {
            PostMessageW(main, WM_CHECK_UPDATE_REQUEST, language.combo_index(), 0);
        } else if let Ok(exe) = std::env::current_exe() {
            match Command::new(exe).arg("--hidden").creation_flags(CREATE_NO_WINDOW).spawn() {
                Ok(_) => { std::thread::spawn(move || {
                    for _ in 0..50 {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        let main = FindWindowW(wide(CLASS_NAME).as_ptr(), std::ptr::null());
                        if !main.is_null() { PostMessageW(main, WM_CHECK_UPDATE_REQUEST, language.combo_index(), 0); return; }
                    }
                }); }
                Err(error) => update_error(hwnd, &error.to_string()),
            }
        }
        return;
    }
    if BUSY.swap(true, Ordering::AcqRel) {
        if manual { NOTIFY.store(true, Ordering::Release); }
        if manual { update_message_box(hwnd, wide(localized(language, "An update check is already in progress.")).as_ptr(), wide(APP_NAME).as_ptr(), MB_OK | MB_ICONINFORMATION); }
        return;
    }
    NOTIFY.store(manual, Ordering::Release);
    KillTimer(hwnd, UPDATE_TIMER_ID);
    let lock = CreateSemaphoreW(std::ptr::null(), 1, 1, wide("Local\\FlashLaunchUpdate").as_ptr());
    if lock.is_null() || WaitForSingleObject(lock, 0) != WAIT_OBJECT_0 {
        if !lock.is_null() { CloseHandle(lock); }
        BUSY.store(false, Ordering::Release);
        if manual { update_error(hwnd, localized(language, "An update check is already in progress.")); }
        return;
    }
    LOCK.store(lock as isize, Ordering::Release);
    EnableWindow(update_button(hwnd), FALSE);
    progress(hwnd, language, 1);
    worker(hwnd, language, manual, check);
}
unsafe fn prepare(hwnd: HWND, language: AppLanguage, release: Release) {

            progress(hwnd, language, 2);
            worker(hwnd, language, true, move || {
                let stage = app_dir().join("TEMP").join(format!("update-{}-{}-{}", std::process::id(), release.version, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()));
                std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
                run(include_str!("update_prepare.ps1"), &[
                    ("FLASH_STAGE", stage.to_string_lossy().into_owned()),
                    ("FLASH_URL", release.url), ("FLASH_DIGEST", release.digest),
                    ("FLASH_ARCH", if cfg!(target_arch="x86") { "x86" } else { "x64" }.into())])?;
                std::fs::write(stage.join("install.ps1"), include_str!("update_install.ps1")).map_err(|e| e.to_string())?;
                Ok(Event::Prepared(stage))
            });
}
pub(crate) unsafe fn poll(hwnd: HWND) {
    let settings = FindWindowW(wide(CONFIG_CLASS_NAME).as_ptr(), std::ptr::null());
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    if pending.is_some() {
        if settings.is_null() {
            let (language, release) = pending.take().unwrap();
            drop(pending);
            KillTimer(hwnd, UPDATE_TIMER_ID);
            prepare(hwnd, language, release);
        } else if SendMessageW(settings, WM_UPDATE_SETTINGS_STATE, 0, 0) != 2 {
            pending.take();
            KillTimer(hwnd, UPDATE_TIMER_ID);
            finish(hwnd);
        }
    } else if settings.is_null() {
        drop(pending);
        KillTimer(hwnd, UPDATE_TIMER_ID);
        let language = with_app(|app| app.language).unwrap_or_else(default_language);
        start(hwnd, language, false);
    }
}
pub(crate) unsafe fn handle_ready(hwnd: HWND) {
    let result = RESULT.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some((language, manual, result)) = result else { return; };
    let manual = NOTIFY.swap(false, Ordering::AcqRel) || manual;
    match result {
        Err(error) => {
            finish(hwnd);
            if manual { update_error(hwnd, &format!("{}\n{}", localized(language, "Could not check or install updates. Please try again."), error)); }
        }
        Ok(Event::Checked(None)) => {
            finish(hwnd);
            if manual { update_message_box(hwnd, wide(localized(language, "You are using the latest version.")).as_ptr(), wide(APP_NAME).as_ptr(), MB_OK | MB_ICONINFORMATION); }
        }
        Ok(Event::Checked(Some(release))) => {
            // Never interrupt an open editor during the silent startup check.
            let settings = FindWindowW(wide(CONFIG_CLASS_NAME).as_ptr(), std::ptr::null());
            if !manual && !settings.is_null() {
                finish(hwnd);
                // Retry when the editor closes instead of overlapping its confirmation dialogs.
                SetTimer(hwnd, UPDATE_TIMER_ID, 1000, None);
                return;
            }
            let question = localized_format1(language,
                "Version {} is available. Download and install it? The app will close and restart. Your settings and data will be kept.", &release.version);
            if update_message_box(hwnd, wide(&question).as_ptr(), wide(APP_NAME).as_ptr(), MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_SETFOREGROUND) != IDYES {
                finish(hwnd); return;
            }
            // The existing close workflow handles Save / Discard / Stay and pending saves.
            if !settings.is_null() {
                SendMessageW(settings, WM_CLOSE, 0, 0);
                if IsWindow(settings) != 0 {
                    if SendMessageW(settings, WM_UPDATE_SETTINGS_STATE, 0, 0) == 2 {
                        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((language, release));
                        SetTimer(hwnd, UPDATE_TIMER_ID, 200, None);
                        return;
                    }
                    finish(hwnd);
                    update_message_box(hwnd, wide(localized(language, "Finish saving or close Settings, then check for updates again.")).as_ptr(), wide(APP_NAME).as_ptr(), MB_OK | MB_ICONINFORMATION);
                    return;
                }
            }
            prepare(hwnd, language, release);
        }
        Ok(Event::Prepared(stage)) => {
            // Recheck the editor after the download, since a user may have opened it meanwhile.
            let settings = FindWindowW(wide(CONFIG_CLASS_NAME).as_ptr(), std::ptr::null());
            if !settings.is_null() {
                finish(hwnd);
                update_message_box(hwnd, wide(localized(language, "Finish saving or close Settings, then check for updates again.")).as_ptr(), wide(APP_NAME).as_ptr(), MB_OK | MB_ICONINFORMATION);
                return;
            }
            let executable = match std::env::current_exe() { Ok(p) => p, Err(e) => { finish(hwnd); update_error(hwnd, &e.to_string()); return; } };
            let launched = powershell().arg("-File").arg(stage.join("install.ps1"))
                .env("FLASH_STAGE", &stage).env("FLASH_ROOT", app_dir())
                .env("FLASH_EXE", executable).env("FLASH_PID", std::process::id().to_string())
                .env("FLASH_FAILURE", localized(language, "Could not check or install updates. Please try again."))
                .spawn();
            finish(hwnd);
            match launched {
                Ok(_) => { with_app(|app| execute_tray_menu_command(app, ID_TRAY_QUIT)); }
                Err(e) => update_error(hwnd, &e.to_string()),
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn stable_versions_compare_numerically() {
        assert!(version("v1.10.0") > version("1.9.9"));
        assert_eq!(version("1.0.0"), Some([1,0,0]));
        for invalid in ["1.0", "1.0.0-beta", "1.0.0.1", "garbage"] { assert_eq!(version(invalid), None); }
    }
}

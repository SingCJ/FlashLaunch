use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Threading::{GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::*;
use crate::app_impl::FLASH_LAUNCH_SHUTDOWN_REQUESTED;

const NIF_SHOWTIP_LOCAL: u32 = 0x0000_0080;

unsafe extern "system" fn destroy_other_thread_windows(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let main_hwnd = lparam as HWND;
    if hwnd != main_hwnd {
        DestroyWindow(hwnd);
    }
    TRUE
}

unsafe fn force_shutdown_settings_process() {
    let settings_hwnd = FindWindowW(wide(CONFIG_CLASS_NAME).as_ptr(), null());
    if !settings_hwnd.is_null() && is_flash_launch_window(settings_hwnd) {
        SendMessageW(settings_hwnd, WM_FORCE_SHUTDOWN, 0, 0);
    }
}

unsafe fn is_flash_launch_window(hwnd: HWND) -> bool {
    let mut process_id = 0u32;
    GetWindowThreadProcessId(hwnd, &mut process_id);
    if process_id == 0 {
        return false;
    }
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, process_id);
    if process.is_null() {
        return false;
    }
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    let matches = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) != 0
        && std::env::current_exe()
            .ok()
            .map(|current| {
                let other = PathBuf::from(OsString::from_wide(&buffer[..length as usize]));
                current
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&other.to_string_lossy())
            })
            .unwrap_or(false);
    CloseHandle(process);
    matches
}

unsafe fn destroy_other_app_windows(main_hwnd: HWND) {
    EnumThreadWindows(
        GetCurrentThreadId(),
        Some(destroy_other_thread_windows),
        main_hwnd as LPARAM,
    );
}

pub(crate) unsafe fn add_tray_icon(app: &mut AppState) {
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = app.hwnd;
    data.uID = TRAY_UID;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP_LOCAL;
    data.uCallbackMessage = WM_TRAYICON;
    data.hIcon = if app.app_icon_small.is_null() {
        LoadIconW(null_mut(), IDI_APPLICATION)
    } else {
        app.app_icon_small
    };
    set_notify_icon_tip(&mut data, &format!("{} {}", APP_NAME, APP_VERSION));
    if Shell_NotifyIconW(NIM_ADD, &data) != 0 {
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &data);
        data.uFlags = NIF_TIP | NIF_SHOWTIP_LOCAL;
        Shell_NotifyIconW(NIM_MODIFY, &data);
        app.tray_added = true;
    }
}

pub(crate) unsafe fn remove_tray_icon(app: &mut AppState) {
    if !app.tray_added {
        return;
    }
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = app.hwnd;
    data.uID = TRAY_UID;
    Shell_NotifyIconW(NIM_DELETE, &data);
    app.tray_added = false;
}

pub(crate) unsafe fn track_tray_menu(hwnd: HWND, language: AppLanguage) -> usize {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return 0;
    }

    AppendMenuW(
        menu,
        MF_STRING,
        ID_TRAY_SHOW_HIDE,
        wide(localized(language, "Show / Hide")).as_ptr(),
    );
    AppendMenuW(
        menu,
        MF_STRING,
        ID_TRAY_CONFIG,
        wide(localized(language, "Settings")).as_ptr(),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(
        menu,
        MF_STRING,
        ID_TRAY_QUIT,
        wide(localized(language, "Quit")).as_ptr(),
    );

    let mut point: POINT = std::mem::zeroed();
    GetCursorPos(&mut point);
    SetForegroundWindow(hwnd);
    let command = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        point.x,
        point.y,
        0,
        hwnd,
        null(),
    );
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
    command as usize
}

pub(crate) unsafe fn execute_tray_menu_command(app: &mut AppState, command: usize) {
    match command {
        ID_TRAY_SHOW_HIDE => app.toggle_launcher(),
        ID_TRAY_CONFIG => app.show_config_window(),
        ID_TRAY_QUIT => {
            FLASH_LAUNCH_SHUTDOWN_REQUESTED.store(true, Ordering::Release);
            force_shutdown_settings_process();
            remove_tray_icon(app);
            destroy_other_app_windows(app.hwnd);
            DestroyWindow(app.hwnd);
        }
        _ => {}
    }
}

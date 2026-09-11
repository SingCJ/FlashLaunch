#![windows_subsystem = "windows"]

use std::ptr::null;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub(crate) mod app_impl;
pub(crate) mod background_tasks;
pub(crate) mod config_io;
pub(crate) mod config_labels;
pub(crate) mod constants;
#[cfg(feature = "debug-tools")]
pub(crate) mod debug_tools;
pub(crate) mod filesystem_scan;
pub(crate) mod helper_process;
pub(crate) mod i18n;
pub(crate) mod icon_helper;
pub(crate) mod plugin_worker;
mod plugins;
pub(crate) mod result_store;
pub(crate) mod root_ownership;
pub(crate) mod score_window;
pub(crate) mod scoring;
pub(crate) mod search;
pub(crate) mod search_worker;
pub(crate) mod settings_model;
pub(crate) mod shell;
pub(crate) mod shell_context_menu;
pub(crate) mod shell_helper;
pub(crate) mod startup_options;
pub(crate) mod autostart;
pub(crate) mod state;
pub(crate) mod tray;
pub(crate) mod types;
pub(crate) mod ui_helpers;
pub(crate) mod util;
pub(crate) mod winapi_compat;
pub(crate) mod wndproc;

// Re-export modules so modules can use crate::*
pub(crate) use background_tasks::*;
pub(crate) use config_io::*;
pub(crate) use config_labels::*;
pub(crate) use constants::*;
#[cfg(feature = "debug-tools")]
use debug_tools::{run_bench_search, run_debug_search};
pub(crate) use filesystem_scan::*;
pub(crate) use i18n::*;
pub(crate) use result_store::*;
pub(crate) use root_ownership::*;
pub(crate) use score_window::*;
pub(crate) use scoring::*;
pub(crate) use search::*;
pub(crate) use search_worker::*;
pub(crate) use shell::*;
pub(crate) use shell_context_menu::*;
pub(crate) use startup_options::*;
pub(crate) use state::*;
pub(crate) use tray::*;
pub(crate) use types::*;
pub(crate) use ui_helpers::*;
pub(crate) use util::*;
pub(crate) use winapi_compat::*;
pub(crate) use wndproc::*;

#[cfg(feature = "debug-tools")]
fn log_debug_search_error(error: &std::io::Error) {
    let message = format!(
        "{} debug search output error: {}\n",
        local_timestamp_text(),
        error
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(app_dir().join("debug-search-error.log"))
        .and_then(|mut file| {
            use std::io::Write as _;
            file.write_all(message.as_bytes())
        });
}

fn local_timestamp_text() -> String {
    unsafe {
        let mut time: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut time);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
        )
    }
}

fn format_crash_log_line(timestamp: &str, event: &str) -> String {
    format!(
        "{} | {} {} ({}) | {}\n",
        timestamp, APP_NAME, APP_VERSION, APP_BUILD_TIME, event
    )
}

pub(crate) fn log_crash_event(event: &str) {
    let log = format_crash_log_line(&local_timestamp_text(), event);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(app_dir().join("crash.log"))
    {
        use std::io::Write;
        let _ = file.write_all(log.as_bytes());
    }
}

pub(crate) fn log_helper_error(helper: &str, error: &str) {
    log_crash_event(&format!("{helper} helper failed: {error}"));
}

pub(crate) fn log_helper_fatal_error(helper: &str, stage: &str, error: &dyn std::fmt::Display) {
    log_crash_event(&format!(
        "{helper} helper fatal error while {stage}: {error}"
    ));
}

struct SingleInstanceGuard(HANDLE);

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

unsafe fn create_named_instance_guard(name: &str) -> Option<SingleInstanceGuard> {
    let mutex = CreateMutexW(null_mut(), TRUE, wide(name).as_ptr());
    if mutex.is_null() {
        return None;
    }
    if GetLastError() == ERROR_ALREADY_EXISTS {
        CloseHandle(mutex);
        return None;
    }
    Some(SingleInstanceGuard(mutex))
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = match info.payload().downcast_ref::<&'static str>() {
            Some(s) => *s,
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => &s[..],
                None => "Box<dyn Any>",
            },
        };
        let loc = if let Some(l) = info.location() {
            format!("{}:{}", l.file(), l.line())
        } else {
            "unknown".to_string()
        };
        log_crash_event(&format!("Panic at {loc}: {msg}"));
    }));
}

fn run_settings_process(startup_options: &StartupOptions) -> bool {
    if !startup_options.open_settings {
        return false;
    }

    unsafe {
        let class_name = wide(CONFIG_CLASS_NAME);
        let instance_guard = create_named_instance_guard(SETTINGS_INSTANCE_MUTEX_NAME);
        let existing = FindWindowW(class_name.as_ptr(), null());
        if instance_guard.is_none() || !existing.is_null() {
            if !existing.is_null() {
                let page = startup_options
                    .settings_page
                    .map(config_nav_index_for_page)
                    .unwrap_or(usize::MAX);
                PostMessageW(existing, WM_SETTINGS_SHOW_PAGE, page, 0);
                ShowWindow(existing, SW_RESTORE);
                ShowWindow(existing, SW_SHOW);
                SetForegroundWindow(existing);
            }
            return true;
        }
        let _instance_guard = instance_guard.unwrap();
        let instance = GetModuleHandleW(null());
        let settings = load_app_settings_snapshot();
        let language = settings.language;
        set_active_language(language);
        with_app_tls(|cell| {
            let mut app = AppState::new_with_settings(Vec::new(), settings);
            app.settings_process_mode = true;
            app.instance = instance;
            app.app_icon = load_app_icon(32);
            app.app_icon_small = load_app_icon(16);
            *cell.borrow_mut() = Some(app);
        });

        let cursor = LoadCursorW(null_mut(), IDC_ARROW);
        let app_icon = with_app(|app| app.app_icon).unwrap_or(null_mut());
        let app_icon_small = with_app(|app| app.app_icon_small).unwrap_or(null_mut());
        let config_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(config_window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: app_icon,
            hCursor: cursor,
            hbrBackground: (COLOR_WINDOW + 1) as isize as _,
            lpszMenuName: null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: app_icon_small,
        };
        let add_class_name = wide(ADD_INDEX_CLASS_NAME);
        let add_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(add_index_window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: app_icon,
            hCursor: cursor,
            hbrBackground: (COLOR_WINDOW + 1) as isize as _,
            lpszMenuName: null(),
            lpszClassName: add_class_name.as_ptr(),
            hIconSm: app_icon_small,
        };
        if RegisterClassExW(&config_class) == 0 || RegisterClassExW(&add_class) == 0 {
            show_error(
                null_mut(),
                localized(language, "Could not register the Settings window class."),
            );
            return true;
        }

        with_app(|app| {
            if let Some(page) = startup_options.settings_page {
                app.show_config_window_for_page(page);
            } else {
                app.show_config_window();
            }
        });

        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            if handle_key_message(&mut message) {
                continue;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        with_app_tls(|cell| {
            if let Some(app) = cell.borrow_mut().as_mut() {
                app.request_background_shutdown();
                app.join_background_workers();
            }
            *cell.borrow_mut() = None;
        });
    }
    true
}

fn main() {
    install_panic_hook();

    if let Some(exit_code) = icon_helper::run_icon_helper_if_requested(std::env::args_os().skip(1))
    {
        std::process::exit(exit_code);
    }
    if let Some(exit_code) =
        shell_helper::run_shell_helper_if_requested(std::env::args_os().skip(1))
    {
        std::process::exit(exit_code);
    }

    cleanup_stale_result_store_files();

    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let common_controls = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_UPDOWN_CLASS | ICC_BAR_CLASSES,
        };
        InitCommonControlsEx(&common_controls);
    }
    let _ole_guard = unsafe { OleGuard::initialize() };

    let startup_options = parse_startup_options(std::env::args().skip(1));
    if run_settings_process(&startup_options) {
        return;
    }
    #[cfg(feature = "debug-tools")]
    {
        if let Some(query) = startup_options.debug_search.as_deref() {
            if let Err(error) = run_debug_search(
                query,
                startup_options.debug_limit,
                startup_options.debug_output.as_deref(),
            ) {
                log_debug_search_error(&error);
            }
            return;
        }
        if let Some(query) = startup_options.bench_search.as_deref() {
            if let Err(error) = run_bench_search(
                query,
                startup_options.bench_threads,
                startup_options.bench_runs,
                startup_options.debug_output.as_deref(),
            ) {
                log_debug_search_error(&error);
            }
            return;
        }
    }

    unsafe {
        let instance = GetModuleHandleW(null());
        let class_name = wide(CLASS_NAME);
        let instance_guard = create_named_instance_guard(INSTANCE_MUTEX_NAME);
        let existing = FindWindowW(class_name.as_ptr(), null());
        if instance_guard.is_none() || !existing.is_null() {
            if !existing.is_null() {
                if !startup_options.hide_existing {
                    ShowWindow(existing, SW_RESTORE);
                    ShowWindow(existing, SW_SHOW);
                    SetForegroundWindow(existing);
                }
            }
            return;
        }
        let _instance_guard = instance_guard.unwrap();
        let startup_settings = load_app_settings_snapshot();
        let window_settings = startup_settings.window;
        let startup_language = startup_settings.language;
        set_active_language(startup_language);
        let startup_show_cpu = startup_settings.show_cpu_in_title;
        let startup_show_ram = startup_settings.show_ram_in_title;
        let startup_show_build_timestamp = startup_settings.show_build_timestamp_in_title;
        with_app_tls(|cell| {
            *cell.borrow_mut() = Some(AppState::new_with_settings(Vec::new(), startup_settings));
        });

        let cursor = LoadCursorW(null_mut(), IDC_ARROW);
        let app_icon = load_app_icon(32);
        let app_icon_small = load_app_icon(16);
        let wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: app_icon,
            hCursor: cursor,
            hbrBackground: (COLOR_WINDOW + 1) as isize as _,
            lpszMenuName: null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: app_icon_small,
        };
        let owner_class_name = wide(OWNER_CLASS_NAME);
        let owner_wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(DefWindowProcW),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: null_mut(),
            hCursor: cursor,
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: owner_class_name.as_ptr(),
            hIconSm: null_mut(),
        };
        let config_class_name = wide(CONFIG_CLASS_NAME);
        let config_wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(config_window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: app_icon,
            hCursor: cursor,
            hbrBackground: (COLOR_WINDOW + 1) as isize as _,
            lpszMenuName: null(),
            lpszClassName: config_class_name.as_ptr(),
            hIconSm: app_icon_small,
        };
        let add_index_class_name = wide(ADD_INDEX_CLASS_NAME);
        let add_index_wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(add_index_window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: app_icon,
            hCursor: cursor,
            hbrBackground: (COLOR_WINDOW + 1) as isize as _,
            lpszMenuName: null(),
            lpszClassName: add_index_class_name.as_ptr(),
            hIconSm: app_icon_small,
        };
        let score_breakdown_class_name = wide(SCORE_BREAKDOWN_CLASS_NAME);
        let score_breakdown_wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(score_breakdown_window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: app_icon,
            hCursor: cursor,
            hbrBackground: (COLOR_WINDOW + 1) as isize as _,
            lpszMenuName: null(),
            lpszClassName: score_breakdown_class_name.as_ptr(),
            hIconSm: app_icon_small,
        };
        let tooltip_class_name = wide(TOOLTIP_CLASS_NAME);
        let tooltip_wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(tooltip_window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: null_mut(),
            hCursor: cursor,
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: tooltip_class_name.as_ptr(),
            hIconSm: null_mut(),
        };
        if RegisterClassExW(&wnd_class) == 0 {
            show_error(
                null_mut(),
                localized(startup_language, "Could not register window class."),
            );
            return;
        }
        if RegisterClassExW(&owner_wnd_class) == 0 {
            show_error(
                null_mut(),
                localized(startup_language, "Could not register owner window class."),
            );
            return;
        }
        if RegisterClassExW(&config_wnd_class) == 0 {
            show_error(
                null_mut(),
                localized(
                    startup_language,
                    "Could not register settings window class.",
                ),
            );
            return;
        }
        if RegisterClassExW(&add_index_wnd_class) == 0 {
            show_error(
                null_mut(),
                localized(
                    startup_language,
                    "Could not register Add Search Folder window class.",
                ),
            );
            return;
        }
        if RegisterClassExW(&score_breakdown_wnd_class) == 0 {
            show_error(
                null_mut(),
                localized(
                    startup_language,
                    "Could not register score breakdown window class.",
                ),
            );
            return;
        }
        if RegisterClassExW(&tooltip_wnd_class) == 0 {
            show_error(
                null_mut(),
                localized(startup_language, "Could not register tooltip window class."),
            );
            return;
        }
        let title = wide(&window_title_text(
            startup_language,
            startup_show_cpu,
            startup_show_ram,
            startup_show_build_timestamp,
        ));
        let owner_hwnd = CreateWindowExW(
            0,
            owner_class_name.as_ptr(),
            wide(&format!("{} Owner", APP_NAME)).as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if owner_hwnd.is_null() {
            show_error(
                null_mut(),
                localized(startup_language, "Could not create owner window."),
            );
            return;
        }
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            window_style_no_minimize(),
            window_settings.x,
            window_settings.y,
            window_settings.width,
            window_settings.height,
            owner_hwnd,
            null_mut(),
            instance,
            null(),
        );

        if hwnd.is_null() {
            DestroyWindow(owner_hwnd);
            show_error(
                null_mut(),
                localized(startup_language, "Could not create main window."),
            );
            return;
        }

        with_app(|app| {
            app.hwnd = hwnd;
            app.plugin_supervisor.set_window(hwnd as isize);
            app.owner_hwnd = owner_hwnd;
            app.instance = instance;
            app.app_icon = app_icon;
            app.app_icon_small = app_icon_small;
            app.taskbar_created_message = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
            SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, app_icon as isize);
            SendMessageW(
                hwnd,
                WM_SETICON,
                ICON_SMALL as usize,
                app_icon_small as isize,
            );
            app.create_controls(instance);
            add_tray_icon(app);
            set_window_text(
                hwnd,
                &window_title_text(
                    app.language,
                    app.show_cpu_in_title,
                    app.show_ram_in_title,
                    app.show_build_timestamp_in_title,
                ),
            );
            app.schedule_search_configuration_reload(true);
        });

        with_app(|app| {
            if !app.apply_hotkey() {
                show_error(
                    hwnd,
                    localized(
                        startup_language,
                        "Could not register popup hotkey. Change it in Settings.",
                    ),
                );
            }
        });
        if startup_options.show_main {
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            PostMessageW(hwnd, WM_REFRESH_RESULTS, 0, 0);
            with_app(|app| {
                SetFocus(app.edit);
                app.begin_helper_focus_session();
            });
        }
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            if handle_key_message(&mut message) {
                continue;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        with_app(|app| {
            app.request_background_shutdown();
            app.join_background_workers();
        });
        with_app_tls(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn crash_log_line_includes_application_metadata_and_event() {
        let line =
            format_crash_log_line("2026-08-01 12:34:56", "Icon helper failed: Test failure.");
        assert_eq!(
            line,
            format!(
                "2026-08-01 12:34:56 | {} {} ({}) | Icon helper failed: Test failure.\n",
                APP_NAME, APP_VERSION, APP_BUILD_TIME
            )
        );
    }

    #[test]
    fn folds_vietnamese_accents() {
        assert_eq!(
            fold_text("Ti\u{1ebf}ng Vi\u{1ec7}t \u{0110}\u{1ea7}y \u{0110}\u{1ee7}"),
            "tieng viet day du"
        );
        assert_eq!(fold_text("\u{0102}n c\u{01a1}m"), "an com");
        assert!(score_text(
            &searchable_text("tieng viet"),
            &searchable_text("Ti\u{1ebf}ng Vi\u{1ec7}t"),
            true
        )
        .is_some());
        assert!(score_text(
            &searchable_text("An com"),
            &searchable_text("\u{0102}n c\u{01a1}m"),
            true
        )
        .is_some());
    }

    #[test]
    fn collects_vietnamese_query_without_crash() {
        let title = "\u{0102}n c\u{01a1}m";
        let items = vec![LaunchItem::new(
            title.to_string(),
            "Test item".to_string(),
            PathBuf::from("C:\\Temp\\an-com.txt"),
            false,
            100,
            None,
        )];
        let plugin_state = plugins::PluginState::default();
        let results = collect_results("\u{0103}n", &items, &[], &plugin_state);
        assert!(results.iter().any(|result| result.title == title));
    }

    #[test]
    fn search_box_only_returns_indexed_items() {
        for query in ["config", "g m\u{00e8}o", "C:\\Windows", "calc"] {
            let plugin_state = plugins::PluginState::default();
            assert!(collect_results(query, &[], &[], &plugin_state).is_empty());
        }
    }

    #[test]
    fn search_matches_name_not_parent_path() {
        let item = LaunchItem::new(
            "WinMerge".to_string(),
            "C:\\Users\\TestUser\\Desktop\\FILE\\WinMerge.lnk".to_string(),
            PathBuf::from("C:\\Users\\TestUser\\Desktop\\FILE\\WinMerge.lnk"),
            false,
            100,
            None,
        );
        let plugin_state = plugins::PluginState::default();
        assert!(
            !collect_results("WinMerge", std::slice::from_ref(&item), &[], &plugin_state)
                .is_empty()
        );
        let plugin_state = plugins::PluginState::default();
        assert!(collect_results("Desktop FILE", &[item], &[], &plugin_state).is_empty());
    }

    #[test]
    fn search_matches_independent_fuzzy_tokens() {
        let item = LaunchItem::new(
            "FirefoxPortable2ndProfile-private clean".to_string(),
            "Desktop".to_string(),
            PathBuf::from(
                "C:\\Users\\TestUser\\Desktop\\FirefoxPortable2ndProfile-private clean.lnk",
            ),
            false,
            100,
            None,
        );
        let plugin_state = plugins::PluginState::default();
        assert!(!collect_results(
            "pcl firefox",
            std::slice::from_ref(&item),
            &[],
            &plugin_state
        )
        .is_empty());
        assert!(
            !collect_results("pcl fi", std::slice::from_ref(&item), &[], &plugin_state).is_empty()
        );
        assert!(!collect_results("fi pcl", &[item], &[], &plugin_state).is_empty());
    }

    #[test]
    fn file_title_keeps_extension() {
        let root = IndexRoot {
            raw: "C:\\Apps".to_string(),
            path: Some(PathBuf::from("C:\\Apps")),
            enabled: true,
            score: 100,
            max_depth: DEFAULT_SEARCH_DEPTH,
            label: String::new(),
            keywords: Vec::new(),
        };
        let mut seen = HashSet::new();
        let mut items = Vec::new();

        push_item(
            PathBuf::from("C:\\Apps\\main.rs"),
            &root,
            &mut seen,
            &mut items,
        );

        assert_eq!(
            items.first().map(|item| item.title.as_str()),
            Some("main.rs")
        );
    }

    #[test]
    fn exact_match_uses_filename_without_extension() {
        let item = LaunchItem::new(
            "opencode.lnk".to_string(),
            "Desktop".to_string(),
            PathBuf::from("C:\\Desktop\\opencode.lnk"),
            false,
            0,
            None,
        );

        assert!(
            score_item_text_with_config("opencode", &item, &ScoringConfig::default()) > Some(0)
        );
    }

    #[test]
    fn fuzzy_filename_match_crosses_extension_boundary_naturally() {
        let item = LaunchItem::new(
            "report.pdf".to_string(),
            "Documents".to_string(),
            PathBuf::from("C:\\Documents\\report.pdf"),
            false,
            0,
            None,
        );
        let scoring = ScoringConfig::default();

        assert!(score_item_text_with_config("rptf", &item, &scoring).is_some());
        assert!(score_item_text_with_config("rpt pdf", &item, &scoring).is_some());
        assert!(score_item_text_with_config("pdf", &item, &scoring).is_some());
        assert!(score_item_text_with_config("rpt", &item, &scoring).is_some());
    }

    #[test]
    fn path_adjustment_uses_clear_parent_path_matches_only() {
        let scoring = ScoringConfig::default();
        let folder = LaunchItem::new(
            "OpenCode".to_string(),
            "Exports".to_string(),
            PathBuf::from("C:\\Exports\\OpenCode"),
            true,
            0,
            None,
        );
        let nested = LaunchItem::new(
            "App.exe".to_string(),
            "C:\\Tools\\App".to_string(),
            PathBuf::from("C:\\Tools\\App\\App.exe"),
            false,
            0,
            None,
        );

        let base_folder = score_text("opencode", &fold_text("OpenCode"), false).unwrap();
        assert_eq!(
            score_item_text_with_config("opencode", &folder, &scoring),
            Some(base_folder)
        );
        let nested_base =
            score_text("app", &fold_text("App.exe"), false).unwrap() + scoring.exact_match_bonus;
        assert_eq!(
            score_item_text_with_config("app", &nested, &scoring),
            Some(nested_base)
        );
        assert_eq!(
            score_item_path_with_config("app", &nested, &scoring),
            scoring.explicit_folder_name_match_adjustment
        );

        let cloud_shortcut = LaunchItem::new(
            "KopiaUI.exe - Shortcut.lnk".to_string(),
            "C:\\Users\\User\\Desktop\\CLOUD".to_string(),
            PathBuf::from("C:\\Users\\User\\Desktop\\CLOUD\\KopiaUI.exe - Shortcut.lnk"),
            false,
            0,
            None,
        );
        assert_eq!(
            score_item_path_with_config("pcl", &cloud_shortcut, &scoring),
            0
        );
        assert_eq!(
            score_item_path_with_config("cloud", &cloud_shortcut, &scoring),
            scoring.explicit_folder_name_match_adjustment
        );
        assert_eq!(
            score_item_path_with_config("desktop cloud", &cloud_shortcut, &scoring),
            scoring.explicit_folder_name_match_adjustment
        );
    }

    #[test]
    fn recent_items_rank_first() {
        let first = PathBuf::from("C:\\Apps\\Old.exe");
        let second = PathBuf::from("C:\\Apps\\Recent.exe");
        let recent_items = recent_lookup(&[format!("100>>>{}", second.display())]);
        assert!(
            recent_item_score(&second, &recent_items) > recent_item_score(&first, &recent_items)
        );
    }

    #[test]
    fn recent_path_match_uses_index_root_score_when_available() {
        let root_path =
            env::temp_dir().join(format!("flash-launch-test-root-{}", std::process::id()));
        let file_path = root_path.join("FirefoxPortable2ndProfile-private clean.lnk");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(&file_path, b"shortcut").unwrap();

        let root = IndexRoot {
            raw: root_path.to_string_lossy().to_string(),
            path: Some(root_path.clone()),
            enabled: true,
            score: 250,
            max_depth: SEARCH_DEPTH_ALL,
            label: String::new(),
            keywords: Vec::new(),
        };
        let recent_items = vec![recent_entry_line(110.0, &file_path.to_string_lossy())];
        let recent_map = recent_lookup(&recent_items);
        let scoring = ScoringConfig::default();
        let spec = parse_search_query("pcl").effective_for_scoring(&scoring);
        let root_plan = RootOwnershipPlan::build(&[root]);

        let breakdowns = collect_recent_path_match_breakdowns(
            &spec,
            &root_plan,
            &recent_items,
            &recent_map,
            &scoring,
            10,
        );

        fs::remove_dir_all(&root_path).unwrap();
        assert_eq!(breakdowns.first().map(|item| item.index_score), Some(250));
    }

    #[test]
    fn recent_path_uses_deepest_root_and_first_exact_duplicate() {
        let project = env::current_dir().unwrap();
        let source = project.join("SOURCE");
        let root = |raw: &str, path: PathBuf, score: i32| IndexRoot {
            raw: raw.to_string(),
            path: Some(path),
            enabled: true,
            score,
            max_depth: SEARCH_DEPTH_ALL,
            label: String::new(),
            keywords: Vec::new(),
        };
        let roots = vec![
            root("parent-top", project.clone(), 10),
            root("parent-duplicate", project, 20),
            root("child", source.clone(), 75),
        ];
        let root_plan = RootOwnershipPlan::build(&roots);

        let child_owner = recent_path_root(&source.join("main.rs"), &root_plan);
        assert_eq!(child_owner.raw, "child");
        assert_eq!(child_owner.score, 75);

        let parent_owner =
            recent_path_root(&source.parent().unwrap().join("Cargo.toml"), &root_plan);
        assert_eq!(parent_owner.raw, "parent-top");
        assert_eq!(parent_owner.score, 10);
    }

    #[test]
    fn empty_query_returns_recent_launch_order_only() {
        let old = LaunchItem::new(
            "Old".to_string(),
            "High score folder".to_string(),
            PathBuf::from("C:\\High\\Old.exe"),
            false,
            10_000,
            None,
        );
        let newest = LaunchItem::new(
            "Newest".to_string(),
            "Low score folder".to_string(),
            PathBuf::from("C:\\Low\\Newest.lnk"),
            false,
            1,
            None,
        );
        let plugin_state = plugins::PluginState::default();
        let recent_items = vec![
            recent_entry_line(100.0, &recent_item_key(&newest.path)),
            recent_entry_line(110.0, &recent_item_key(&old.path)),
        ];
        let results = collect_results("", &[old, newest], &recent_items, &plugin_state);
        assert_eq!(
            results.first().map(|result| result.title.as_str()),
            Some("Newest")
        );
        assert_eq!(results.first().map(|result| result.score), Some(0));
        assert_eq!(results.first().map(|result| result.display_score), Some(0));
    }

    #[test]
    fn equal_score_results_keep_search_folder_order() {
        let mut results = vec![
            SearchResult {
                title: "Foo.exe".to_string(),
                subtitle: "FirstRoot".to_string(),
                target: LaunchTarget::Path(PathBuf::from("C:\\First\\Foo.exe")),
                is_dir: false,
                from_history: false,
                from_query_launch_rule: false,
                ranking_kind: ResultRankingKind::Heuristic,
                explanation: None,
                display_score: 100,
                score_detail: String::new(),
                score: 100,
            },
            SearchResult {
                title: "Foo.exe".to_string(),
                subtitle: "SecondRoot".to_string(),
                target: LaunchTarget::Path(PathBuf::from("D:\\Second\\Foo.exe")),
                is_dir: false,
                from_history: false,
                from_query_launch_rule: false,
                ranking_kind: ResultRankingKind::Heuristic,
                explanation: None,
                display_score: 100,
                score_detail: String::new(),
                score: 100,
            },
        ];

        sort_search_results(&mut results);

        assert_eq!(results[0].subtitle, "FirstRoot");
        assert_eq!(results[1].subtitle, "SecondRoot");
    }

    #[test]
    fn recent_can_beat_small_folder_gap() {
        let recent = LaunchItem::new(
            "Code".to_string(),
            "C:\\Apps".to_string(),
            PathBuf::from("C:\\Apps\\Code.exe"),
            false,
            100,
            None,
        );
        let not_recent = LaunchItem::new(
            "Code".to_string(),
            "C:\\Tools".to_string(),
            PathBuf::from("C:\\Tools\\Code.lnk"),
            false,
            120,
            None,
        );
        let recent_items = recent_lookup(&[recent_item_key(&recent.path)]);
        assert!(
            ranked_score("code", &[], &recent, &recent_items)
                > ranked_score("code", &[], &not_recent, &recent_items)
        );
    }

    #[test]
    fn history_bonus_does_not_beat_stronger_exact_match() {
        let launched = LaunchItem::new(
            "OpenCode".to_string(),
            "Desktop".to_string(),
            PathBuf::from("C:\\Users\\TestUser\\Desktop\\PROGRAMMING\\OpenCode.lnk"),
            false,
            100,
            None,
        );
        let stronger_match = LaunchItem::new(
            "open".to_string(),
            "Tools".to_string(),
            PathBuf::from("C:\\Tools\\open.exe"),
            false,
            400,
            None,
        );
        let recent_items =
            recent_lookup(&[recent_entry_line(100.0, &recent_item_key(&launched.path))]);
        assert!(
            ranked_score("open", &[], &stronger_match, &recent_items)
                > ranked_score("open", &[], &launched, &recent_items)
        );
    }

    #[test]
    fn launcher_scoring_prefers_strong_name_match_over_recent_embedded_substring() {
        let notepad = LaunchItem::new(
            "Notepad++".to_string(),
            "Desktop\\TEXT".to_string(),
            PathBuf::from("C:\\Users\\TestUser\\Desktop\\TEXT\\Notepad++.lnk"),
            false,
            50,
            None,
        );
        let debt_note = LaunchItem::new(
            "DebtNote_CLI-KH25-0222_CLI260400049.xlsx".to_string(),
            "Recent".to_string(),
            PathBuf::from("C:\\Users\\TestUser\\AppData\\Roaming\\Microsoft\\Windows\\Recent\\DebtNote_CLI-KH25-0222_CLI260400049.xlsx.lnk"),
            false,
            50,
            None,
        );
        let plugin_state = plugins::PluginState::default();
        let recent_items = vec![
            recent_entry_line(102.0, &recent_item_key(&notepad.path)),
            recent_entry_line(100.0, &recent_item_key(&debt_note.path)),
        ];
        let results = collect_results("note", &[debt_note, notepad], &recent_items, &plugin_state);
        assert_eq!(
            results.first().map(|result| result.title.as_str()),
            Some("Notepad++")
        );
    }

    #[test]
    fn substring_match_beats_history_fuzzy_like_matches() {
        let phone = LaunchItem::new(
            "PHONE - Shortcut".to_string(),
            "Desktop".to_string(),
            PathBuf::from("C:\\Users\\TestUser\\Desktop\\PHONE.lnk"),
            false,
            50,
            None,
        );
        let one_piece = LaunchItem::new(
            "ONE PIECE".to_string(),
            "Recent".to_string(),
            PathBuf::from("C:\\Users\\TestUser\\Recent\\ONE PIECE.lnk"),
            true,
            200,
            None,
        );
        let plugin_state = plugins::PluginState::default();
        let results = collect_results("one", &[phone, one_piece], &[], &plugin_state);
        assert_eq!(
            results.first().map(|result| result.title.as_str()),
            Some("ONE PIECE")
        );
    }

    #[test]
    fn matches_separator_words() {
        assert!(score_text(
            &searchable_text("gemini yolo"),
            &searchable_text("gemini-yolo-wez.vbs"),
            false,
        )
        .is_some());
        assert!(score_text(
            &searchable_text("gemini yolo"),
            &searchable_text("gemini_yolo_wez"),
            false,
        )
        .is_some());
    }

    #[test]
    fn matches_chinese_text() {
        assert!(score_text(
            &searchable_text("\u{4e2d}\u{6587}"),
            &searchable_text("\u{4e2d}\u{6587}\u{5de5}\u{5177}"),
            true,
        )
        .is_some());
    }

    #[test]
    fn fuzzy_matches_non_contiguous_letters() {
        assert!(score_text(
            &searchable_text("prn"),
            &searchable_text("Printers and scanners"),
            false,
        )
        .is_some());
    }

    #[test]
    fn fuzzy_score_rewards_leftmost_and_boundary_positions() {
        let scoring = ScoringConfig::default();

        assert!(fuzzy_score("tn", "tnt", &scoring) > fuzzy_score("tn", "bitwarden", &scoring));
    }

    #[test]
    fn higher_score_index_beats_launch_history_cache() {
        let launched = LaunchItem::new(
            "Calculator".to_string(),
            "Start Menu".to_string(),
            PathBuf::from("C:\\Start\\Calculator.lnk"),
            false,
            100,
            None,
        );
        let indexed = LaunchItem::new(
            "Calendar".to_string(),
            "Desktop".to_string(),
            PathBuf::from("C:\\Desktop\\Calendar.exe"),
            false,
            250,
            None,
        );
        let plugin_state = plugins::PluginState::default();
        let results = collect_results(
            "cal",
            &[indexed, launched.clone()],
            &[recent_entry_line(110.0, &recent_item_key(&launched.path))],
            &plugin_state,
        );
        assert_eq!(
            results.first().map(|result| result.title.as_str()),
            Some("Calendar")
        );
        assert_eq!(
            results.first().map(|result| result.from_history),
            Some(false)
        );
    }

    #[test]
    fn higher_index_score_beats_launch_history_source() {
        let launched = LaunchItem::new(
            "Work Tool".to_string(),
            "History".to_string(),
            PathBuf::from("C:\\History\\Work Tool.lnk"),
            false,
            10,
            None,
        );
        let indexed = LaunchItem::new(
            "Work Tool Pro".to_string(),
            "Index".to_string(),
            PathBuf::from("C:\\Index\\Work Tool Pro.exe"),
            false,
            10_000,
            None,
        );
        let plugin_state = plugins::PluginState::default();

        let results = collect_results(
            "work tool",
            &[indexed, launched.clone()],
            &[recent_entry_line(100.0, &recent_item_key(&launched.path))],
            &plugin_state,
        );

        assert_eq!(
            results.first().map(|result| result.title.as_str()),
            Some("Work Tool Pro")
        );
        assert_eq!(
            results.first().map(|result| result.from_history),
            Some(false)
        );
    }

    #[test]
    fn negative_search_depth_means_all_subfolders() {
        assert_eq!(normalize_search_depth(-1), SEARCH_DEPTH_ALL);
        assert_eq!(depth_display(SEARCH_DEPTH_ALL), "-1");
        assert_eq!(normalize_search_depth(2), 2);
    }

    #[test]
    fn default_search_folders_use_all_subfolders() {
        let root = parse_index_root_line("C:\\Apps | score=100").unwrap();
        assert_eq!(root.max_depth, DEFAULT_SEARCH_DEPTH);
        assert!(default_index_text()
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .all(|line| line.contains("depth=-1")));
        assert!(default_index_text().contains(QUICK_LAUNCH_INDEX_ROOT));
        assert!(!default_index_text().contains("Application Data"));
        assert!(default_index_text().contains("%PROGRAMFILES% | enabled=0 | score=60 | depth=-1"));
        assert!(default_index_text().contains("%PROGRAMFILES86% | enabled=0 | score=60 | depth=-1"));
    }

    #[test]
    fn custom_search_folder_has_no_first_run_default_value() {
        let custom_root = parse_index_root_line(
            "D:\\Projects\\Example Workspace | enabled=1 | score=100 | depth=-1",
        )
        .unwrap();

        assert_eq!(default_index_root_value(&custom_root), "");
        assert!(default_index_root_for(&custom_root).is_none());
    }

    #[test]
    fn parses_query_modifiers_and_sall() {
        let spec = parse_search_query("note +sall +docs");

        assert_eq!(spec.mode, SearchQueryMode::NormalSearch);
        assert_eq!(spec.search_text, "note");
        assert_eq!(spec.folded_search_text, "note");
        assert!(spec.show_all);
        assert_eq!(spec.modifiers, vec!["docs"]);
    }

    #[test]
    fn modifier_roots_follow_keyword_rules() {
        let always = parse_index_root_line("C:\\Apps | score=100").unwrap();
        let docs = parse_index_root_line("C:\\Docs | score=100 | keywords=docs").unwrap();
        let star = parse_index_root_line("C:\\Any | score=100 | keywords=*").unwrap();
        let blank = parse_index_root_line("C:\\Blank | score=100 | keywords=[Blank],zip").unwrap();
        let safe = parse_index_root_line("C:\\Safe | score=100 | keywords=*,-safe").unwrap();

        assert!(root_matches_query_modifiers(&always, &[], false));
        assert!(!root_matches_query_modifiers(
            &always,
            &["docs".to_string()],
            true
        ));
        assert!(root_matches_query_modifiers(
            &always,
            &["unknown".to_string()],
            false
        ));
        assert!(!root_matches_query_modifiers(&docs, &[], false));
        assert!(root_matches_query_modifiers(
            &docs,
            &["docs".to_string()],
            true
        ));
        assert!(!root_matches_query_modifiers(&star, &[], false));
        assert!(root_matches_query_modifiers(
            &star,
            &["docs".to_string()],
            true
        ));
        assert!(root_matches_query_modifiers(&blank, &[], false));
        assert!(root_matches_query_modifiers(
            &blank,
            &["zip".to_string()],
            true
        ));
        assert!(!root_matches_query_modifiers(
            &blank,
            &["mp3".to_string()],
            false
        ));
        assert!(root_matches_query_modifiers(
            &safe,
            &["mp3".to_string()],
            true
        ));
        assert!(!root_matches_query_modifiers(
            &safe,
            &["safe".to_string()],
            true
        ));
    }

    #[test]
    fn pattern_modifiers_require_plus_keywords() {
        let scoring = ScoringConfig {
            pattern_rules: vec![PatternRule {
                pattern: "*.mp3".to_string(),
                folded_pattern: fold_text("*.mp3"),
                score: 3000,
                modifiers: vec!["mp3".to_string()],
            }],
            ..Default::default()
        };
        let path = PathBuf::from("C:\\Music\\song.mp3");
        let plain = parse_search_query("song mp3").effective_for_scoring(&scoring);
        let explicit = parse_search_query("song +mp3").effective_for_scoring(&scoring);

        assert_eq!(plain.scoring_modifiers, Vec::<String>::new());
        assert_eq!(plain.folded_search_text, "song mp3");
        assert_eq!(
            pattern_score_with_config_and_modifiers(&path, &scoring, &plain.scoring_modifiers),
            0
        );
        assert_eq!(explicit.scoring_modifiers, vec!["mp3".to_string()]);
        assert_eq!(explicit.folded_search_text, "song");
        assert_eq!(
            pattern_score_with_config_and_modifiers(&path, &scoring, &explicit.scoring_modifiers),
            3000
        );
    }

    #[test]
    fn pattern_modifier_special_keywords_match_farr_style() {
        let scoring = ScoringConfig {
            pattern_rules: vec![
                PatternRule {
                    pattern: "*.zip".to_string(),
                    folded_pattern: fold_text("*.zip"),
                    score: 500,
                    modifiers: normalize_keywords("[Blank] zip"),
                },
                PatternRule {
                    pattern: "*.bak".to_string(),
                    folded_pattern: fold_text("*.bak"),
                    score: -999,
                    modifiers: normalize_keywords("* -safe"),
                },
            ],
            ..Default::default()
        };
        let zip = PathBuf::from("C:\\Backup\\archive.zip");
        let bak = PathBuf::from("C:\\Backup\\old.bak");

        assert_eq!(
            pattern_score_with_config_and_modifiers(&zip, &scoring, &[]),
            500
        );
        assert_eq!(
            pattern_score_with_config_and_modifiers(&zip, &scoring, &["zip".to_string()]),
            500
        );
        assert_eq!(
            pattern_score_with_config_and_modifiers(&zip, &scoring, &["mp3".to_string()]),
            0
        );
        assert_eq!(
            pattern_score_with_config_and_modifiers(&bak, &scoring, &["mp3".to_string()]),
            -999
        );
        assert_eq!(
            pattern_score_with_config_and_modifiers(&bak, &scoring, &["safe".to_string()]),
            0
        );
    }

    #[test]
    fn unmodified_pattern_rules_pause_when_modifier_matches_other_rule() {
        let scoring = ScoringConfig {
            pattern_rules: vec![
                PatternRule {
                    pattern: "*.txt".to_string(),
                    folded_pattern: fold_text("*.txt"),
                    score: 100,
                    modifiers: Vec::new(),
                },
                PatternRule {
                    pattern: "*.pdf".to_string(),
                    folded_pattern: fold_text("*.pdf"),
                    score: 200,
                    modifiers: vec!["docs".to_string()],
                },
            ],
            ..Default::default()
        };
        let path = PathBuf::from("C:\\Docs\\readme.txt");

        assert_eq!(
            pattern_score_with_config_and_modifiers(&path, &scoring, &[]),
            100
        );
        assert_eq!(
            pattern_score_with_config_and_modifiers(&path, &scoring, &["docs".to_string()]),
            0
        );
        assert_eq!(
            pattern_score_with_config_and_modifiers(&path, &scoring, &["unknown".to_string()]),
            100
        );
    }

    #[test]
    fn windows_style_wildcards_match_full_paths() {
        let chrome = PatternRule {
            pattern: "*chrome*.lnk".to_string(),
            folded_pattern: fold_text("*chrome*.lnk"),
            score: 1,
            modifiers: Vec::new(),
        };
        let nested = PatternRule {
            pattern: "D:\\Apps\\*\\bin\\*.exe".to_string(),
            folded_pattern: fold_text("D:\\Apps\\*\\bin\\*.exe"),
            score: 1,
            modifiers: Vec::new(),
        };

        assert!(wildcard_rule_matches(
            &chrome,
            Path::new("C:\\Start Menu\\Google Chrome.lnk"),
            &fold_text("C:\\Start Menu\\Google Chrome.lnk")
        ));
        assert!(wildcard_rule_matches(
            &nested,
            Path::new("D:\\Apps\\Foo\\bin\\tool.exe"),
            &fold_text("D:\\Apps\\Foo\\bin\\tool.exe")
        ));
    }

    #[test]
    fn basename_patterns_do_not_match_parent_folders() {
        let help = PatternRule {
            pattern: "*help*".to_string(),
            folded_pattern: fold_text("*help*"),
            score: -100,
            modifiers: Vec::new(),
        };

        assert!(!wildcard_rule_matches(
            &help,
            Path::new("C:\\x\\@swc\\helpers\\build.js"),
            &fold_text("C:\\x\\@swc\\helpers\\build.js")
        ));
        assert!(wildcard_rule_matches(
            &help,
            Path::new("C:\\x\\help.lnk"),
            &fold_text("C:\\x\\help.lnk")
        ));
    }

    #[test]
    fn enabled_pattern_scoring_rules_control_file_scanning() {
        let mut scoring = ScoringConfig {
            pattern_rules: vec![PatternRule {
                pattern: "*.exe".to_string(),
                folded_pattern: fold_text("*.exe"),
                score: 150,
                modifiers: Vec::new(),
            }],
            ..Default::default()
        };

        assert!(is_allowed_by_pattern_scoring(
            Path::new("C:\\Tools\\app.exe"),
            &scoring
        ));
        assert!(!is_allowed_by_pattern_scoring(
            Path::new("C:\\Tools\\notes.txt"),
            &scoring
        ));

        scoring.pattern_rules.clear();
        assert!(!is_allowed_by_pattern_scoring(
            Path::new("C:\\Tools\\app.exe"),
            &scoring
        ));
    }

    #[test]
    fn index_root_keywords_roundtrip() {
        let root =
            parse_index_root_line("C:\\Docs | enabled=1 | score=120 | depth=-1 | keywords=docs,*")
                .unwrap();

        assert_eq!(root.keywords, vec!["docs", "*"]);
        let line = index_root_line(&root);
        assert!(line.contains("keywords=docs,*"));
        assert!(!line.contains("extensions="));
    }

    #[test]
    fn directory_browse_query_uses_base_and_filter() {
        let spec = parse_search_query("C:\\Windows\\sys");

        assert_eq!(spec.mode, SearchQueryMode::DirectoryBrowse);
        let directory = spec.directory.unwrap();
        assert_eq!(directory.base, PathBuf::from("C:\\Windows"));
        assert_eq!(directory.filter, "sys");
    }

    #[test]
    fn result_limit_clamps_only_to_minimum() {
        assert_eq!(parse_result_limit("8"), MIN_RESULT_LIMIT);
        assert_eq!(parse_result_limit("9"), 9);
        assert_eq!(parse_result_limit("500"), 500);
        assert_eq!(parse_result_limit("501"), 501);
        assert_eq!(parse_result_limit("5000"), 5000);
    }

    #[test]
    fn collect_results_respects_effective_limit() {
        let items = (0..12)
            .map(|index| {
                LaunchItem::new(
                    format!("Note {index}"),
                    r"C:\Notes".to_string(),
                    PathBuf::from(format!(r"C:\Notes\Note {index}.txt")),
                    false,
                    100 - index,
                    None,
                )
            })
            .collect::<Vec<_>>();
        let plugin_state = plugins::PluginState::default();

        let results = collect_results_with_config(
            "note",
            &items,
            &[],
            &plugin_state,
            &ScoringConfig::default(),
            9,
        );

        assert_eq!(results.len(), 9);
    }

    #[test]
    fn sall_uses_unbounded_effective_limit() {
        let spec = parse_search_query("note +sall");
        let result_limit = 12;
        let effective_limit = if spec.show_all {
            usize::MAX
        } else {
            result_limit
        };

        assert!(spec.show_all);
        assert_eq!(effective_limit, usize::MAX);
    }

    #[test]
    fn result_list_display_respects_score_breakdown_setting() {
        let result = SearchResult {
            title: "App".to_string(),
            subtitle: "C:\\App.exe".to_string(),
            target: LaunchTarget::Path(PathBuf::from("C:\\App.exe")),
            is_dir: false,
            from_history: false,
            from_query_launch_rule: false,
            ranking_kind: ResultRankingKind::Heuristic,
            explanation: None,
            display_score: 42,
            score_detail: "text 10 + path 32 = 42".to_string(),
            score: 42,
        };

        assert!(result_list_display_text(0, &result, true).contains("(text 10 + path 32 = 42)"));
        let hidden = result_list_display_text(0, &result, false);
        assert!(hidden.contains("[score 42]"));
        assert!(!hidden.contains("text 10"));
    }

    #[test]
    fn result_score_breakdown_tooltip_requires_real_breakdown() {
        let mut result = SearchResult {
            title: "App".to_string(),
            subtitle: r"C:\App.exe".to_string(),
            target: LaunchTarget::Path(PathBuf::from(r"C:\App.exe")),
            is_dir: false,
            from_history: false,
            from_query_launch_rule: false,
            ranking_kind: ResultRankingKind::Heuristic,
            explanation: None,
            display_score: 42,
            score_detail: "text 10 + path 32 = 42".to_string(),
            score: 42,
        };

        assert_eq!(
            result_score_breakdown_tooltip_text(&result),
            Some("text 10 + path 32 = 42")
        );
        assert_eq!(
            result_score_breakdown_tooltip_text_for_setting(&result, false),
            None
        );
        for detail in ["", "recent order", "starred", "plugin", "detail"] {
            result.score_detail = detail.to_string();
            assert_eq!(result_score_breakdown_tooltip_text(&result), None);
        }
    }

    #[test]
    fn result_detail_text_respects_score_breakdown_setting() {
        let result = SearchResult {
            title: "App".to_string(),
            subtitle: r"C:\App.exe".to_string(),
            target: LaunchTarget::Path(PathBuf::from(r"C:\App.exe")),
            is_dir: false,
            from_history: false,
            from_query_launch_rule: false,
            ranking_kind: ResultRankingKind::Heuristic,
            explanation: None,
            display_score: 42,
            score_detail: "text 10 + path 32 = 42".to_string(),
            score: 42,
        };
        let rule_result = SearchResult {
            from_query_launch_rule: true,
            ranking_kind: ResultRankingKind::Heuristic,
            explanation: None,
            ..result.clone()
        };

        assert_eq!(
            result_detail_text(&result, true),
            r"C:\App.exe - score 42 (text 10 + path 32 = 42)"
        );
        assert_eq!(result_detail_text(&result, false), r"C:\App.exe - score 42");
        assert_eq!(
            result_detail_text(&rule_result, false),
            r"C:\App.exe - score *"
        );

        let recent_result = SearchResult {
            score_detail: "recent order".to_string(),
            display_score: 0,
            score: 0,
            ..result
        };
        assert_eq!(result_detail_text(&recent_result, false), r"C:\App.exe");
        assert!(!result_list_display_text(0, &recent_result, false).contains("score"));
    }

    #[test]
    fn shipped_language_files_parse_with_unique_ids() {
        let language_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("Languages");
        let mut paths = fs::read_dir(&language_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("ini"))
            })
            .collect::<Vec<_>>();
        paths.sort();
        assert!(!paths.is_empty());

        let mut ids = HashSet::new();
        for path in paths {
            let pack = parse_language_file(&path).unwrap();
            assert!(!pack.id.trim().is_empty());
            assert!(!pack.name.trim().is_empty());
            assert!(ids.insert(normalize_language_id(pack.id)));
        }
    }

    #[test]
    fn visible_result_signature_detects_meaningful_changes() {
        let first = SearchResult {
            title: "Tool".to_string(),
            subtitle: "Apps".to_string(),
            target: LaunchTarget::Path(PathBuf::from("C:\\Apps\\Tool.exe")),
            is_dir: false,
            from_history: false,
            from_query_launch_rule: false,
            ranking_kind: ResultRankingKind::Heuristic,
            explanation: None,
            display_score: 10,
            score_detail: "detail".to_string(),
            score: 10,
        };
        let same = SearchResult {
            title: "Tool".to_string(),
            subtitle: "Apps".to_string(),
            target: LaunchTarget::Path(PathBuf::from("C:\\Apps\\Tool.exe")),
            is_dir: false,
            from_history: false,
            from_query_launch_rule: false,
            ranking_kind: ResultRankingKind::Heuristic,
            explanation: None,
            display_score: 10,
            score_detail: "detail".to_string(),
            score: 10,
        };
        let changed = SearchResult {
            title: "Tool".to_string(),
            subtitle: "Apps".to_string(),
            target: LaunchTarget::Path(PathBuf::from("C:\\Apps\\Tool.exe")),
            is_dir: false,
            from_history: false,
            from_query_launch_rule: false,
            ranking_kind: ResultRankingKind::Heuristic,
            explanation: None,
            display_score: 11,
            score_detail: "detail".to_string(),
            score: 11,
        };

        assert_eq!(
            visible_results_signature(std::slice::from_ref(&first), 9),
            visible_results_signature(&[same], 9)
        );
        assert_ne!(
            visible_results_signature(&[first], 9),
            visible_results_signature(&[changed], 9)
        );
    }

    #[test]
    fn enter_without_selection_targets_first_visible_result() {
        assert_eq!(first_visible_result_index(3, 9), Some(0));
        assert_eq!(first_visible_result_index(0, 9), None);
        assert_eq!(first_visible_result_index(3, 0), None);
    }

    #[test]
    fn arrows_start_selection_from_no_selection() {
        assert_eq!(selection_after_move(None, 4, 1), Some(0));
        assert_eq!(selection_after_move(None, 4, 10), Some(0));
        assert_eq!(selection_after_move(None, 4, -1), Some(3));
        assert_eq!(selection_after_move(Some(1), 4, 1), Some(2));
        assert_eq!(selection_after_move(Some(0), 4, -1), Some(3));
        assert_eq!(selection_after_move(Some(3), 4, 1), Some(0));
        assert_eq!(selection_after_move(Some(0), 4, 10), Some(2));
        assert_eq!(selection_after_move(None, 0, 1), None);
    }

    #[test]
    fn wrapped_selection_clamps_list_viewport() {
        assert_eq!(top_index_after_select(19, 19, 20, 9), 11);
        assert_eq!(top_index_after_select(11, 0, 20, 9), 0);
        assert_eq!(top_index_after_select(3, 5, 20, 9), 3);
        assert_eq!(top_index_after_select(0, 19, 20, 50), 0);
        assert_eq!(top_index_after_select(0, 0, 0, 9), 0);
    }

    #[test]
    fn default_pattern_scoring_matches_launcher_rules() {
        let scoring = ScoringConfig::default();

        assert_eq!(
            pattern_score_with_config(Path::new("C:\\Apps\\Tool.exe"), &scoring),
            150
        );
        assert_eq!(
            pattern_score_with_config(Path::new("C:\\Apps\\Tool.lnk"), &scoring),
            50
        );
        assert_eq!(
            pattern_score_with_config(Path::new("C:\\Apps\\Uninstall Tool.exe"), &scoring),
            -50
        );
        assert_eq!(
            pattern_score_with_config(Path::new("C:\\Apps\\License.txt"), &scoring),
            0
        );
        assert_eq!(scoring.pattern_rules.len(), 9);
    }

    #[test]
    fn default_heuristic_scoring_matches_launcher_visible_rules() {
        let text = default_scoring_text();

        assert!(text.contains("Recent First Launch Score=90"));
        assert!(text.contains("Recent Launch Increment=5"));
        assert!(text.contains("Exact Match Bonus=250"));
        assert!(text.contains("Prefix Match Bonus=110"));
        assert!(text.contains("Word Boundary Bonus=60"));
        assert!(text.contains("Recent Score Ceiling=250"));
        assert!(text.contains("Exact Word Bonus=75"));
        assert!(text.contains("Leftmost Match Bonus=25"));
        assert!(text.contains("Leftmost Distance Penalty=5"));
        assert!(text.contains("Length Score Weight=90"));
        assert!(text.contains("Compact Match Bonus=10"));
        assert!(!text.contains("Fuzzy Score Ceiling"));
        assert!(!text.contains("Long Path Threshold"));
        assert!(!text.contains("Script Bonus"));
        assert!(!text.contains("Long Path Penalty"));
        assert!(!text.contains("Short Name Score"));
        assert!(!text.contains("Full Days"));
        assert!(!text.contains("Half Days"));
        assert!(!text.contains("Quarter Days"));
        assert!(!text.contains("Recency Date Percent"));
        assert!(!text.contains("Percentage of Search String Points"));
        assert!(!text.contains("Whitespace Removal Penalty"));
        assert!(text.contains("Folder Score As % of File Score=90"));
    }

    #[test]
    fn default_heuristic_scoring_notes_are_complete() {
        let entries = parse_scoring_rule_entries(&default_scoring_text());
        let columns = heuristic_scoring_columns(AppLanguage::Source);

        assert_eq!(columns[2].1, 420);
        for entry in entries
            .iter()
            .filter(|entry| entry.kind == ScoringRuleKind::Heuristic)
        {
            assert!(
                !heuristic_scoring_note(&entry.key, AppLanguage::Source).is_empty(),
                "missing English note for {}",
                entry.key
            );
        }
    }

    #[test]
    fn scoring_rule_entries_roundtrip_disabled_rules() {
        let content = "[HeuristicScoring]\r\nRecent Launch Increment=120\r\nExact Match Bonus<<<100\r\nRecency Date Bonus<<<100\r\n\r\n[PatternScoring]\r\n*.exe=150\r\n*tmp*<<<-100\r\n";
        let entries = parse_scoring_rule_entries(content);

        assert!(entries.iter().any(|entry| {
            entry.kind == ScoringRuleKind::Heuristic
                && entry.key == "Recent Launch Increment"
                && entry.value == "120"
                && entry.enabled
        }));
        assert!(entries.iter().any(|entry| {
            entry.kind == ScoringRuleKind::Heuristic
                && entry.key == "Recency Date Bonus"
                && entry.value == "100"
                && !entry.enabled
        }));
        assert!(entries.iter().any(|entry| {
            entry.kind == ScoringRuleKind::Pattern
                && entry.key == "*tmp*"
                && entry.value == "-100"
                && !entry.enabled
        }));

        let saved = scoring_rule_entries_to_text(&entries);
        assert!(saved.contains("Recent Launch Increment=120"));
        assert!(saved.contains("Exact Match Bonus<<<100"));
        assert!(saved.contains("Recency Date Bonus<<<100"));
        assert!(saved.contains("*tmp*<<<-100"));
        let parsed = parse_scoring_config(&saved);
        assert_eq!(parsed.recent_launch_increment, 120);
        assert_eq!(parsed.exact_match_bonus, 0);
        assert!(!parsed.recency_date_enabled);
        assert!(parsed
            .pattern_rules
            .iter()
            .all(|rule| rule.pattern != "*tmp*"));
    }

    #[test]
    fn disabled_pattern_scoring_section_does_not_fallback_to_defaults() {
        let parsed = parse_scoring_config("[PatternScoring]\r\n*.exe<<<150\r\n*.lnk<<<50\r\n");

        assert!(parsed.pattern_rules.is_empty());
        assert_eq!(
            pattern_score_with_config(Path::new("C:\\Apps\\Tool.exe"), &parsed),
            0
        );
    }

    #[test]
    fn empty_pattern_scoring_section_does_not_fallback_to_defaults() {
        let parsed = parse_scoring_config(
            "[HeuristicScoring]\r\nRecent Launch Increment=50\r\n\r\n[PatternScoring]\r\n",
        );

        assert!(parsed.pattern_rules.is_empty());
    }

    #[test]
    fn missing_pattern_scoring_section_does_not_fallback_in_parser() {
        let parsed = parse_scoring_config("[HeuristicScoring]\r\nRecent Launch Increment=50\r\n");

        assert_eq!(
            pattern_score_with_config(Path::new("C:\\Apps\\Tool.exe"), &parsed),
            0
        );
    }

    #[test]
    fn history_score_follows_runtime_config() {
        let entry = RecentEntry { score: 250.0 };
        let uncapped = ScoringConfig {
            recent_score_ceiling: 300,
            ..Default::default()
        };
        let capped = ScoringConfig {
            recent_score_ceiling: 120,
            ..Default::default()
        };

        assert_eq!(recent_entry_score_with_config(&entry, &uncapped), 250);
        assert_eq!(recent_entry_score_with_config(&entry, &capped), 120);
        assert_eq!(
            recent_entry_line_with_config(700.0, "C:\\Apps\\Tool.exe", &capped),
            "700>>>C:\\Apps\\Tool.exe"
        );
    }

    #[test]
    fn recent_entry_config_line_preserves_user_score_without_default_clamp() {
        let line = recent_config_entry_line(&RecentConfigEntry {
            enabled: true,
            score: 250.0,
            path: "C:\\Apps\\Tool.exe".to_string(),
        });

        assert_eq!(line, "250>>>C:\\Apps\\Tool.exe");
        let parsed = parse_recent_config_entry(&line).unwrap();
        assert_eq!(parsed.score, 250.0);
    }

    #[test]
    fn index_root_path_escapes_pipe_without_breaking_windows_backslashes() {
        let windows_path = parse_index_root_line("C:\\Apps | score=100").unwrap();
        assert_eq!(windows_path.raw, "C:\\Apps");

        let root =
            parse_index_root_line("C:\\Apps\\A\\|B | enabled=1 | score=120 | depth=-1").unwrap();
        assert_eq!(root.raw, "C:\\Apps\\A|B");
        let line = index_root_line(&root);
        assert!(line.contains("C:\\\\Apps\\\\A\\|B"));
        let roundtrip = parse_index_root_line(&line).unwrap();
        assert_eq!(roundtrip.raw, root.raw);
    }

    #[test]
    fn pattern_scoring_modifiers_roundtrip() {
        let content = "[PatternScoring]\r\n*uninstall*=-200\r\n*uninstall*=300 | modifiers=uninstall,remove\r\n";
        let entries = parse_scoring_rule_entries(content);

        assert!(entries.iter().any(|entry| {
            entry.kind == ScoringRuleKind::Pattern
                && entry.key == "*uninstall*"
                && entry.value == "300"
                && entry.modifiers == vec!["uninstall", "remove"]
        }));
        let saved = scoring_rule_entries_to_text(&entries);
        assert!(saved.contains("*uninstall*=300 | modifiers=uninstall,remove"));

        let parsed = parse_scoring_config(&saved);
        assert!(parsed.pattern_rules.iter().any(|rule| {
            rule.pattern == "*uninstall*"
                && rule.score == 300
                && rule.modifiers == vec!["uninstall", "remove"]
        }));
    }

    #[test]
    fn pattern_modifier_keywords_apply_from_explicit_plus_query() {
        let scoring = ScoringConfig {
            pattern_rules: vec![
                PatternRule {
                    pattern: "*uninstall*".to_string(),
                    folded_pattern: fold_text("*uninstall*"),
                    score: -100,
                    modifiers: Vec::new(),
                },
                PatternRule {
                    pattern: "*uninstall*".to_string(),
                    folded_pattern: fold_text("*uninstall*"),
                    score: 300,
                    modifiers: vec!["uninstall".to_string(), "remove".to_string()],
                },
            ],
            ..Default::default()
        };
        let item = LaunchItem::new(
            "Zalo Uninstall".to_string(),
            "Programs".to_string(),
            PathBuf::from("C:\\Apps\\Zalo Uninstall.lnk"),
            false,
            0,
            None,
        );
        let recent_items = HashMap::new();
        let plain = parse_search_query("zalo").effective_for_scoring(&scoring);
        let uninstall = parse_search_query("zalo +uninstall").effective_for_scoring(&scoring);
        let remove = parse_search_query("zalo +remove").effective_for_scoring(&scoring);

        let plain_score = ranked_score_with_config(
            &plain.folded_search_text,
            &plain.scoring_modifiers,
            &item,
            &recent_items,
            &scoring,
        )
        .unwrap();
        let uninstall_score = ranked_score_with_config(
            &uninstall.folded_search_text,
            &uninstall.scoring_modifiers,
            &item,
            &recent_items,
            &scoring,
        )
        .unwrap();
        let remove_score = ranked_score_with_config(
            &remove.folded_search_text,
            &remove.scoring_modifiers,
            &item,
            &recent_items,
            &scoring,
        )
        .unwrap();

        assert_eq!(uninstall.folded_search_text, "zalo");
        assert_eq!(remove.folded_search_text, "zalo");
        assert_eq!(uninstall_score - plain_score, 400);
        assert_eq!(remove_score - plain_score, 400);
    }

    #[test]
    fn settings_sidebar_indexes_map_to_pages() {
        assert_eq!(settings_page_from_nav_index(0), SettingsPage::General);
        assert_eq!(settings_page_from_nav_index(1), SettingsPage::SearchFolders);
        assert_eq!(
            settings_page_from_nav_index(2),
            SettingsPage::HeuristicScoring
        );
        assert_eq!(
            settings_page_from_nav_index(3),
            SettingsPage::PatternScoring
        );
        assert_eq!(settings_page_from_nav_index(4), SettingsPage::PluginAliases);
        assert_eq!(
            settings_page_from_nav_index(5),
            SettingsPage::QueryLaunchRules
        );
        let pages = [
            SettingsPage::General,
            SettingsPage::SearchFolders,
            SettingsPage::HeuristicScoring,
            SettingsPage::PatternScoring,
            SettingsPage::PluginAliases,
            SettingsPage::QueryLaunchRules,
        ];
        for (index, page) in pages.into_iter().enumerate() {
            assert_eq!(config_nav_index_for_page(page), index);
            assert_eq!(settings_page_from_nav_index(index as i32), page);
        }
        assert_eq!(settings_page_from_nav_index(-1), SettingsPage::General);
        assert_eq!(settings_page_from_nav_index(6), SettingsPage::General);
    }

    #[test]
    fn settings_repaint_is_async_and_does_not_erase() {
        assert_ne!(SETTINGS_REPAINT_FLAGS & RDW_INVALIDATE, 0);
        assert_ne!(SETTINGS_REPAINT_FLAGS & RDW_ALLCHILDREN, 0);
        assert_eq!(SETTINGS_REPAINT_FLAGS & RDW_ERASE, 0);
        assert_eq!(SETTINGS_REPAINT_FLAGS & RDW_UPDATENOW, 0);
    }

    #[test]
    fn settings_navigation_treats_each_mouse_press_as_selection() {
        for message in [WM_LBUTTONDOWN, WM_LBUTTONDBLCLK] {
            assert_eq!(wndproc::settings_nav_mouse_message(message), WM_LBUTTONDOWN);
        }
        for message in [WM_LBUTTONUP, WM_MOUSEMOVE, WM_KEYDOWN, WM_NCDESTROY] {
            assert_eq!(wndproc::settings_nav_mouse_message(message), message);
        }
    }

    #[test]
    fn settings_redraw_restores_each_child_visibility() {
        assert!(app_impl::config_child_visibility_after_redraw(
            true, false, false
        ));
        assert!(!app_impl::config_child_visibility_after_redraw(
            false, false, false
        ));
        assert!(app_impl::config_child_visibility_after_redraw(
            false, true, true
        ));
        assert!(!app_impl::config_child_visibility_after_redraw(
            true, true, false
        ));
    }

    #[test]
    fn search_query_history_normalizes_for_settings() {
        let items = vec![
            "  note  ".to_string(),
            "NOTE".to_string(),
            "/c 1+1".to_string(),
            "calc".to_string(),
            "".to_string(),
        ];

        let plugin_state = plugins::PluginState::default();
        assert_eq!(
            normalize_search_history_items(&items, &plugin_state),
            vec!["note".to_string(), "calc".to_string()]
        );
    }

    #[test]
    fn query_launch_rule_upserts_by_query() {
        let mut rules = vec![QueryLaunchRule {
            query: "note".to_string(),
            target: "C:\\Apps\\Old.exe".to_string(),
        }];

        upsert_query_launch_rule(&mut rules, " NOTE ", "C:\\Apps\\New.exe");

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].query, "NOTE");
        assert_eq!(rules[0].target, "C:\\Apps\\New.exe");
    }

    #[test]
    fn query_launch_rule_removes_by_query_and_target() {
        let mut rules = vec![
            QueryLaunchRule {
                query: "note".to_string(),
                target: "C:\\Apps\\Tool.exe".to_string(),
            },
            QueryLaunchRule {
                query: "calc".to_string(),
                target: "C:\\Apps\\Calc.exe".to_string(),
            },
        ];

        assert!(remove_query_launch_rule(
            &mut rules,
            " NOTE ",
            &PathBuf::from("C:/Apps/Tool.exe")
        ));

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].query, "calc");
        assert!(!remove_query_launch_rule(
            &mut rules,
            "note",
            &PathBuf::from("C:\\Apps\\Tool.exe")
        ));
    }

    #[test]
    fn query_launch_rule_filter_matches_visible_rows() {
        let rules = vec![
            QueryLaunchRule {
                query: "Tiếng Việt".to_string(),
                target: "C:\\Apps\\Chrome.exe".to_string(),
            },
            QueryLaunchRule {
                query: "calculator".to_string(),
                target: "C:\\Tools\\Calc.exe".to_string(),
            },
        ];

        assert_eq!(query_launch_rule_visible_indices(&rules, ""), vec![0, 1]);
        assert_eq!(
            query_launch_rule_visible_indices(&rules, "tieng chrome"),
            vec![0]
        );
        assert_eq!(
            query_launch_rule_visible_indices(&rules, "CALC tools"),
            vec![1]
        );
        assert!(query_launch_rule_visible_indices(&rules, "chrome missing").is_empty());
    }

    #[test]
    fn query_launch_rule_filtered_delete_uses_actual_indices() {
        let mut rules = vec![
            QueryLaunchRule {
                query: "alpha".to_string(),
                target: "C:\\Apps\\A.exe".to_string(),
            },
            QueryLaunchRule {
                query: "chrome work".to_string(),
                target: "C:\\Apps\\Chrome.exe".to_string(),
            },
            QueryLaunchRule {
                query: "beta".to_string(),
                target: "C:\\Apps\\B.exe".to_string(),
            },
            QueryLaunchRule {
                query: "chrome home".to_string(),
                target: "D:\\Tools\\Chrome.lnk".to_string(),
            },
        ];
        let visible = query_launch_rule_visible_indices(&rules, "chrome");
        let actual = query_launch_rule_actual_indices_for_visible_selection(&visible, &[1]);

        assert_eq!(visible, vec![1, 3]);
        assert_eq!(actual, vec![3]);
        assert_eq!(delete_query_launch_rules_by_indices(&mut rules, &actual), 1);
        assert_eq!(
            rules
                .iter()
                .map(|rule| rule.query.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "chrome work", "beta"]
        );
    }

    #[test]
    fn query_launch_rule_count_text_is_localized() {
        assert_eq!(
            query_launch_rule_selection_count_text(AppLanguage::Source, 13, 17, 20),
            "Selected: 13 | Showing: 17 | Total: 20"
        );
    }

    #[test]
    fn query_launch_rule_priority_inserts_existing_target_first() {
        let dir = env::temp_dir().join(format!("flashlaunch-query-rule-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("Preferred.exe");
        fs::write(&target, "").unwrap();
        let other = LaunchItem::new(
            "Other".to_string(),
            "Apps".to_string(),
            PathBuf::from("C:\\Apps\\Other.exe"),
            false,
            100,
            None,
        );
        let plugin_state = plugins::PluginState::default();
        let mut results = collect_results("preferred", &[other], &[], &plugin_state);
        let rules = vec![QueryLaunchRule {
            query: "preferred".to_string(),
            target: target.to_string_lossy().to_string(),
        }];

        prioritize_query_launch_rule_result(
            &mut results,
            "preferred",
            &rules,
            &ScoringConfig::default(),
        );

        assert_eq!(results.first().and_then(result_target_path), Some(target));
        assert!(results
            .first()
            .is_some_and(|result| result.from_query_launch_rule));
        assert!(results
            .first()
            .is_some_and(|result| result.display_score != i32::MAX && result.score != i32::MAX));
        assert_eq!(
            results.first().map(result_score_label),
            Some("*".to_string())
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn query_launch_rule_missing_target_is_ignored() {
        let rules = vec![QueryLaunchRule {
            query: "missing".to_string(),
            target: "C:\\Missing\\Nope.exe".to_string(),
        }];
        let mut results = Vec::new();

        prioritize_query_launch_rule_result(
            &mut results,
            "missing",
            &rules,
            &ScoringConfig::default(),
        );

        assert!(results.is_empty());
    }

    #[test]
    fn disabled_recent_history_is_not_scored() {
        let path = "C:\\Apps\\Tool.exe";
        let entries = vec![format!("110<<<{path}")];
        let lookup = recent_lookup(&entries);
        assert!(lookup.is_empty());

        let parsed = parse_recent_config_entry(&entries[0]).unwrap();
        assert!(!parsed.enabled);
        assert_eq!(recent_config_entry_line(&parsed), entries[0]);
    }

    #[test]
    fn recording_disabled_recent_reenables_without_duplicate() {
        let path = PathBuf::from("C:\\Apps\\Tool.exe");
        let mut entries = vec![format!("110<<<{}", path.to_string_lossy())];

        record_recent_item_with_config(&mut entries, &path, &ScoringConfig::default());

        assert_eq!(entries.len(), 1);
        assert!(entries[0].contains(">>>"));
        assert!(recent_lookup(&entries).contains_key(&recent_item_key(&path)));
    }

    #[test]
    fn expands_common_path_aliases() {
        assert!(expand_path("%USERPROFILE%\\Desktop")
            .to_string_lossy()
            .contains("Desktop"));
        assert!(expand_path("%PROGRAMFILES%")
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("program files"));
        assert!(expand_path("%MYSTARTMENU%")
            .to_string_lossy()
            .contains("Start Menu"));
    }

    #[test]
    fn alias_dropdown_item_shows_resolved_machine_path() {
        let alias = path_aliases()
            .iter()
            .find(|alias| alias.name == "%USERPROFILE%")
            .unwrap();
        let display = alias_display_text(alias);
        assert!(display.contains("%USERPROFILE%"));
        assert!(display.contains(&env::var("USERPROFILE").unwrap()));
        assert_eq!(alias_name_from_combo_text(&display), "%USERPROFILE%");
        assert_eq!(alias_name_from_combo_text("%APPDATA%"), "%APPDATA%");
        let alias_names = path_aliases()
            .iter()
            .map(|alias| alias.name)
            .collect::<Vec<_>>();
        assert!(alias_names.contains(&"%PROGRAMFILES%"));
        assert!(alias_names.contains(&"%PROGRAMFILES86%"));
        for removed in [
            "%PROFILE%",
            "%MYAPPDATA%",
            "%COMMONAPPDATA%",
            "%PROGRAMFILES(X86)%",
            "%PROGRAMFILES64%",
            "%SPECIALSYS_DOCS%",
            "%SPECIALSYS_APPS%",
            "%EXEDIR%",
            "%SYSTEMROOT%",
        ] {
            assert!(!alias_names.contains(&removed));
        }
    }

    #[test]
    fn language_file_supports_escaped_equals_in_keys() {
        let path = env::temp_dir().join(format!(
            "flashlaunch-language-{}-{}.ini",
            std::process::id(),
            "escaped-equals"
        ));
        fs::write(
            &path,
            "id=test\nname=Test\n\n[Strings]\nQuery key \\= value=Translated equals \\= sign\n",
        )
        .unwrap();

        let pack = parse_language_file(&path).unwrap();
        assert_eq!(pack.id, "test");
        assert_eq!(pack.name, "Test");
        assert_eq!(
            pack.translations.get("Query key = value").copied(),
            Some("Translated equals = sign")
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn folder_menu_path_adds_trailing_slash() {
        assert_eq!(
            folder_menu_path(Path::new("C:\\Users\\TestUser\\Desktop\\PROGRAMMING")),
            "C:\\Users\\TestUser\\Desktop\\PROGRAMMING\\"
        );
    }
}

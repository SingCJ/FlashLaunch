use std::fs;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::SystemServices::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::*;

pub(crate) fn settings_nav_mouse_message(message: u32) -> u32 {
    // A navigation list has no double-click action: every press selects its row.
    if message == WM_LBUTTONDBLCLK {
        WM_LBUTTONDOWN
    } else {
        message
    }
}

pub(crate) unsafe extern "system" fn settings_nav_subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    _reference_data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        RemoveWindowSubclass(hwnd, Some(settings_nav_subclass_proc), subclass_id);
    }
    DefSubclassProc(hwnd, settings_nav_mouse_message(message), wparam, lparam)
}

pub(crate) unsafe extern "system" fn tooltip_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut paint);
            let mut rect: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rect);
            FillRect(hdc, &rect, GetSysColorBrush(COLOR_INFOBK));
            let border = CreateSolidBrush(0x00A0A0A0);
            FrameRect(hdc, &rect, border);
            DeleteObject(border as _);

            rect.left += 7;
            rect.right -= 7;
            rect.top += 4;
            rect.bottom -= 4;
            let old_font = SelectObject(hdc, GetStockObject(DEFAULT_GUI_FONT));
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, GetSysColor(COLOR_INFOTEXT));
            let mut text = vec![0u16; GetWindowTextLengthW(hwnd).max(0) as usize + 1];
            GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32);
            DrawTextW(
                hdc,
                text.as_ptr(),
                -1,
                &mut rect,
                DT_WORDBREAK | DT_NOPREFIX,
            );
            SelectObject(hdc, old_font);
            EndPaint(hwnd, &paint);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

pub(crate) fn with_app<T>(callback: impl FnOnce(&mut AppState) -> T) -> Option<T> {
    with_app_tls(|cell| cell.try_borrow_mut().ok()?.as_mut().map(callback))
}

fn should_end_helper_session(window_visible: bool, launcher_app_active: bool) -> bool {
    !window_visible || !launcher_app_active
}

unsafe fn show_result_context_menu_after_borrow(point: POINT) {
    let request = with_app(|app| unsafe { app.result_context_menu_request() }).flatten();
    let Some(request) = request else {
        return;
    };
    let command = AppState::track_result_context_menu(&request, point.x, point.y);
    if command != 0 {
        with_app(|app| unsafe {
            app.execute_result_context_menu_command(&request, command);
        });
    }
}

unsafe fn show_tray_menu_after_borrow() {
    let menu_state = with_app(|app| (app.hwnd, app.language));
    let Some((hwnd, language)) = menu_state else {
        return;
    };
    let command = track_tray_menu(hwnd, language);
    if command != 0 {
        with_app(|app| unsafe {
            execute_tray_menu_command(app, command);
        });
    }
}

unsafe fn list_view_font_or_default(list_hwnd: HWND) -> HGDIOBJ {
    let font = SendMessageW(list_hwnd, WM_GETFONT, 0, 0) as HGDIOBJ;
    if font.is_null() {
        GetStockObject(DEFAULT_GUI_FONT)
    } else {
        font
    }
}

unsafe fn list_view_item_param(list_hwnd: HWND, row: usize) -> LPARAM {
    if row > i32::MAX as usize {
        return 0;
    }
    let mut item: LVITEMW = std::mem::zeroed();
    item.mask = LVIF_PARAM;
    item.iItem = row as i32;
    item.iSubItem = 0;
    if SendMessageW(list_hwnd, LVM_GETITEMW, 0, &mut item as *mut _ as isize) == 0 {
        0
    } else {
        item.lParam
    }
}

pub(crate) unsafe extern "system" fn list_subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    match message {
        WM_LBUTTONDOWN => {
            let handled = with_app(|app| unsafe {
                hwnd == app.list
                    && app.begin_result_drag_at_point(
                        signed_loword(lparam as usize) as i32,
                        signed_hiword(lparam as usize) as i32,
                    )
            })
            .unwrap_or(false);
            if handled {
                SetCapture(hwnd);
                InvalidateRect(hwnd, null(), TRUE);
            }
        }
        WM_MOUSEMOVE => {
            let mut track: TRACKMOUSEEVENT = std::mem::zeroed();
            track.cbSize = std::mem::size_of::<TRACKMOUSEEVENT>() as u32;
            track.dwFlags = TME_LEAVE;
            track.hwndTrack = hwnd;
            TrackMouseEvent(&mut track);
            let drag = with_app(|app| unsafe {
                if hwnd == app.list && (wparam & MK_LBUTTON as usize) != 0 {
                    return app.take_ready_result_drag(
                        signed_loword(lparam as usize) as i32,
                        signed_hiword(lparam as usize) as i32,
                    );
                }
                None
            })
            .flatten();
            if let Some(drag) = drag {
                with_app(|app| unsafe { app.hide_app_tooltip() });
                ReleaseCapture();
                if let Err(error) = begin_shell_shortcut_drag(hwnd, &drag.path, &drag.title) {
                    with_app(|app| unsafe {
                        app.set_main_status(&localized_format1(
                            app.language,
                            "Could not drag shortcut: {}",
                            error,
                        ))
                    });
                }
                return 0;
            }
            with_app(|app| unsafe {
                if hwnd == app.list {
                    app.show_hover_path(
                        signed_loword(lparam as usize) as i32,
                        signed_hiword(lparam as usize) as i32,
                    );
                }
            });
        }
        WM_LBUTTONUP => {
            with_app(|app| unsafe { app.hide_app_tooltip() });
            let had_drag = with_app(|app| {
                let had_drag = app.pending_drag.is_some();
                app.clear_result_drag();
                had_drag
            })
            .unwrap_or(false);
            if had_drag {
                ReleaseCapture();
            }
        }
        WM_LBUTTONDBLCLK => {
            let handled = with_app(|app| unsafe {
                if hwnd != app.list || GetKeyState(VK_SHIFT as i32) >= 0 {
                    return false;
                }
                let x = signed_loword(lparam as usize) as i32;
                let y = signed_hiword(lparam as usize) as i32;
                if app.select_result_at_point(x, y).is_none() {
                    return false;
                }
                app.open_selected_target_folder();
                true
            })
            .unwrap_or(false);
            if handled {
                return 0;
            }
        }
        WM_RBUTTONDOWN => {
            with_app(|app| unsafe { app.hide_app_tooltip() });
            with_app(|app| app.clear_result_drag());
            let screen_point = with_app(|app| unsafe {
                if hwnd == app.list
                    && app
                        .select_result_at_point(
                            signed_loword(lparam as usize) as i32,
                            signed_hiword(lparam as usize) as i32,
                        )
                        .is_some()
                {
                    let mut point = POINT {
                        x: signed_loword(lparam as usize) as i32,
                        y: signed_hiword(lparam as usize) as i32,
                    };
                    ClientToScreen(app.list, &mut point);
                    return Some(point);
                }
                None
            })
            .flatten();
            InvalidateRect(hwnd, null(), TRUE);
            UpdateWindow(hwnd);
            if let Some(point) = screen_point {
                show_result_context_menu_after_borrow(point);
            }
            return 0;
        }
        WM_CONTEXTMENU => {
            let screen_point = with_app(|app| unsafe {
                if hwnd == app.list {
                    let point = if lparam == -1 {
                        let mut cursor: POINT = std::mem::zeroed();
                        GetCursorPos(&mut cursor);
                        cursor
                    } else {
                        POINT {
                            x: signed_loword(lparam as usize) as i32,
                            y: signed_hiword(lparam as usize) as i32,
                        }
                    };
                    let mut client = point;
                    ScreenToClient(app.list, &mut client);
                    app.select_result_at_point(client.x, client.y);
                    return Some(point);
                }
                None
            })
            .flatten();
            InvalidateRect(hwnd, null(), TRUE);
            UpdateWindow(hwnd);
            if let Some(point) = screen_point {
                show_result_context_menu_after_borrow(point);
            }
            return 0;
        }
        WM_RBUTTONUP => {
            return 0;
        }
        WM_MOUSELEAVE => {
            with_app(|app| unsafe {
                app.hide_app_tooltip();
                app.set_main_status(&app.default_status_text());
            });
        }
        WM_NCDESTROY => {
            with_app(|app| app.clear_result_drag());
            RemoveWindowSubclass(hwnd, Some(list_subclass_proc), subclass_id);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, message, wparam, lparam)
}

pub(crate) unsafe extern "system" fn settings_tooltip_subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    match message {
        WM_MOUSEMOVE => {
            let mut track: TRACKMOUSEEVENT = std::mem::zeroed();
            track.cbSize = std::mem::size_of::<TRACKMOUSEEVENT>() as u32;
            track.dwFlags = TME_LEAVE;
            track.hwndTrack = hwnd;
            TrackMouseEvent(&mut track);
            with_app(|app| unsafe {
                let mut point: POINT = std::mem::zeroed();
                if GetCursorPos(&mut point) != 0 {
                    app.update_settings_tooltip(hwnd, point.x, point.y);
                }
            });
        }
        WM_MOUSELEAVE | WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MOUSEWHEEL => {
            with_app(|app| unsafe { app.hide_app_tooltip() });
        }
        WM_NCDESTROY => {
            with_app(|app| unsafe { app.hide_app_tooltip() });
            RemoveWindowSubclass(
                hwnd,
                Some(settings_tooltip_subclass_proc),
                SETTINGS_TOOLTIP_SUBCLASS_ID,
            );
        }
        _ => {}
    }
    DefSubclassProc(hwnd, message, wparam, lparam)
}

pub(crate) unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if with_app(|app| message == app.taskbar_created_message && app.taskbar_created_message != 0)
        .unwrap_or(false)
    {
        with_app(|app| unsafe {
            app.tray_added = false;
            add_tray_icon(app);
        });
        return 0;
    }

    match message {
        WM_COMMAND => {
            let id = loword(wparam) as i32;
            let code = hiword(wparam);
            with_app(|app| {
                if id == ID_EDIT && code == EN_CHANGE as u16 {
                    unsafe {
                        if !app.ime_composing {
                            app.query_launch_rule_cursor = None;
                            app.schedule_refresh_results();
                        }
                    };
                } else if id == ID_LIST && code == LBN_DBLCLK as u16 {
                    unsafe { app.launch_selected() };
                } else if id == ID_LIST && code == LBN_SELCHANGE as u16 {
                    unsafe { app.sync_selected_result_from_list() };
                } else if id == ID_CONFIG_BUTTON && code == BN_CLICKED as u16 {
                    unsafe { app.show_config_window() };
                } else if id == ID_PLUGIN_HELP_BUTTON && code == BN_CLICKED as u16 {
                    unsafe { app.show_plugin_help() };
                }
            });
            0
        }
        WM_IME_STARTCOMPOSITION => {
            with_app(|app| app.ime_composing = true);
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_IME_ENDCOMPOSITION => {
            with_app(|app| unsafe {
                app.ime_composing = false;
                app.schedule_refresh_results();
            });
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_REFRESH_RESULTS => {
            with_app(|app| unsafe { app.refresh_results() });
            0
        }
        WM_REPAINT_RESULTS => {
            let list = with_app(|app| app.list);
            if let Some(list) = list {
                unsafe {
                    RedrawWindow(list, null(), null_mut(), RDW_INVALIDATE | RDW_FRAME);
                }
            }
            0
        }
        WM_INDEX_RELOAD_DONE => {
            let items = pending_index_slot()
                .lock()
                .ok()
                .and_then(|mut pending| pending.take());
            with_app(|app| unsafe {
                if let Some(items) = items {
                    app.finish_background_rebuild(items);
                } else {
                    app.reload_in_progress = false;
                }
            });
            0
        }
        WM_SEARCH_RESULTS_READY => {
            with_app(|app| unsafe { app.apply_search_results_batch() });
            0
        }
        WM_ICON_READY => {
            with_app(|app| unsafe { app.apply_icon_results() });
            0
        }
        WM_LAUNCH_RESULT_READY => {
            with_app(|app| unsafe { app.apply_launch_worker_results() });
            0
        }
        WM_PLUGIN_EVENT_READY => {
            with_app(|app| unsafe { app.apply_plugin_events() });
            0
        }
        WM_HELPER_STATUS_READY => {
            with_app(|app| app.apply_helper_status_results());
            0
        }
        WM_FILE_TASK_READY => {
            with_app(|app| unsafe { app.apply_file_task_results() });
            0
        }
        WM_CONFIG_RELOAD_READY => {
            with_app(|app| unsafe { app.apply_config_reload_results() });
            0
        }
        WM_ACTIVATEAPP => {
            if wparam != 0 {
                unsafe {
                    KillTimer(hwnd, HELPER_FOCUS_GRACE_TIMER_ID);
                }
                with_app(|app| unsafe {
                    app.launcher_app_active = true;
                    if IsWindowVisible(hwnd) != 0 {
                        app.ensure_search_worker_running();
                        app.begin_helper_focus_session();
                    }
                });
            } else {
                with_app(|app| app.launcher_app_active = false);
                unsafe {
                    if IsWindowVisible(hwnd) != 0 {
                        SetTimer(
                            hwnd,
                            HELPER_FOCUS_GRACE_TIMER_ID,
                            HELPER_FOCUS_GRACE_MS,
                            None,
                        );
                    } else {
                        KillTimer(hwnd, HELPER_FOCUS_GRACE_TIMER_ID);
                        PostMessageW(hwnd, WM_END_HELPER_SESSION, 0, 0);
                    }
                }
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_SHOWWINDOW => {
            if wparam != 0 {
                KillTimer(hwnd, POPUP_CLEANUP_TIMER_ID);
                KillTimer(hwnd, HELPER_FOCUS_GRACE_TIMER_ID);
                with_app(|app| {
                    set_window_text(
                        hwnd,
                        &window_title_text(
                            app.language,
                            app.show_cpu_in_title,
                            app.show_ram_in_title,
                            app.show_build_timestamp_in_title,
                        ),
                    );
                });
                SetTimer(hwnd, TITLE_TIMER_ID, TITLE_TIMER_MS, None);
            } else {
                KillTimer(hwnd, TITLE_TIMER_ID);
                KillTimer(hwnd, HELPER_FOCUS_GRACE_TIMER_ID);
                with_app(|app| {
                    app.clear_pending_launch();
                    app.plugin_supervisor.cancel_all(app.search_generation);
                    app.release_show_all_results_on_hide();
                });
                PostMessageW(hwnd, WM_END_HELPER_SESSION, 0, 0);
                SetTimer(hwnd, POPUP_CLEANUP_TIMER_ID, POPUP_CLEANUP_GRACE_MS, None);
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_END_HELPER_SESSION => {
            unsafe {
                KillTimer(hwnd, HELPER_FOCUS_GRACE_TIMER_ID);
            }
            let window_visible = unsafe { IsWindowVisible(hwnd) } != 0;
            with_app(|app| unsafe {
                if should_end_helper_session(window_visible, app.launcher_app_active) {
                    app.release_inactive_launcher_memory();
                }
            });
            0
        }
        WM_TIMER => {
            if wparam == TITLE_TIMER_ID {
                with_app(|app| {
                    set_window_text(
                        hwnd,
                        &window_title_text(
                            app.language,
                            app.show_cpu_in_title,
                            app.show_ram_in_title,
                            app.show_build_timestamp_in_title,
                        ),
                    )
                });
                0
            } else if wparam == SEARCH_REFRESH_TIMER_ID {
                unsafe {
                    KillTimer(hwnd, SEARCH_REFRESH_TIMER_ID);
                }
                with_app(|app| unsafe { app.refresh_results() });
                0
            } else if wparam == POPUP_CLEANUP_TIMER_ID {
                KillTimer(hwnd, POPUP_CLEANUP_TIMER_ID);
                with_app(|app| unsafe { app.cleanup_after_popup_hidden() });
                0
            } else if wparam == HELPER_FOCUS_GRACE_TIMER_ID {
                KillTimer(hwnd, HELPER_FOCUS_GRACE_TIMER_ID);
                PostMessageW(hwnd, WM_END_HELPER_SESSION, 0, 0);
                0
            } else {
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
        }
        WM_HOTKEY => {
            with_app(|app| unsafe { app.toggle_launcher() });
            0
        }
        WM_TRAYICON => {
            let tray_event = loword(lparam as usize) as u32;
            if tray_event == WM_LBUTTONUP {
                with_app(|app| unsafe { app.toggle_launcher() });
            } else if tray_event == WM_RBUTTONUP || tray_event == WM_CONTEXTMENU {
                unsafe { show_tray_menu_after_borrow() };
            }
            0
        }
        WM_SHOW_SETTINGS => {
            with_app(|app| unsafe {
                if wparam == usize::MAX {
                    app.show_config_window();
                } else {
                    app.show_config_window_for_page(settings_page_from_nav_index(wparam as i32));
                }
            });
            0
        }
        WM_SETTINGS_CHANGED => {
            with_app(|app| unsafe { app.reload_settings_from_disk() });
            0
        }
        WM_CONTEXTMENU => {
            let point = with_app(|app| unsafe {
                if wparam as HWND != app.list {
                    return None;
                }

                let point = if lparam == -1 {
                    let mut cursor: POINT = std::mem::zeroed();
                    GetCursorPos(&mut cursor);
                    cursor
                } else {
                    POINT {
                        x: signed_loword(lparam as usize) as i32,
                        y: signed_hiword(lparam as usize) as i32,
                    }
                };

                let mut client = point;
                ScreenToClient(app.list, &mut client);
                app.select_result_at_point(client.x, client.y);
                Some(point)
            })
            .flatten();
            if let Some(point) = point {
                unsafe { show_result_context_menu_after_borrow(point) };
                0
            } else {
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
        }
        WM_CLOSE => {
            save_window_settings(hwnd);
            unsafe {
                KillTimer(hwnd, HELPER_FOCUS_GRACE_TIMER_ID);
                ShowWindow(hwnd, SW_HIDE);
                PostMessageW(hwnd, WM_END_HELPER_SESSION, 0, 0);
            }
            0
        }
        WM_SIZE => {
            if wparam as u32 != SIZE_MINIMIZED {
                with_app(|app| unsafe { app.resize_controls() });
            }
            0
        }
        WM_DRAWITEM => {
            let handled = with_app(|app| unsafe {
                let draw = lparam as *const DRAWITEMSTRUCT;
                if draw.is_null() {
                    return false;
                }
                match (*draw).CtlID as i32 {
                    ID_LIST => {
                        app.draw_result_item(&*draw);
                        true
                    }
                    ID_STATUS => {
                        app.draw_status_item(&*draw);
                        true
                    }
                    _ => false,
                }
            })
            .unwrap_or(false);
            if handled {
                TRUE as isize
            } else {
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
        }
        WM_GETMINMAXINFO => {
            let info = lparam as *mut MINMAXINFO;
            if !info.is_null() {
                unsafe {
                    (*info).ptMinTrackSize.x = MIN_WIDTH;
                    (*info).ptMinTrackSize.y = MIN_HEIGHT;
                }
            }
            0
        }
        WM_SETFOCUS => {
            with_app(|app| unsafe { SetFocus(app.edit) });
            0
        }
        WM_DESTROY => {
            save_window_settings(hwnd);
            with_app(|app| unsafe {
                remove_tray_icon(app);
                app.end_helper_focus_session();
                app.request_background_shutdown();
                app.cleanup_fonts();
                app.cleanup_icon_cache();
                if !app.result_tooltip.is_null() {
                    DestroyWindow(app.result_tooltip);
                    app.result_tooltip = null_mut();
                }
                if !app.owner_hwnd.is_null() {
                    DestroyWindow(app.owner_hwnd);
                    app.owner_hwnd = null_mut();
                }
            });
            unsafe {
                KillTimer(hwnd, TITLE_TIMER_ID);
                KillTimer(hwnd, SEARCH_REFRESH_TIMER_ID);
                KillTimer(hwnd, HELPER_FOCUS_GRACE_TIMER_ID);
                UnregisterHotKey(hwnd, HOTKEY_ID);
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

pub(crate) unsafe extern "system" fn config_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_SETTINGS_SHOW_PAGE => {
            with_app(|app| unsafe {
                if wparam != usize::MAX {
                    app.show_config_window_for_page(settings_page_from_nav_index(wparam as i32));
                }
                ShowWindow(hwnd, SW_RESTORE);
                ShowWindow(hwnd, SW_SHOW);
                SetForegroundWindow(hwnd);
            });
            0
        }

        WM_CONFIG_RELOAD_READY => {
            with_app(|app| unsafe { app.apply_config_reload_results() });
            0
        }
        WM_CONFIG_SAVE_READY => {
            with_app(|app| unsafe { app.apply_config_save_results() });
            0
        }
        WM_REPAINT_SETTINGS => unsafe {
            RedrawWindow(hwnd, null(), null_mut(), SETTINGS_REPAINT_FLAGS);
            0
        },
        WM_COMMAND => {
            let id = loword(wparam) as i32;
            let code = hiword(wparam);
            with_app(|app| {
                if id == ID_CFG_HOTKEY && code == EN_SETFOCUS as u16 {
                    unsafe { app.start_hotkey_capture() };
                    return;
                }
                if id == ID_CFG_NAV
                    && (code == LBN_SELCHANGE as u16 || code == LBN_DBLCLK as u16)
                {
                    unsafe { app.select_config_page_from_nav() };
                    return;
                }
                if id == ID_CFG_SEARCH_THREAD_MODE && code == CBN_SELCHANGE as u16 {
                    unsafe { app.apply_search_thread_mode_selection() };
                    return;
                }
                if id == ID_CFG_LANGUAGE && code == CBN_SELCHANGE as u16 {
                    unsafe { app.apply_language_selection_from_gui() };
                    return;
                }
                if id == ID_CFG_QUERY_LAUNCH_FILTER
                    && code == EN_CHANGE as u16
                    && !app.loading_config_entry
                {
                    unsafe { app.apply_query_launch_rules_filter() };
                    return;
                }
                if id == ID_SCORING_ENABLED && code == BN_CLICKED as u16 {
                    unsafe { app.toggle_selected_scoring_rule() };
                    return;
                }
                if id == ID_CFG_RESULT_LIMIT
                    && code == EN_CHANGE as u16
                    && !app.loading_config_entry
                {
                    return;
                }
                if id == ID_CFG_SOUND && code == BN_CLICKED as u16 && !app.loading_config_entry {
                    return;
                }
                if id == ID_CFG_SCORE_BREAKDOWN_TOOLTIP
                    && code == BN_CLICKED as u16
                    && !app.loading_config_entry
                {
                    unsafe { app.apply_score_breakdown_tooltip_controls_visibility() };
                    return;
                }
                match id {
                    ID_CFG_APPLY_ENTRY => unsafe { app.show_add_index_window() },
                    ID_CFG_TOGGLE_ENTRY => unsafe { app.toggle_selected_index_entry() },
                    ID_CFG_RESET_DEFAULT => unsafe { app.reset_selected_default_entry() },
                    ID_CFG_GENERAL_RESET => unsafe { app.reset_general_settings_to_defaults() },
                    ID_CFG_DELETE_ENTRY => unsafe { app.delete_selected_index_entry() },
                    ID_CFG_MOVE_UP => unsafe { app.move_selected_index_entry(-1) },
                    ID_CFG_MOVE_DOWN => unsafe { app.move_selected_index_entry(1) },
                    ID_CFG_CLEAR_HISTORY => unsafe { app.clear_history_from_config_page() },
                    ID_CFG_MODIFIER_HELP_BUTTON => unsafe { app.show_modifier_keyword_help() },
                    ID_SCORING_ADD => unsafe { app.add_scoring_pattern_rule() },
                    ID_SCORING_DELETE => unsafe { app.delete_selected_scoring_rule() },
                    ID_SCORING_MOVE_UP => unsafe { app.move_selected_scoring_rule(-1) },
                    ID_SCORING_MOVE_DOWN => unsafe { app.move_selected_scoring_rule(1) },
                    ID_SCORING_RESET => unsafe { app.reset_selected_scoring_rule_to_default() },
                    ID_CFG_SAVE => unsafe {
                        app.save_config_from_gui();
                    },
                    ID_CFG_OPEN_FOLDER => {
                        let config_dir = app_config_dir();
                        required_file_operation("CONFIG", &config_dir, "create directory", || {
                            fs::create_dir_all(&config_dir)
                        });
                        let error = shell_open_path_result(app.config_hwnd, &config_dir).err();
                        if let Some(error) = error {
                            show_error(
                                app.config_hwnd,
                                &localized_format2(
                                    app.language,
                                    "Could not open folder.\n\nPath:\n{}\n\nReason:\n{}",
                                    config_dir.to_string_lossy(),
                                    error,
                                ),
                            );
                        }
                    }
                    ID_CFG_CLOSE => unsafe {
                        PostMessageW(hwnd, WM_CLOSE, 0, 0);
                    },
                    _ => {}
                }
            });
            0
        }
        WM_HSCROLL => {
            with_app(|app| {
                if lparam as HWND == app.cfg_tooltip_opacity {
                    unsafe { app.update_tooltip_opacity_value_label() };
                }
            });
            0
        }
        WM_NOTIFY => {
            let hdr = lparam as *const NMHDR;
            if hdr.is_null() {
                return 0;
            }
            unsafe {
                if (*hdr).code == NM_CUSTOMDRAW {
                    let list_hwnd = (*hdr).hwndFrom;
                    let cfg_index = GetDlgItem(hwnd, ID_CFG_INDEX);
                    let scoring_list = GetDlgItem(hwnd, ID_SCORING_LIST);
                    let is_settings_list = (!cfg_index.is_null() && list_hwnd == cfg_index)
                        || (!scoring_list.is_null() && list_hwnd == scoring_list);
                    if !is_settings_list {
                        return 0;
                    }

                    let cd = lparam as *mut NMLVCUSTOMDRAW;
                    if cd.is_null() {
                        return 0;
                    }
                    let stage = (*cd).nmcd.dwDrawStage;
                    if stage == CDDS_PREPAINT {
                        return CDRF_NOTIFYITEMDRAW as isize;
                    }
                    if stage == CDDS_ITEMPREPAINT {
                        return CDRF_NOTIFYSUBITEMDRAW as isize;
                    }
                    if stage == (CDDS_ITEMPREPAINT | CDDS_SUBITEM) {
                        const READONLY_DEFAULT_BLUE: u32 = 0x00FF_0000;

                        let row = (*cd).nmcd.dwItemSpec;
                        let subitem = (*cd).iSubItem as usize;
                        let cell_style = crate::settings_model::decode_cell_style(
                            list_view_item_param(list_hwnd, row),
                            subitem,
                        );
                        let selected =
                            (SendMessageW(list_hwnd, LVM_GETITEMSTATE, row, LVIS_SELECTED as isize)
                                as u32
                                & LVIS_SELECTED)
                                != 0;
                        if cell_style == crate::settings_model::CellStyle::ReadonlyDefault {
                            (*cd).clrText = READONLY_DEFAULT_BLUE;
                            SetTextColor((*cd).nmcd.hdc, READONLY_DEFAULT_BLUE);
                        } else if !selected {
                            let default_text_color = GetSysColor(COLOR_WINDOWTEXT);
                            (*cd).clrText = default_text_color;
                            SetTextColor((*cd).nmcd.hdc, default_text_color);
                        }
                        let font =
                            if cell_style == crate::settings_model::CellStyle::ModifiedEditable {
                                with_app(|app| {
                                    app.ensure_fonts();
                                    if !app.list_bold_font.is_null() {
                                        app.list_bold_font as HGDIOBJ
                                    } else {
                                        list_view_font_or_default(list_hwnd)
                                    }
                                })
                                .unwrap_or_else(|| list_view_font_or_default(list_hwnd))
                            } else {
                                list_view_font_or_default(list_hwnd)
                            };
                        if !font.is_null() {
                            SelectObject((*cd).nmcd.hdc, font);
                        }
                        return CDRF_NEWFONT as isize;
                    }
                    return 0;
                }
            }
            let handled = with_app(|app| unsafe {
                if (*hdr).code == NM_DBLCLK {
                    let nm = lparam as *const NMITEMACTIVATE;
                    let item = (*nm).iItem;
                    let subitem = (*nm).iSubItem;
                    if item >= 0
                        && subitem >= 0
                        && ((*hdr).hwndFrom == app.cfg_index || (*hdr).hwndFrom == app.scoring_list)
                    {
                        app.start_inline_edit((*hdr).hwndFrom, item, subitem);
                        return true;
                    }
                }

                if (*hdr).code != LVN_ITEMCHANGED {
                    return false;
                }

                let notification = lparam as *const NMLISTVIEW;
                if notification.is_null() || (*notification).iItem < 0 {
                    return false;
                }
                let selection_changed = ((*notification).uChanged & LVIF_STATE) != 0
                    && (((*notification).uOldState ^ (*notification).uNewState) & LVIS_SELECTED)
                        != 0;
                let selected_changed = ((*notification).uChanged & LVIF_STATE) != 0
                    && (((*notification).uOldState ^ (*notification).uNewState)
                        & (LVIS_SELECTED | LVIS_FOCUSED))
                        != 0
                    && ((*notification).uNewState & LVIS_SELECTED) != 0;
                let check_changed = ((*notification).uChanged & LVIF_STATE) != 0
                    && (((*notification).uOldState ^ (*notification).uNewState)
                        & LVIS_STATEIMAGEMASK)
                        != 0;

                if (*hdr).hwndFrom == app.cfg_index {
                    if app.settings_page == SettingsPage::QueryLaunchRules && selection_changed {
                        app.update_query_launch_rules_selection_count();
                    }
                    if selected_changed {
                        match app.settings_page {
                            SettingsPage::QueryLaunchRules => {
                                app.load_selected_query_launch_rule_entry()
                            }
                            SettingsPage::PluginAliases => app.load_selected_plugin_alias_entry(),
                            _ => app.load_selected_index_entry(),
                        }
                    }
                    if check_changed {
                        let checked =
                            ((*notification).uNewState & LVIS_STATEIMAGEMASK) == LV_CHECKED_STATE;
                        app.sync_index_check_from_list_view((*notification).iItem, checked);
                    }
                    return true;
                }

                if (*hdr).hwndFrom == app.scoring_list {
                    if selected_changed {
                        app.load_selected_scoring_rule();
                    }
                    if check_changed {
                        let checked =
                            ((*notification).uNewState & LVIS_STATEIMAGEMASK) == LV_CHECKED_STATE;
                        app.sync_scoring_check_from_list_view((*notification).iItem, checked);
                    }
                    return true;
                }

                false
            })
            .unwrap_or(false);
            if handled {
                0
            } else {
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
        }
        WM_CLOSE => {
            let close_process = with_app(|app| unsafe {
                if app.inline_edit_hwnd != 0 as HWND {
                    app.finish_inline_edit(app.inline_edit_hwnd, true);
                    if app.inline_edit_hwnd != 0 as HWND {
                        return false;
                    }
                }
                if app.config_has_unsaved_changes() {
                    let message = wide(localized(
                        app.language,
                        "Settings changed but not saved. Save now?",
                    ));
                    let answer = MessageBoxW(
                        hwnd,
                        message.as_ptr(),
                        wide(APP_NAME).as_ptr(),
                        MB_YESNOCANCEL | MB_ICONQUESTION,
                    );
                    if answer == IDCANCEL {
                        return false;
                    }
                    if answer == IDYES {
                        if !app.save_config_from_gui() {
                            return false;
                        }
                        app.config_close_after_save = true;
                        return false;
                    }
                    if answer == IDNO {
                        app.restore_config_snapshot();
                    }
                }
                save_config_window_settings(hwnd);
                if !app.add_index_hwnd.is_null() {
                    save_add_index_window_settings(app.add_index_hwnd);
                    if app.settings_process_mode {
                        DestroyWindow(app.add_index_hwnd);
                    } else {
                        ShowWindow(app.add_index_hwnd, SW_HIDE);
                    }
                }
                if app.settings_process_mode {
                    true
                } else {
                    ShowWindow(hwnd, SW_HIDE);
                    false
                }
            })
            .unwrap_or(false);
            if close_process {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_SIZE => {
            with_app(|app| unsafe { app.resize_config_controls() });
            0
        }
        WM_GETMINMAXINFO => {
            let info = lparam as *mut MINMAXINFO;
            if !info.is_null() {
                unsafe {
                    (*info).ptMinTrackSize.x = SETTINGS_MIN_WIDTH;
                    (*info).ptMinTrackSize.y = SETTINGS_MIN_HEIGHT;
                }
            }
            0
        }
        WM_ERASEBKGND => unsafe {
            let mut rect: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rect);
            FillRect(wparam as _, &rect, GetSysColorBrush(COLOR_WINDOW));
            TRUE as isize
        },
        WM_PAINT => unsafe {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut paint);
            let mut rect: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rect);
            FillRect(hdc, &rect, GetSysColorBrush(COLOR_WINDOW));
            EndPaint(hwnd, &paint);
            0
        },
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => unsafe {
            SetBkMode(wparam as _, TRANSPARENT as i32);
            GetSysColorBrush(COLOR_WINDOW) as isize
        },
        WM_VSCROLL => {
            let request = loword(wparam) as i32;
            with_app(|app| unsafe {
                match request {
                    SB_LINEUP => app.scroll_config_by(-32),
                    SB_LINEDOWN => app.scroll_config_by(32),
                    SB_PAGEUP => app.scroll_config_by(-240),
                    SB_PAGEDOWN => app.scroll_config_by(240),
                    SB_THUMBTRACK | SB_THUMBPOSITION => {
                        let mut scroll_info: SCROLLINFO = std::mem::zeroed();
                        scroll_info.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
                        scroll_info.fMask = SIF_TRACKPOS;
                        if GetScrollInfo(hwnd, SB_VERT, &mut scroll_info) != 0 {
                            app.set_config_scroll(scroll_info.nTrackPos);
                        }
                    }
                    SB_TOP => app.set_config_scroll(0),
                    SB_BOTTOM => app.set_config_scroll(app.config_content_height),
                    _ => {}
                }
            });
            0
        }
        WM_MOUSEWHEEL => {
            let delta = signed_hiword(wparam) as i32;
            with_app(|app| unsafe {
                app.hide_app_tooltip();
                app.scroll_config_by(-(delta / 120) * 96);
            });
            0
        }
        WM_DESTROY => {
            save_config_window_settings(hwnd);
            let settings_process_mode = with_app(|app| {
                let settings_process_mode = app.settings_process_mode;
                if app.config_hwnd == hwnd {
                    app.config_hwnd = null_mut();
                    app.cfg_hotkey = null_mut();
                    app.cfg_index = null_mut();
                    app.cfg_result_limit = null_mut();
                    app.cfg_search_thread_mode = null_mut();
                    app.cfg_search_threads = null_mut();
                    app.cfg_language = null_mut();
                    app.cfg_sound = null_mut();
                    app.cfg_status = null_mut();
                    app.cfg_modifier_help_button = null_mut();
                    app.scoring_list = null_mut();
                    app.scoring_status = null_mut();
                }
                settings_process_mode
            })
            .unwrap_or(false);
            if settings_process_mode {
                PostQuitMessage(0);
            }
            0
        }
        msg if msg == WM_INLINE_EDIT_FINISH => {
            let edit_hwnd = wparam as HWND;
            let save = lparam == 1;
            with_app(|app| unsafe {
                app.finish_inline_edit(edit_hwnd, save);
            });
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

pub(crate) unsafe extern "system" fn add_index_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND => {
            let id = loword(wparam) as i32;
            with_app(|app| match id {
                ID_ADD_BROWSE => unsafe { app.browse_add_index_folder() },
                ID_ADD_OK => unsafe { app.add_index_entry_from_dialog() },
                ID_ADD_MODIFIER_HELP_BUTTON => unsafe {
                    app.show_modifier_keyword_help_for_owner(hwnd)
                },
                ID_ADD_PATH => {
                    let code = hiword(wparam);
                    if matches!(
                        code,
                        value
                            if value == CBN_SELCHANGE as u16
                                || value == CBN_SELENDOK as u16
                                || value == CBN_CLOSEUP as u16
                    ) {
                        unsafe { PostMessageW(hwnd, WM_APPLY_ADD_ALIAS, 0, 0) };
                    } else if matches!(code, value if value == CBN_EDITCHANGE as u16 || value == CBN_EDITUPDATE as u16)
                    {
                        unsafe { app.update_add_alias_hint() };
                    }
                    if code == CBN_EDITCHANGE as u16 && get_window_text(app.add_path).trim() == "%"
                    {
                        unsafe { SendMessageW(app.add_path, CB_SHOWDROPDOWN, TRUE as usize, 0) };
                    }
                }
                ID_ADD_CANCEL => unsafe {
                    save_add_index_window_settings(hwnd);
                    ShowWindow(hwnd, SW_HIDE);
                },
                _ => {}
            });
            0
        }
        WM_CLOSE => {
            save_add_index_window_settings(hwnd);
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => unsafe {
            SetBkMode(wparam as _, TRANSPARENT as i32);
            GetSysColorBrush(COLOR_WINDOW) as isize
        },
        WM_APPLY_ADD_ALIAS => {
            with_app(|app| unsafe { app.select_add_alias() });
            0
        }
        WM_SIZE => {
            with_app(|app| unsafe { app.resize_add_index_controls() });
            0
        }
        WM_GETMINMAXINFO => {
            let info = lparam as *mut MINMAXINFO;
            if !info.is_null() {
                unsafe {
                    (*info).ptMinTrackSize.x = ADD_MIN_WIDTH;
                    (*info).ptMinTrackSize.y = ADD_MIN_HEIGHT;
                }
            }
            0
        }
        WM_DESTROY => {
            save_add_index_window_settings(hwnd);
            with_app(|app| {
                if app.add_index_hwnd == hwnd {
                    app.add_index_hwnd = null_mut();
                    app.add_path = null_mut();
                    app.add_score = null_mut();
                    app.add_depth = null_mut();
                    app.add_label = null_mut();
                    app.add_keywords = null_mut();
                    app.add_enabled = null_mut();
                    app.add_status = null_mut();
                    app.add_modifier_help_button = null_mut();
                }
            });
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::should_end_helper_session;

    #[test]
    fn helper_session_close_policy_preserves_active_visible_window() {
        assert!(!should_end_helper_session(true, true));
        assert!(should_end_helper_session(true, false));
        assert!(should_end_helper_session(false, true));
        assert!(should_end_helper_session(false, false));
    }
}

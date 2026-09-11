use std::path::PathBuf;
use std::ptr::{copy_nonoverlapping, null, null_mut};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::*;

/// Measure localized labels using the control's actual font without showing a window.
pub(crate) unsafe fn control_text_width(hwnd: HWND, text: &str) -> i32 {
    let dc = GetDC(hwnd);
    if dc.is_null() {
        return 0;
    }
    let font = SendMessageW(hwnd, WM_GETFONT, 0, 0) as HGDIOBJ;
    let previous = if font.is_null() {
        null_mut()
    } else {
        SelectObject(dc, font)
    };
    let text = wide(text);
    let mut size: SIZE = std::mem::zeroed();
    GetTextExtentPoint32W(dc, text.as_ptr(), (text.len() - 1) as i32, &mut size);
    if !previous.is_null() {
        SelectObject(dc, previous);
    }
    ReleaseDC(hwnd, dc);
    size.cx
}

pub(crate) fn result_target_text(result: &SearchResult) -> Option<String> {
    match &result.target {
        LaunchTarget::Path(path) => {
            if is_shortcut_file(path) {
                if let Some(target) = resolve_shortcut_target(path) {
                    return Some(target.to_string_lossy().to_string());
                }
            }
            Some(path.to_string_lossy().to_string())
        }
        LaunchTarget::Plugin(_) => None,
    }
}

pub(crate) fn visible_results_signature(
    results: &[SearchResult],
    limit: usize,
) -> Vec<VisibleResultSignature> {
    results
        .iter()
        .take(limit)
        .map(|result| VisibleResultSignature {
            title: result.title.clone(),
            subtitle: result.subtitle.clone(),
            target: result_target_text(result).unwrap_or_else(|| plugin_result_signature(result)),
            is_dir: result.is_dir,
            from_history: result.from_history,
            from_query_launch_rule: result.from_query_launch_rule,
            ranking_kind: result.ranking_kind,
            display_score: result.display_score,
            score_detail: result.score_detail.clone(),
            score: result.score,
        })
        .collect()
}

pub(crate) fn result_score_label(result: &SearchResult) -> String {
    if result.from_query_launch_rule {
        "*".to_string()
    } else {
        result.display_score.to_string()
    }
}

#[cfg(test)]
pub(crate) fn result_score_breakdown_tooltip_text(result: &SearchResult) -> Option<&str> {
    result_score_breakdown_tooltip_text_for_setting(result, true)
}

pub(crate) fn result_score_breakdown_tooltip_text_for_setting(
    result: &SearchResult,
    show_score_breakdown_tooltip: bool,
) -> Option<&str> {
    if !show_score_breakdown_tooltip {
        return None;
    }
    let detail = result.score_detail.trim();
    if result.is_alias_result() || detail.is_empty() {
        return None;
    }
    if matches!(detail, "recent order" | "starred" | "plugin") {
        return None;
    }
    (detail.contains(" + ") && detail.contains(" = ")).then_some(detail)
}

pub(crate) fn result_detail_text(result: &SearchResult, show_score_breakdown: bool) -> String {
    if result.is_alias_result() || result.score_detail == "recent order" {
        return result.subtitle.clone();
    }
    let breakdown = if show_score_breakdown && !result.score_detail.is_empty() {
        format!(" ({})", result.score_detail)
    } else {
        String::new()
    };
    format!(
        "{} - score {}{}",
        result.subtitle,
        result_score_label(result),
        breakdown
    )
}

pub(crate) fn result_list_display_text(
    index: usize,
    result: &SearchResult,
    show_score_breakdown: bool,
) -> String {
    if result.is_alias_result() {
        format!("{}    -    {}", result.title, result.subtitle)
    } else if result.score_detail == "recent order" {
        format!(
            "{:>2}.  {}    -    {}",
            index + 1,
            result.title,
            result.subtitle
        )
    } else {
        let breakdown = if show_score_breakdown && !result.score_detail.is_empty() {
            format!(" ({})", result.score_detail)
        } else {
            String::new()
        };
        format!(
            "{:>2}.  {}    -    {}    [score {}{}]",
            index + 1,
            result.title,
            result.subtitle,
            result_score_label(result),
            breakdown
        )
    }
}

pub(crate) fn plugin_result_signature(result: &SearchResult) -> String {
    format!(
        "plugin:{}:{}:{}",
        result.title, result.subtitle, result.score
    )
}

pub(crate) fn browse_for_folder(owner: HWND, language: AppLanguage) -> Option<PathBuf> {
    crate::shell_helper::run_browse_folder_direct(
        owner,
        localized(language, "Choose a folder to search"),
    )
}

pub(crate) fn set_clipboard_text(owner: HWND, text: &str) -> bool {
    unsafe {
        let data = wide(text);
        let bytes = data.len() * std::mem::size_of::<u16>();
        let memory = GlobalAlloc(GHND, bytes);
        if memory.is_null() {
            return false;
        }

        let target = GlobalLock(memory) as *mut u16;
        if target.is_null() {
            GlobalFree(memory);
            return false;
        }
        copy_nonoverlapping(data.as_ptr(), target, data.len());
        GlobalUnlock(memory);

        if OpenClipboard(owner) == 0 {
            GlobalFree(memory);
            return false;
        }
        EmptyClipboard();
        let transferred = SetClipboardData(CF_UNICODETEXT_ID, memory);
        CloseClipboard();
        if transferred.is_null() {
            GlobalFree(memory);
            return false;
        }
        true
    }
}

pub(crate) fn load_app_icon(size: i32) -> HICON {
    let icon_path = app_dir().join(APP_ICON_FILE);
    if icon_path.exists() {
        let icon_path = os_wide(icon_path.as_os_str());
        let icon = unsafe {
            LoadImageW(
                null_mut(),
                icon_path.as_ptr(),
                IMAGE_ICON,
                size,
                size,
                LR_LOADFROMFILE | LR_DEFAULTSIZE,
            ) as HICON
        };
        if !icon.is_null() {
            return icon;
        }
    }
    unsafe { LoadIconW(null_mut(), IDI_APPLICATION) }
}

pub(crate) fn show_error(hwnd: HWND, message: &str) {
    let title = wide(APP_NAME);
    let message = wide(message);
    unsafe {
        MessageBoxW(hwnd, message.as_ptr(), title.as_ptr(), MB_ICONERROR | MB_OK);
    }
}

fn colorref(red: u8, green: u8, blue: u8) -> COLORREF {
    red as COLORREF | ((green as COLORREF) << 8) | ((blue as COLORREF) << 16)
}

struct HelpWindowState {
    content: HWND,
    font_edit: HWND,
    font: HFONT,
}

unsafe fn create_help_content_font(size: i32) -> HFONT {
    CreateFontW(
        -size.clamp(MIN_HELP_FONT_SIZE, MAX_HELP_FONT_SIZE),
        0,
        0,
        0,
        FW_NORMAL as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        CLIP_DEFAULT_PRECIS as u32,
        CLEARTYPE_QUALITY as u32,
        FIXED_PITCH as u32 | FF_MODERN as u32,
        wide("Cascadia Mono").as_ptr(),
    )
}

unsafe fn help_window_state(hwnd: HWND) -> Option<&'static mut HelpWindowState> {
    let state = get_window_long(hwnd, GWLP_USERDATA) as *mut HelpWindowState;
    (!state.is_null()).then(|| &mut *state)
}

unsafe fn layout_help_window(hwnd: HWND, width: i32, height: i32) {
    MoveWindow(GetDlgItem(hwnd, HELP_FONT_LABEL_ID), 15, 12, 118, 22, TRUE);
    MoveWindow(GetDlgItem(hwnd, HELP_FONT_EDIT_ID), 135, 8, 64, 26, TRUE);
    MoveWindow(GetDlgItem(hwnd, HELP_FONT_SPIN_ID), 200, 8, 22, 26, TRUE);
    let content_top = 44;
    let content = GetDlgItem(hwnd, HELP_CONTENT_ID);
    if !content.is_null() {
        MoveWindow(
            content,
            15,
            content_top,
            (width - 30).max(0),
            (height - content_top - 15).max(0),
            TRUE,
        );
    }
}

unsafe fn apply_help_font_size(hwnd: HWND, size: i32) {
    let size = size.clamp(MIN_HELP_FONT_SIZE, MAX_HELP_FONT_SIZE);
    let Some(state) = help_window_state(hwnd) else {
        return;
    };
    let size_text = size.to_string();
    if get_window_text(state.font_edit).trim() != size_text {
        set_window_text(state.font_edit, &size_text);
    }
    let next_font = create_help_content_font(size);
    if next_font.is_null() {
        return;
    }
    let old_font = state.font;
    state.font = next_font;
    SendMessageW(state.content, WM_SETFONT, next_font as usize, TRUE as isize);
    if !old_font.is_null() {
        DeleteObject(old_font as _);
    }
    let _ = save_help_font_size(size);
}

pub(crate) unsafe extern "system" fn help_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SIZE => {
            let width = loword(lparam as usize) as i32;
            let height = hiword(lparam as usize) as i32;
            layout_help_window(hwnd, width, height);
            0
        }
        WM_COMMAND => {
            let id = loword(wparam) as i32;
            let code = hiword(wparam);
            if id == HELP_FONT_EDIT_ID && code == EN_CHANGE as u16 {
                let value = get_window_text(GetDlgItem(hwnd, HELP_FONT_EDIT_ID));
                if !value.trim().is_empty() {
                    apply_help_font_size(hwnd, parse_help_font_size(&value));
                }
                return 0;
            }
            0
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC => {
            let hdc = wparam as HDC;
            SetTextColor(hdc, colorref(20, 70, 140));
            SetBkColor(hdc, colorref(255, 255, 255));
            GetStockObject(WHITE_BRUSH) as LRESULT
        }
        WM_DESTROY => {
            save_help_window_settings(hwnd);
            0
        }
        WM_NCDESTROY => {
            let state = get_window_long(hwnd, GWLP_USERDATA) as *mut HelpWindowState;
            if !state.is_null() {
                set_window_long(hwnd, GWLP_USERDATA, 0);
                let state = Box::from_raw(state);
                if !state.font.is_null() {
                    DeleteObject(state.font as _);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub(crate) fn show_help_form(owner: HWND, title: &str, content: &str, language: AppLanguage) {
    unsafe {
        let class_name = wide("FlashLaunchHelpForm");
        let title_text = wide(title);
        let h_instance = GetModuleHandleW(null_mut());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        if GetClassInfoW(h_instance, class_name.as_ptr(), &mut wc) == 0 {
            wc.lpfnWndProc = Some(help_window_proc);
            wc.hInstance = h_instance;
            wc.lpszClassName = class_name.as_ptr();
            wc.hbrBackground = (COLOR_WINDOW + 1) as HBRUSH;
            RegisterClassW(&wc);
        }

        let existing = FindWindowW(class_name.as_ptr(), title_text.as_ptr());
        if !existing.is_null() {
            let content_hwnd = GetDlgItem(existing, HELP_CONTENT_ID);
            if !content_hwnd.is_null() {
                set_window_text(content_hwnd, content);
                SendMessageW(content_hwnd, EM_SETSEL, 0, 0);
            }
            if IsIconic(existing) != 0 {
                ShowWindow(existing, SW_RESTORE);
            }
            ShowWindow(existing, SW_SHOW);
            SetForegroundWindow(existing);
            return;
        }

        let settings = load_help_window_settings();
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_APPWINDOW,
            class_name.as_ptr(),
            title_text.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            settings.x,
            settings.y,
            settings.width,
            settings.height,
            owner,
            null_mut(),
            h_instance,
            null_mut(),
        );

        let label = CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            wide(localized(language, "Font size")).as_ptr(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            0,
            0,
            hwnd,
            HELP_FONT_LABEL_ID as isize as HMENU,
            h_instance,
            null_mut(),
        );
        let font_edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            wide("EDIT").as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32 | ES_NUMBER as u32,
            0,
            0,
            0,
            0,
            hwnd,
            HELP_FONT_EDIT_ID as isize as HMENU,
            h_instance,
            null_mut(),
        );
        let spin = CreateWindowExW(
            0,
            UPDOWN_CLASSW,
            null(),
            WS_CHILD | WS_VISIBLE | UDS_ARROWKEYS | UDS_SETBUDDYINT,
            0,
            0,
            0,
            0,
            hwnd,
            HELP_FONT_SPIN_ID as isize as HMENU,
            h_instance,
            null_mut(),
        );
        let edit = CreateWindowExW(
            0,
            wide("EDIT").as_ptr(),
            wide(content).as_ptr(),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | ES_MULTILINE as u32
                | ES_READONLY as u32
                | ES_AUTOVSCROLL as u32,
            0,
            0,
            0,
            0,
            hwnd,
            HELP_CONTENT_ID as isize as HMENU,
            h_instance,
            null_mut(),
        );
        let gui_font = GetStockObject(DEFAULT_GUI_FONT);
        for child in [label, font_edit, spin] {
            SendMessageW(child, WM_SETFONT, gui_font as usize, TRUE as isize);
        }
        let font_size = load_help_font_size();
        set_window_text(font_edit, &font_size.to_string());
        SendMessageW(spin, UDM_SETBUDDY, font_edit as usize, 0);
        SendMessageW(
            spin,
            UDM_SETRANGE32,
            MIN_HELP_FONT_SIZE as usize,
            MAX_HELP_FONT_SIZE as isize,
        );
        SendMessageW(spin, UDM_SETPOS32, 0, font_size as isize);
        let font = create_help_content_font(font_size);
        SendMessageW(edit, WM_SETFONT, font as usize, 0);
        SendMessageW(edit, EM_SETSEL, -1isize as usize, -1isize);
        set_window_long(
            hwnd,
            GWLP_USERDATA,
            Box::into_raw(Box::new(HelpWindowState {
                content: edit,
                font_edit,
                font,
            })) as isize,
        );

        let mut rect: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rect);
        layout_help_window(hwnd, rect.right - rect.left, rect.bottom - rect.top);
    }
}

pub(crate) fn loword(value: usize) -> u16 {
    (value & 0xffff) as u16
}

pub(crate) fn hiword(value: usize) -> u16 {
    ((value >> 16) & 0xffff) as u16
}

pub(crate) fn signed_loword(value: usize) -> i16 {
    loword(value) as i16
}

pub(crate) fn signed_hiword(value: usize) -> i16 {
    hiword(value) as i16
}

pub(crate) fn make_lparam(low: i32, high: i32) -> LPARAM {
    ((low as u16 as usize) | ((high as u16 as usize) << 16)) as LPARAM
}

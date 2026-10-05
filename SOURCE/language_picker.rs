use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::*;

struct PickerState {
    list: HWND,
    ok: HWND,
    selection: Option<AppLanguage>,
    closed: bool,
}

unsafe fn layout_picker(hwnd: HWND, state: &PickerState) {
    let mut rect: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rect);
    let scale = |value: i32| (value * GetDpiForWindow(hwnd).max(96) as i32 + 48) / 96;
    let pad = scale(12);
    let button_w = scale(88);
    let button_h = scale(30);
    MoveWindow(state.list, pad, pad, (rect.right - pad * 2).max(1),
        (rect.bottom - pad * 3 - button_h).max(1), TRUE);
    MoveWindow(state.ok, (rect.right - pad - button_w).max(pad),
        (rect.bottom - pad - button_h).max(pad), button_w, button_h, TRUE);
    SendMessageW(state.list, LB_SETITEMHEIGHT, 0, scale(28) as isize);
}

unsafe fn confirm_selection(hwnd: HWND, state: &mut PickerState) {
    let index = SendMessageW(state.list, LB_GETCURSEL, 0, 0) as i32;
    if index < 0 { return; }
    let language = AppLanguage::from_combo_index(index);
    let name = language_pack(language).map(|pack| pack.name).unwrap_or(BUILTIN_ENGLISH_NAME);
    let message = localized_format1(language, "Use {} as the application language?", name);
    let answer = MessageBoxW(hwnd, wide(&message).as_ptr(),
        wide(localized(language, "Confirm language")).as_ptr(),
        MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2);
    if answer == IDYES {
        state.selection = Some(language);
        DestroyWindow(hwnd);
    } else {
        SetFocus(state.list);
    }
}

unsafe extern "system" fn picker_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        set_window_long(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let state = get_window_long(hwnd, GWLP_USERDATA) as *mut PickerState;
    if !state.is_null() {
        match msg {
            WM_COMMAND if wparam & 0xffff == IDOK as usize && (wparam >> 16) == BN_CLICKED as usize => {
                confirm_selection(hwnd, &mut *state);
                return 0;
            }
            WM_SIZE => {
                layout_picker(hwnd, &*state);
                return 0;
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(lparam as *mut MINMAXINFO);
                let dpi = GetDpiForWindow(hwnd).max(96) as i32;
                info.ptMinTrackSize.x = 240 * dpi / 96;
                info.ptMinTrackSize.y = 180 * dpi / 96;
                return 0;
            }
            WM_DPICHANGED => {
                let rect = &*(lparam as *const RECT);
                SetWindowPos(hwnd, null_mut(), rect.left, rect.top, rect.right - rect.left,
                    rect.bottom - rect.top, SWP_NOZORDER | SWP_NOACTIVATE);
                layout_picker(hwnd, &*state);
                return 0;
            }
            WM_CLOSE => {
                DestroyWindow(hwnd);
                return 0;
            }
            WM_DESTROY => {
                (*state).closed = true;
                return 0;
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

pub(crate) unsafe fn ensure_startup_language() -> bool {
    if settings_path().exists() {
        return true;
    }
    // Share the first-run chooser between the launcher and settings processes.
    let Some(_guard) = create_named_instance_guard("FlashLaunch.FirstRunLanguage") else {
        return false;
    };
    if settings_path().exists() {
        return true;
    }
    let instance = GetModuleHandleW(null());
    let class = wide("FlashLaunch.LanguagePicker");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(picker_proc),
        hInstance: instance,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
        lpszClassName: class.as_ptr(),
        ..std::mem::zeroed()
    };
    if RegisterClassW(&wc) == 0 {
        return false;
    }
    let mut point: POINT = std::mem::zeroed();
    GetCursorPos(&mut point);
    let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(monitor, &mut info);
    let count = language_packs().len() + 1;
    let dpi = GetDpiForSystem().max(96);
    let scale = |value: i32| value * dpi as i32 / 96;
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_THICKFRAME;
    let mut bounds = RECT { left: 0, top: 0, right: scale(340), bottom: scale(count.min(12) as i32 * 28 + 66) };
    AdjustWindowRectExForDpi(&mut bounds, style, FALSE, WS_EX_DLGMODALFRAME, dpi);
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    let work = info.rcWork;
    let mut state = PickerState { list: null_mut(), ok: null_mut(), selection: None, closed: false };
    let hwnd = CreateWindowExW(
        WS_EX_DLGMODALFRAME, class.as_ptr(), wide("").as_ptr(),
        style, work.left + (work.right - work.left - width) / 2,
        work.top + (work.bottom - work.top - height) / 2, width, height,
        null_mut(), null_mut(), instance, &mut state as *mut _ as *mut _,
    );
    if hwnd.is_null() {
        UnregisterClassW(class.as_ptr(), instance);
        return false;
    }
    state.list = CreateWindowExW(
        0, wide("LISTBOX").as_ptr(), wide("").as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32,
        8, 8, width - 18, height - 18, hwnd, 1001 as HMENU, instance, null(),
    );
    if state.list.is_null() {
        DestroyWindow(hwnd);
        UnregisterClassW(class.as_ptr(), instance);
        return false;
    }
    state.ok = CreateWindowExW(0, wide("BUTTON").as_ptr(), wide("OK").as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
        0, 0, 0, 0, hwnd, IDOK as HMENU, instance, null());
    if state.ok.is_null() {
        DestroyWindow(hwnd);
        UnregisterClassW(class.as_ptr(), instance);
        return false;
    }
    SendMessageW(state.ok, WM_SETFONT, GetStockObject(DEFAULT_GUI_FONT) as usize, 1);
    SendMessageW(state.list, WM_SETFONT, GetStockObject(DEFAULT_GUI_FONT) as usize, 1);
    SendMessageW(state.list, LB_ADDSTRING, 0, wide(BUILTIN_ENGLISH_NAME).as_ptr() as isize);
    for pack in language_packs() {
        SendMessageW(state.list, LB_ADDSTRING, 0, wide(pack.name).as_ptr() as isize);
    }
    SendMessageW(state.list, LB_SETCURSEL, 0, 0);
    layout_picker(hwnd, &state);
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
    SetFocus(state.list);
    let mut msg: MSG = std::mem::zeroed();
    while !state.closed && GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
        if msg.message == WM_KEYDOWN && msg.wParam == VK_ESCAPE as usize {
            DestroyWindow(hwnd);
        } else if msg.message == WM_KEYDOWN && msg.wParam == VK_RETURN as usize {
            confirm_selection(hwnd, &mut state);
        } else if IsDialogMessageW(hwnd, &msg) == 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    if !state.closed {
        DestroyWindow(hwnd);
    }
    UnregisterClassW(class.as_ptr(), instance);
    let Some(language) = state.selection else { return false; };
    update_setting_lines(&[("language", language.setting_value().to_string())]).is_ok()
}

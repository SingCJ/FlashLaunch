use std::cell::Cell;
use std::ffi::c_void;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::{failed, format_hresult, os_wide, release_com, wide, IUnknownVTable};

const SHELL_UI_CLASS_NAME: &str = "Flash Launch Shell UI Helper Window";
const CONTEXT_MENU_ID_FIRST: u32 = 1;
const CONTEXT_MENU_ID_LAST: u32 = 0x7FFF;
const CONTEXT_MENU_WINDOW_APPEARANCE_GRACE: Duration = Duration::from_millis(750);
const PROPERTIES_WINDOW_APPEARANCE_GRACE: Duration = Duration::from_secs(3);
const SHELL_WINDOW_MAX_LIFETIME: Duration = Duration::from_secs(570);
const SHELL_WINDOW_MESSAGE_WAIT_MS: u32 = 50;
const CMIC_MASK_NOASYNC_LOCAL: u32 = 0x0000_0100;
const CMIC_MASK_UNICODE_LOCAL: u32 = 0x0000_4000;
const IID_ISHELL_FOLDER_LOCAL: GUID = GUID::from_u128(0x000214e6_0000_0000_c000_000000000046);
const IID_ICONTEXT_MENU_LOCAL: GUID = GUID::from_u128(0x000214e4_0000_0000_c000_000000000046);
const IID_ICONTEXT_MENU2_LOCAL: GUID = GUID::from_u128(0x000214f4_0000_0000_c000_000000000046);
const IID_ICONTEXT_MENU3_LOCAL: GUID = GUID::from_u128(0xbcfce0a0_ec17_11d0_8d10_00a0c90f2719);

#[repr(C)]
struct IShellFolderVTable {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    parse_display_name: unsafe extern "system" fn(
        *mut c_void,
        HWND,
        *mut c_void,
        *mut u16,
        *mut u32,
        *mut *mut ITEMIDLIST,
        *mut u32,
    ) -> i32,
    enum_objects: unsafe extern "system" fn(*mut c_void, HWND, u32, *mut *mut c_void) -> i32,
    bind_to_object: unsafe extern "system" fn(
        *mut c_void,
        *const ITEMIDLIST,
        *mut c_void,
        *const GUID,
        *mut *mut c_void,
    ) -> i32,
    bind_to_storage: unsafe extern "system" fn(
        *mut c_void,
        *const ITEMIDLIST,
        *mut c_void,
        *const GUID,
        *mut *mut c_void,
    ) -> i32,
    compare_ids:
        unsafe extern "system" fn(*mut c_void, isize, *const ITEMIDLIST, *const ITEMIDLIST) -> i32,
    create_view_object:
        unsafe extern "system" fn(*mut c_void, HWND, *const GUID, *mut *mut c_void) -> i32,
    get_attributes_of:
        unsafe extern "system" fn(*mut c_void, u32, *const *const ITEMIDLIST, *mut u32) -> i32,
    get_ui_object_of: unsafe extern "system" fn(
        *mut c_void,
        HWND,
        u32,
        *const *const ITEMIDLIST,
        *const GUID,
        *mut u32,
        *mut *mut c_void,
    ) -> i32,
    get_display_name_of:
        unsafe extern "system" fn(*mut c_void, *const ITEMIDLIST, u32, *mut c_void) -> i32,
    set_name_of: unsafe extern "system" fn(
        *mut c_void,
        HWND,
        *const ITEMIDLIST,
        *const u16,
        u32,
        *mut *mut ITEMIDLIST,
    ) -> i32,
}

#[repr(C)]
struct IContextMenuVTable {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    query_context_menu: unsafe extern "system" fn(*mut c_void, HMENU, u32, u32, u32, u32) -> i32,
    invoke_command: unsafe extern "system" fn(*mut c_void, *const CMINVOKECOMMANDINFO) -> i32,
    get_command_string:
        unsafe extern "system" fn(*mut c_void, usize, u32, *mut u32, *mut u8, u32) -> i32,
}

#[repr(C)]
struct IContextMenu2VTable {
    base: IContextMenuVTable,
    handle_menu_msg: unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> i32,
}

#[repr(C)]
struct IContextMenu3VTable {
    base: IContextMenu2VTable,
    handle_menu_msg2: unsafe extern "system" fn(*mut c_void, u32, usize, isize, *mut isize) -> i32,
}

#[derive(Clone, Copy)]
struct ActiveContextMenu {
    menu2: *mut c_void,
    menu3: *mut c_void,
}

thread_local! {
    static ACTIVE_CONTEXT_MENU: Cell<ActiveContextMenu> = const {
        Cell::new(ActiveContextMenu {
            menu2: null_mut(),
            menu3: null_mut(),
        })
    };
}

struct ComPtr(*mut c_void);

impl ComPtr {
    fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for ComPtr {
    fn drop(&mut self) {
        unsafe { release_com(self.0) };
    }
}

struct PidlPtr(*mut ITEMIDLIST);

impl Drop for PidlPtr {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(self.0 as *const c_void) };
    }
}

struct MenuHandle(HMENU);

impl Drop for MenuHandle {
    fn drop(&mut self) {
        unsafe { DestroyMenu(self.0) };
    }
}

struct WindowHandle(HWND);

impl Drop for WindowHandle {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) };
    }
}

struct ShellWindowSearch {
    owner: HWND,
    process_id: u32,
    found: bool,
}

pub(crate) fn show_shell_context_menu(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err("Target path does not exist.".to_string());
    }
    unsafe { show_shell_context_menu_inner(path) }
}

pub(crate) fn show_shell_properties_dialog(path: &Path) -> Result<(), String> {
    unsafe {
        let owner = create_shell_owner_window()?;
        crate::shell_properties_path(owner.0, path)?;
        wait_for_shell_windows(owner.0, PROPERTIES_WINDOW_APPEARANCE_GRACE)
    }
}

unsafe fn show_shell_context_menu_inner(path: &Path) -> Result<(), String> {
    let path_wide = os_wide(path.as_os_str());
    let mut absolute_pidl = null_mut();
    let parse_result = SHParseDisplayName(
        path_wide.as_ptr(),
        null_mut(),
        &mut absolute_pidl,
        0,
        null_mut(),
    );
    if failed(parse_result) || absolute_pidl.is_null() {
        return Err(format_hresult("SHParseDisplayName", parse_result));
    }
    let absolute_pidl = PidlPtr(absolute_pidl);

    let mut parent_folder = null_mut();
    let mut child_pidl = null_mut();
    let bind_result = SHBindToParent(
        absolute_pidl.0,
        &IID_ISHELL_FOLDER_LOCAL,
        &mut parent_folder,
        &mut child_pidl,
    );
    if failed(bind_result) || parent_folder.is_null() || child_pidl.is_null() {
        return Err(format_hresult("SHBindToParent", bind_result));
    }
    let parent_folder = ComPtr(parent_folder);
    let owner = create_shell_owner_window()?;

    let parent_vtable = *(parent_folder.as_ptr() as *mut *mut IShellFolderVTable);
    let child_items = [child_pidl as *const ITEMIDLIST];
    let mut context_menu = null_mut();
    let menu_result = ((*parent_vtable).get_ui_object_of)(
        parent_folder.as_ptr(),
        owner.0,
        1,
        child_items.as_ptr(),
        &IID_ICONTEXT_MENU_LOCAL,
        null_mut(),
        &mut context_menu,
    );
    if failed(menu_result) || context_menu.is_null() {
        return Err(format_hresult("IShellFolder::GetUIObjectOf", menu_result));
    }
    let context_menu = ComPtr(context_menu);
    let popup_menu = CreatePopupMenu();
    if popup_menu.is_null() {
        return Err("Could not create the Windows Shell context menu.".to_string());
    }
    let popup_menu = MenuHandle(popup_menu);

    let context_vtable = *(context_menu.as_ptr() as *mut *mut IContextMenuVTable);
    let mut query_flags = CMF_NORMAL | CMF_EXPLORE;
    if GetKeyState(VK_SHIFT as i32) < 0 {
        query_flags |= CMF_EXTENDEDVERBS;
    }
    let query_result = ((*context_vtable).query_context_menu)(
        context_menu.as_ptr(),
        popup_menu.0,
        0,
        CONTEXT_MENU_ID_FIRST,
        CONTEXT_MENU_ID_LAST,
        query_flags,
    );
    if failed(query_result) {
        return Err(format_hresult(
            "IContextMenu::QueryContextMenu",
            query_result,
        ));
    }

    let menu3 = query_interface(context_menu.as_ptr(), &IID_ICONTEXT_MENU3_LOCAL);
    let menu2 = if menu3.is_none() {
        query_interface(context_menu.as_ptr(), &IID_ICONTEXT_MENU2_LOCAL)
    } else {
        None
    };
    ACTIVE_CONTEXT_MENU.with(|active| {
        active.set(ActiveContextMenu {
            menu2: menu2.as_ref().map_or(null_mut(), ComPtr::as_ptr),
            menu3: menu3.as_ref().map_or(null_mut(), ComPtr::as_ptr),
        });
    });

    let mut point = POINT { x: 0, y: 0 };
    GetCursorPos(&mut point);
    SetForegroundWindow(owner.0);
    let command = TrackPopupMenuEx(
        popup_menu.0,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        point.x,
        point.y,
        owner.0,
        null(),
    );
    ACTIVE_CONTEXT_MENU.with(|active| {
        active.set(ActiveContextMenu {
            menu2: null_mut(),
            menu3: null_mut(),
        });
    });
    PostMessageW(owner.0, WM_NULL, 0, 0);
    if command == 0 {
        return Ok(());
    }

    let Some(command_offset) = context_menu_command_offset(command) else {
        return Err("Windows Shell returned an invalid context menu command.".to_string());
    };
    let command_verb =
        context_menu_command_verb(context_menu.as_ptr(), context_vtable, command_offset);
    let working_directory = path
        .parent()
        .map(|parent| os_wide(parent.as_os_str()))
        .unwrap_or_else(|| vec![0]);
    let mut invoke_mask = CMIC_MASK_NOASYNC_LOCAL | CMIC_MASK_UNICODE_LOCAL | CMIC_MASK_PTINVOKE;
    if GetKeyState(VK_SHIFT as i32) < 0 {
        invoke_mask |= CMIC_MASK_SHIFT_DOWN;
    }
    if GetKeyState(VK_CONTROL as i32) < 0 {
        invoke_mask |= CMIC_MASK_CONTROL_DOWN;
    }
    let command_resource = command_offset as *const u8;
    let command_resource_wide = command_offset as *const u16;
    let invoke = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: invoke_mask,
        hwnd: owner.0,
        lpVerb: command_resource,
        lpParameters: null(),
        lpDirectory: null(),
        nShow: SW_SHOWNORMAL,
        dwHotKey: 0,
        hIcon: null_mut(),
        lpTitle: null(),
        lpVerbW: command_resource_wide,
        lpParametersW: null(),
        lpDirectoryW: working_directory.as_ptr(),
        lpTitleW: null(),
        ptInvoke: point,
    };
    let invoke_result = ((*context_vtable).invoke_command)(
        context_menu.as_ptr(),
        &invoke as *const CMINVOKECOMMANDINFOEX as *const CMINVOKECOMMANDINFO,
    );
    if failed(invoke_result) {
        return Err(format_hresult("IContextMenu::InvokeCommand", invoke_result));
    }
    if should_wait_for_context_menu_command(command_verb.as_deref()) {
        wait_for_shell_windows(owner.0, CONTEXT_MENU_WINDOW_APPEARANCE_GRACE)?;
    }
    Ok(())
}

unsafe fn create_shell_owner_window() -> Result<WindowHandle, String> {
    let instance = GetModuleHandleW(null());
    let class_name = wide(SHELL_UI_CLASS_NAME);
    let window_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(context_menu_window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: null_mut(),
        hCursor: null_mut(),
        hbrBackground: null_mut(),
        lpszMenuName: null(),
        lpszClassName: class_name.as_ptr(),
    };
    RegisterClassW(&window_class);
    let owner = CreateWindowExW(
        0,
        class_name.as_ptr(),
        class_name.as_ptr(),
        WS_POPUP,
        0,
        0,
        0,
        0,
        null_mut(),
        null_mut(),
        instance,
        null(),
    );
    if owner.is_null() {
        Err("Could not create the Shell UI owner window.".to_string())
    } else {
        Ok(WindowHandle(owner))
    }
}

unsafe fn query_interface(value: *mut c_void, iid: &GUID) -> Option<ComPtr> {
    let vtable = *(value as *mut *mut IUnknownVTable);
    let mut queried = null_mut();
    let result = ((*vtable).query_interface)(value, iid, &mut queried);
    if failed(result) || queried.is_null() {
        None
    } else {
        Some(ComPtr(queried))
    }
}

fn context_menu_command_offset(command: i32) -> Option<usize> {
    u32::try_from(command)
        .ok()?
        .checked_sub(CONTEXT_MENU_ID_FIRST)
        .map(|offset| offset as usize)
}

unsafe fn context_menu_command_verb(
    context_menu: *mut c_void,
    context_vtable: *mut IContextMenuVTable,
    command_offset: usize,
) -> Option<String> {
    let mut verb = [0u16; 128];
    let result = ((*context_vtable).get_command_string)(
        context_menu,
        command_offset,
        GCS_VERBW,
        null_mut(),
        verb.as_mut_ptr() as *mut u8,
        verb.len() as u32,
    );
    if failed(result) {
        return None;
    }
    let length = verb
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(verb.len());
    Some(String::from_utf16_lossy(&verb[..length]))
}

fn should_wait_for_context_menu_command(command_verb: Option<&str>) -> bool {
    command_verb.is_none_or(|verb| verb.eq_ignore_ascii_case("properties"))
}

unsafe fn wait_for_shell_windows(owner: HWND, appearance_grace: Duration) -> Result<(), String> {
    let started = Instant::now();
    let appearance_deadline = started + appearance_grace;
    let timeout_deadline = started + SHELL_WINDOW_MAX_LIFETIME;
    let mut observed_window = false;
    loop {
        pump_shell_window_messages()?;
        let visible_window = has_visible_shell_window(owner);
        observed_window |= visible_window;
        let now = Instant::now();
        if !should_wait_for_shell_window(observed_window, visible_window, now < appearance_deadline)
        {
            return Ok(());
        }
        if now >= timeout_deadline {
            return Err("The Windows Shell dialog exceeded its maximum lifetime.".to_string());
        }
        MsgWaitForMultipleObjectsEx(
            0,
            null(),
            SHELL_WINDOW_MESSAGE_WAIT_MS,
            QS_ALLINPUT,
            MWMO_INPUTAVAILABLE,
        );
    }
}

fn should_wait_for_shell_window(
    observed_window: bool,
    visible_window: bool,
    inside_appearance_grace: bool,
) -> bool {
    visible_window || (!observed_window && inside_appearance_grace)
}

unsafe fn pump_shell_window_messages() -> Result<(), String> {
    let mut message: MSG = std::mem::zeroed();
    while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
        if message.message == WM_QUIT {
            return Err("The Shell context menu helper received a quit request.".to_string());
        }
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    Ok(())
}

unsafe fn has_visible_shell_window(owner: HWND) -> bool {
    let mut search = ShellWindowSearch {
        owner,
        process_id: GetCurrentProcessId(),
        found: false,
    };
    EnumWindows(
        Some(find_visible_shell_window),
        &mut search as *mut ShellWindowSearch as isize,
    );
    search.found
}

unsafe extern "system" fn find_visible_shell_window(hwnd: HWND, lparam: isize) -> i32 {
    let search = unsafe { &mut *(lparam as *mut ShellWindowSearch) };
    if hwnd == search.owner || unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }
    let mut process_id = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) };
    let owned_by_helper = unsafe { GetWindow(hwnd, GW_OWNER) } == search.owner;
    if process_id == search.process_id || owned_by_helper {
        search.found = true;
        return 0;
    }
    1
}

unsafe extern "system" fn context_menu_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    let handled = ACTIVE_CONTEXT_MENU.with(|active| {
        let active = active.get();
        if message == WM_MENUCHAR && !active.menu3.is_null() {
            let vtable = unsafe { *(active.menu3 as *mut *mut IContextMenu3VTable) };
            let mut result = 0isize;
            let status = unsafe {
                ((*vtable).handle_menu_msg2)(active.menu3, message, wparam, lparam, &mut result)
            };
            if !failed(status) {
                return Some(result);
            }
        }
        if matches!(message, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM) {
            if !active.menu3.is_null() {
                let vtable = unsafe { *(active.menu3 as *mut *mut IContextMenu3VTable) };
                let status = unsafe {
                    ((*vtable).base.handle_menu_msg)(active.menu3, message, wparam, lparam)
                };
                if !failed(status) {
                    return Some(0);
                }
            } else if !active.menu2.is_null() {
                let vtable = unsafe { *(active.menu2 as *mut *mut IContextMenu2VTable) };
                let status =
                    unsafe { ((*vtable).handle_menu_msg)(active.menu2, message, wparam, lparam) };
                if !failed(status) {
                    return Some(0);
                }
            }
        }
        None
    });
    handled.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, message, wparam, lparam) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_context_menu_command_ids_use_zero_based_verbs() {
        assert_eq!(context_menu_command_offset(1), Some(0));
        assert_eq!(context_menu_command_offset(17), Some(16));
        assert_eq!(context_menu_command_offset(0), None);
        assert_eq!(context_menu_command_offset(-1), None);
    }

    #[test]
    fn shell_window_waiting_covers_grace_visibility_and_close() {
        assert!(should_wait_for_shell_window(false, false, true));
        assert!(!should_wait_for_shell_window(false, false, false));
        assert!(should_wait_for_shell_window(false, true, false));
        assert!(should_wait_for_shell_window(true, true, false));
        assert!(!should_wait_for_shell_window(true, false, false));
    }

    #[test]
    fn context_menu_waits_only_for_properties_or_unknown_verbs() {
        assert!(should_wait_for_context_menu_command(Some("properties")));
        assert!(should_wait_for_context_menu_command(Some("Properties")));
        assert!(should_wait_for_context_menu_command(None));
        assert!(!should_wait_for_context_menu_command(Some("open")));
        assert!(!should_wait_for_context_menu_command(Some("copy")));
    }
}

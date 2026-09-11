use std::ffi::c_void;
use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{copy_nonoverlapping, null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, HDC};
use windows_sys::Win32::System::Com::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Ole::*;
use windows_sys::Win32::System::SystemServices::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::*;

pub(crate) fn shell_open_path_result(hwnd: HWND, path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err("Target path does not exist.".to_string());
    }
    let target = os_wide(path.as_os_str());
    shell_open_wide(hwnd, &target).map_err(|error| shell_execute_error_message(error, Some(path)))
}

pub(crate) fn shell_explore_path_result(hwnd: HWND, path: &Path) -> Result<(), String> {
    if !path.is_dir() {
        return Err("Target directory does not exist.".to_string());
    }
    shell_explore_directory(hwnd, path)
        .map_err(|error| shell_execute_error_message(error, Some(path)))
}

pub(crate) fn shell_launch_path_result(hwnd: HWND, path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err("Target path does not exist.".to_string());
    }
    let target = os_wide(path.as_os_str());
    let working_directory = launch_working_directory(path, path.is_dir());
    let result = if launch_uses_default_shell_verb(path) {
        shell_default_wide_with_working_directory(hwnd, &target, working_directory)
    } else {
        shell_open_wide_with_working_directory(hwnd, &target, working_directory)
    };
    result.map_err(|error| shell_execute_error_message(error, Some(path)))
}

fn launch_working_directory(path: &Path, is_directory: bool) -> Option<&Path> {
    if is_lnk_file(path) {
        return None;
    }
    if is_directory {
        return Some(path);
    }
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
}

fn launch_uses_default_shell_verb(path: &Path) -> bool {
    is_lnk_file(path)
}

pub(crate) fn shell_open_wide(hwnd: HWND, target: &[u16]) -> Result<(), isize> {
    shell_open_wide_with_working_directory(hwnd, target, None)
}

fn explorer_directory_parameters(path: &Path) -> Vec<u16> {
    let mut parameters = Vec::with_capacity(path.as_os_str().len() + 3);
    parameters.push(u16::from(b'"'));
    parameters.extend(path.as_os_str().encode_wide());
    parameters.push(u16::from(b'"'));
    parameters.push(0);
    parameters
}

fn shell_explore_directory(hwnd: HWND, path: &Path) -> Result<(), isize> {
    let explorer = wide("explorer.exe");
    let parameters = explorer_directory_parameters(path);
    unsafe {
        let result = ShellExecuteW(
            hwnd,
            null(),
            explorer.as_ptr(),
            parameters.as_ptr(),
            null(),
            SW_SHOWNORMAL,
        ) as isize;
        if result > 32 {
            Ok(())
        } else {
            Err(result)
        }
    }
}

fn shell_open_wide_with_working_directory(
    hwnd: HWND,
    target: &[u16],
    working_directory: Option<&Path>,
) -> Result<(), isize> {
    let operation = wide("open");
    shell_execute_wide(hwnd, Some(&operation), target, working_directory)
}

fn shell_default_wide_with_working_directory(
    hwnd: HWND,
    target: &[u16],
    working_directory: Option<&Path>,
) -> Result<(), isize> {
    shell_execute_wide(hwnd, None, target, working_directory)
}

fn shell_execute_wide(
    hwnd: HWND,
    operation: Option<&[u16]>,
    target: &[u16],
    working_directory: Option<&Path>,
) -> Result<(), isize> {
    let operation_ptr = operation.map_or(null(), |value| value.as_ptr());
    let working_directory_wide = working_directory.map(|path| os_wide(path.as_os_str()));
    let working_directory_ptr = working_directory_wide
        .as_ref()
        .map_or(null(), |path| path.as_ptr());
    unsafe {
        let result = ShellExecuteW(
            hwnd,
            operation_ptr,
            target.as_ptr(),
            null(),
            working_directory_ptr,
            SW_SHOWNORMAL,
        ) as isize;
        if result > 32 {
            Ok(())
        } else {
            Err(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_folder_is_real_target_folder_not_shortcut_parent() {
        let root = std::env::current_dir().unwrap().join("TEMP").join(format!(
            "linked-folder-test-{}",
            std::process::id()
        ));
        let target_dir = root.join("Dữ liệu thật");
        let shortcut_dir = root.join("Lối tắt");
        fs::create_dir_all(&target_dir).unwrap();
        fs::create_dir_all(&shortcut_dir).unwrap();
        let target = target_dir.join("tệp.txt");
        fs::write(&target, b"test").unwrap();
        let shortcut = shortcut_dir.join("tệp.LNK");
        unsafe {
            assert!(CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) >= 0);
            create_shortcut_with_shell_link(&shortcut, &target).unwrap();
            assert_eq!(linked_target_folder(&shortcut), Some(target_dir.clone()));
            assert_ne!(linked_target_folder(&shortcut), Some(shortcut_dir.clone()));
            let folder_shortcut = shortcut_dir.join("folder.lnk");
            create_shortcut_with_shell_link(&folder_shortcut, &target_dir).unwrap();
            assert_eq!(linked_target_folder(&folder_shortcut), Some(target_dir));
            fs::remove_file(&target).unwrap();
            assert_eq!(linked_target_folder(&shortcut), None);
            CoUninitialize();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explorer_directory_parameters_quote_spaces_and_unicode() {
        let parameters = explorer_directory_parameters(Path::new(r"C:\Dữ liệu\Firefox Portable"));
        assert_eq!(parameters, wide(r#""C:\Dữ liệu\Firefox Portable""#));
    }

    #[test]
    fn launch_working_directory_uses_file_parent() {
        let path = Path::new(r"C:\Projects\Tools\launch.bat");
        assert_eq!(
            launch_working_directory(path, false),
            Some(Path::new(r"C:\Projects\Tools"))
        );
    }

    #[test]
    fn launch_working_directory_uses_directory_itself() {
        let path = Path::new(r"C:\Projects\Tools");
        assert_eq!(launch_working_directory(path, true), Some(path));
    }

    #[test]
    fn launch_working_directory_does_not_override_shortcut() {
        let path = Path::new(r"C:\Shortcuts\Tool.lnk");
        assert_eq!(launch_working_directory(path, false), None);
    }

    #[test]
    fn launch_uses_default_shell_verb_for_lnk_only() {
        assert!(launch_uses_default_shell_verb(Path::new(
            r"C:\Shortcuts\Control Panel.LNK"
        )));
        assert!(!launch_uses_default_shell_verb(Path::new(
            r"C:\Shortcuts\Website.url"
        )));
        assert!(!launch_uses_default_shell_verb(Path::new(
            r"C:\Tools\Tool.exe"
        )));
    }

    #[test]
    fn launch_working_directory_preserves_unicode_parent() {
        let path = Path::new(r"C:\Dữ liệu\Ứng dụng\công cụ.exe");
        assert_eq!(
            launch_working_directory(path, false),
            Some(Path::new(r"C:\Dữ liệu\Ứng dụng"))
        );
    }

    #[test]
    fn launch_working_directory_ignores_parentless_path() {
        assert_eq!(
            launch_working_directory(Path::new("launch.bat"), false),
            None
        );
    }
}

pub(crate) fn begin_shell_shortcut_drag(
    _hwnd: HWND,
    target: &Path,
    title: &str,
) -> Result<(), String> {
    let shortcut_path = if is_shortcut_file(target) {
        target.to_path_buf()
    } else {
        create_drag_shortcut(target, title)?
    };

    let data_object = create_hdrop_data_object(shortcut_path);
    let drop_source = create_drop_source();
    unsafe {
        let mut effect: DROPEFFECT = 0;
        let drag_hr = DoDragDrop(data_object, drop_source, DROPEFFECT_COPY, &mut effect);
        release_com(drop_source);
        release_com(data_object);
        if failed(drag_hr) {
            return Err(format_hresult("DoDragDrop", drag_hr));
        }
    }
    Ok(())
}

pub(crate) fn create_drag_shortcut(target: &Path, title: &str) -> Result<PathBuf, String> {
    if !target.exists() {
        return Err("Target path does not exist.".to_string());
    }
    let drag_dir = app_config_dir().join("drag");
    fs::create_dir_all(&drag_dir)
        .map_err(|error| format!("Could not create drag folder: {error}"))?;
    cleanup_old_drag_shortcuts(&drag_dir);

    let shortcut_name = unique_shortcut_name(title, target);
    let shortcut_path = drag_dir.join(shortcut_name);
    unsafe { create_shortcut_with_shell_link(&shortcut_path, target)? };
    Ok(shortcut_path)
}

pub(crate) unsafe fn create_shortcut_with_shell_link(
    shortcut_path: &Path,
    target: &Path,
) -> Result<(), String> {
    let mut shell_link: *mut c_void = null_mut();
    let hr = CoCreateInstance(
        &ShellLink,
        null_mut(),
        CLSCTX_INPROC_SERVER,
        &IID_I_SHELL_LINK_W,
        &mut shell_link,
    );
    if failed(hr) || shell_link.is_null() {
        return Err(format_hresult("CoCreateInstance(IShellLinkW)", hr));
    }

    let result = create_shortcut_from_shell_link(shell_link, shortcut_path, target);
    release_com(shell_link);
    result
}

pub(crate) unsafe fn create_shortcut_from_shell_link(
    shell_link: *mut c_void,
    shortcut_path: &Path,
    target: &Path,
) -> Result<(), String> {
    let shell_vtbl = *(shell_link as *mut *mut IShellLinkWVTable);
    let target_text = os_wide(target.as_os_str());
    let hr = ((*shell_vtbl).set_path)(shell_link, target_text.as_ptr());
    if failed(hr) {
        return Err(format_hresult("IShellLinkW::SetPath", hr));
    }

    let working_dir = shortcut_working_directory(target);
    if !working_dir.is_empty() {
        let working_dir = wide(&working_dir);
        let hr = ((*shell_vtbl).set_working_directory)(shell_link, working_dir.as_ptr());
        if failed(hr) {
            return Err(format_hresult("IShellLinkW::SetWorkingDirectory", hr));
        }
    }

    let mut persist_file: *mut c_void = null_mut();
    let hr = ((*shell_vtbl).query_interface)(shell_link, &IID_I_PERSIST_FILE, &mut persist_file);
    if failed(hr) || persist_file.is_null() {
        return Err(format_hresult("QueryInterface(IPersistFile)", hr));
    }

    let persist_vtbl = *(persist_file as *mut *mut IPersistFileVTable);
    let shortcut_text = os_wide(shortcut_path.as_os_str());
    let hr = ((*persist_vtbl).save)(persist_file, shortcut_text.as_ptr(), TRUE);
    release_com(persist_file);
    if failed(hr) {
        return Err(format_hresult("IPersistFile::Save", hr));
    }
    if !shortcut_path.exists() {
        return Err("ShellLink did not create the shortcut.".to_string());
    }
    Ok(())
}

pub(crate) fn create_hdrop_data_object(path: PathBuf) -> *mut c_void {
    Box::into_raw(Box::new(HdropDataObject {
        vtbl: &HDROP_DATA_OBJECT_VTABLE,
        refs: AtomicU32::new(1),
        path,
    })) as *mut c_void
}

pub(crate) fn create_drop_source() -> *mut c_void {
    Box::into_raw(Box::new(DropSource {
        vtbl: &DROP_SOURCE_VTABLE,
        refs: AtomicU32::new(1),
    })) as *mut c_void
}

pub(crate) static HDROP_DATA_OBJECT_VTABLE: HdropDataObjectVTable = HdropDataObjectVTable {
    query_interface: hdrop_query_interface,
    add_ref: hdrop_add_ref,
    release: hdrop_release,
    get_data: hdrop_get_data,
    get_data_here: hdrop_get_data_here,
    query_get_data: hdrop_query_get_data,
    get_canonical_format_etc: hdrop_get_canonical_format_etc,
    set_data: hdrop_set_data,
    enum_format_etc: hdrop_enum_format_etc,
    d_advise: hdrop_d_advise,
    d_unadvise: hdrop_d_unadvise,
    enum_d_advise: hdrop_enum_d_advise,
};

pub(crate) static DROP_SOURCE_VTABLE: DropSourceVTable = DropSourceVTable {
    query_interface: drop_source_query_interface,
    add_ref: drop_source_add_ref,
    release: drop_source_release,
    query_continue_drag: drop_source_query_continue_drag,
    give_feedback: drop_source_give_feedback,
};

pub(crate) unsafe extern "system" fn hdrop_query_interface(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = null_mut();
    if !iid.is_null() && (guid_eq(&*iid, &IID_I_UNKNOWN) || guid_eq(&*iid, &IID_I_DATA_OBJECT)) {
        *out = this;
        hdrop_add_ref(this);
        return S_OK;
    }
    E_NOINTERFACE
}

pub(crate) unsafe extern "system" fn hdrop_add_ref(this: *mut c_void) -> u32 {
    let object = this as *mut HdropDataObject;
    (*object).refs.fetch_add(1, Ordering::Relaxed) + 1
}

pub(crate) unsafe extern "system" fn hdrop_release(this: *mut c_void) -> u32 {
    let object = this as *mut HdropDataObject;
    let refs = (*object).refs.fetch_sub(1, Ordering::Release) - 1;
    if refs == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(Box::from_raw(object));
    }
    refs
}

pub(crate) unsafe extern "system" fn hdrop_get_data(
    this: *mut c_void,
    format: *const FORMATETC,
    medium: *mut STGMEDIUM,
) -> i32 {
    if medium.is_null() {
        return E_INVALIDARG;
    }
    *medium = std::mem::zeroed();
    let status = hdrop_query_get_data(this, format);
    if status != S_OK {
        return status;
    }
    let object = &*(this as *mut HdropDataObject);
    let Some(hglobal) = create_hdrop_global(&object.path) else {
        return E_INVALIDARG;
    };
    (*medium).tymed = TYMED_HGLOBAL as u32;
    (*medium).u = STGMEDIUM_0 { hGlobal: hglobal };
    (*medium).pUnkForRelease = null_mut();
    S_OK
}

pub(crate) unsafe extern "system" fn hdrop_get_data_here(
    _this: *mut c_void,
    _format: *const FORMATETC,
    _medium: *mut STGMEDIUM,
) -> i32 {
    E_NOTIMPL
}

pub(crate) unsafe extern "system" fn hdrop_query_get_data(
    _this: *mut c_void,
    format: *const FORMATETC,
) -> i32 {
    if format.is_null() {
        return E_INVALIDARG;
    }
    let format = &*format;
    if format.cfFormat != CF_HDROP {
        return DV_E_FORMATETC;
    }
    if format.dwAspect != DVASPECT_CONTENT {
        return DV_E_FORMATETC;
    }
    if (format.tymed & TYMED_HGLOBAL as u32) == 0 {
        return DV_E_FORMATETC;
    }
    S_OK
}

pub(crate) unsafe extern "system" fn hdrop_get_canonical_format_etc(
    _this: *mut c_void,
    _input: *const FORMATETC,
    output: *mut FORMATETC,
) -> i32 {
    if !output.is_null() {
        (*output).ptd = null_mut();
    }
    E_NOTIMPL
}

pub(crate) unsafe extern "system" fn hdrop_set_data(
    _this: *mut c_void,
    _format: *const FORMATETC,
    _medium: *const STGMEDIUM,
    _release: i32,
) -> i32 {
    E_NOTIMPL
}

pub(crate) unsafe extern "system" fn hdrop_enum_format_etc(
    _this: *mut c_void,
    direction: u32,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = null_mut();
    if direction != DATADIR_GET as u32 {
        return E_NOTIMPL;
    }
    let format = FORMATETC {
        cfFormat: CF_HDROP,
        ptd: null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL as u32,
    };
    SHCreateStdEnumFmtEtc(1, &format, out)
}

pub(crate) unsafe extern "system" fn hdrop_d_advise(
    _this: *mut c_void,
    _format: *const FORMATETC,
    _flags: u32,
    _sink: *mut c_void,
    _connection: *mut u32,
) -> i32 {
    OLE_E_ADVISENOTSUPPORTED
}

pub(crate) unsafe extern "system" fn hdrop_d_unadvise(_this: *mut c_void, _connection: u32) -> i32 {
    OLE_E_ADVISENOTSUPPORTED
}

pub(crate) unsafe extern "system" fn hdrop_enum_d_advise(
    _this: *mut c_void,
    out: *mut *mut c_void,
) -> i32 {
    if !out.is_null() {
        *out = null_mut();
    }
    OLE_E_ADVISENOTSUPPORTED
}

pub(crate) unsafe extern "system" fn drop_source_query_interface(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = null_mut();
    if !iid.is_null() && (guid_eq(&*iid, &IID_I_UNKNOWN) || guid_eq(&*iid, &IID_I_DROP_SOURCE)) {
        *out = this;
        drop_source_add_ref(this);
        return S_OK;
    }
    E_NOINTERFACE
}

pub(crate) unsafe extern "system" fn drop_source_add_ref(this: *mut c_void) -> u32 {
    let object = this as *mut DropSource;
    (*object).refs.fetch_add(1, Ordering::Relaxed) + 1
}

pub(crate) unsafe extern "system" fn drop_source_release(this: *mut c_void) -> u32 {
    let object = this as *mut DropSource;
    let refs = (*object).refs.fetch_sub(1, Ordering::Release) - 1;
    if refs == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(Box::from_raw(object));
    }
    refs
}

pub(crate) unsafe extern "system" fn drop_source_query_continue_drag(
    _this: *mut c_void,
    escape_pressed: i32,
    key_state: u32,
) -> i32 {
    if escape_pressed != 0 {
        return DRAGDROP_S_CANCEL;
    }
    if (key_state & MK_LBUTTON) == 0 {
        return DRAGDROP_S_DROP;
    }
    S_OK
}

pub(crate) unsafe extern "system" fn drop_source_give_feedback(
    _this: *mut c_void,
    _effect: DROPEFFECT,
) -> i32 {
    DRAGDROP_S_USEDEFAULTCURSORS
}

pub(crate) fn create_hdrop_global(path: &Path) -> Option<HGLOBAL> {
    let mut file_units: Vec<u16> = os_wide(path.as_os_str());
    file_units.push(0);
    let header_size = std::mem::size_of::<DROPFILES>();
    let bytes = header_size + file_units.len() * std::mem::size_of::<u16>();
    unsafe {
        let hglobal = GlobalAlloc(GHND, bytes);
        if hglobal.is_null() {
            return None;
        }
        let data = GlobalLock(hglobal) as *mut u8;
        if data.is_null() {
            GlobalFree(hglobal);
            return None;
        }
        let dropfiles = DROPFILES {
            pFiles: header_size as u32,
            pt: POINT { x: 0, y: 0 },
            fNC: 0,
            fWide: TRUE,
        };
        (data as *mut DROPFILES).write(dropfiles);
        copy_nonoverlapping(
            file_units.as_ptr(),
            data.add(header_size) as *mut u16,
            file_units.len(),
        );
        GlobalUnlock(hglobal);
        Some(hglobal)
    }
}

pub(crate) fn guid_eq(left: &GUID, right: &GUID) -> bool {
    left.data1 == right.data1
        && left.data2 == right.data2
        && left.data3 == right.data3
        && left.data4 == right.data4
}

pub(crate) fn is_shortcut_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_ascii_lowercase())
            .as_deref(),
        Some("lnk" | "url")
    )
}

pub(crate) fn linked_target_folder(path: &Path) -> Option<PathBuf> {
    let target = resolve_shortcut_target(path)?;
    if target.is_dir() {
        Some(target)
    } else if target.is_file() {
        target.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

pub(crate) fn resolve_shortcut_target(path: &Path) -> Option<PathBuf> {
    if !is_lnk_file(path) {
        return None;
    }
    unsafe { resolve_lnk_target(path) }
}

pub(crate) fn is_lnk_file(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case("lnk"))
        .unwrap_or(false)
}

pub(crate) unsafe fn resolve_lnk_target(path: &Path) -> Option<PathBuf> {
    let mut shell_link: *mut c_void = null_mut();
    if CoCreateInstance(
        &ShellLink,
        null_mut(),
        CLSCTX_INPROC_SERVER,
        &IID_I_SHELL_LINK_W,
        &mut shell_link,
    ) < 0
        || shell_link.is_null()
    {
        return None;
    }

    let result = resolve_lnk_target_from_shell_link(shell_link, path);
    release_com(shell_link);
    result
}

pub(crate) unsafe fn resolve_lnk_target_from_shell_link(
    shell_link: *mut c_void,
    path: &Path,
) -> Option<PathBuf> {
    let shell_vtbl = *(shell_link as *mut *mut IShellLinkWVTable);
    let mut persist_file: *mut c_void = null_mut();
    if ((*shell_vtbl).query_interface)(shell_link, &IID_I_PERSIST_FILE, &mut persist_file) < 0
        || persist_file.is_null()
    {
        return None;
    }

    let persist_vtbl = *(persist_file as *mut *mut IPersistFileVTable);
    let file = os_wide(path.as_os_str());
    let loaded = ((*persist_vtbl).load)(persist_file, file.as_ptr(), STGM_READ) >= 0;
    release_com(persist_file);
    if !loaded {
        return None;
    }

    let mut buffer = vec![0u16; 32_768];
    if ((*shell_vtbl).get_path)(
        shell_link,
        buffer.as_mut_ptr(),
        buffer.len() as i32,
        null_mut(),
        0,
    ) < 0
    {
        return None;
    }
    path_from_wide_buffer(&buffer)
}

pub(crate) fn path_from_wide_buffer(buffer: &[u16]) -> Option<PathBuf> {
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    if length == 0 {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&buffer[..length])))
}

pub(crate) fn folder_menu_path(path: &Path) -> String {
    let mut text = path.to_string_lossy().to_string();
    if !text.ends_with(['\\', '/']) {
        text.push('\\');
    }
    text
}

pub(crate) fn cleanup_old_drag_shortcuts(drag_dir: &Path) {
    let Ok(entries) = fs::read_dir(drag_dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("lnk") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now
            .duration_since(modified)
            .map(|age| age > Duration::from_secs(3600))
            .unwrap_or(false)
        {
            let _ = fs::remove_file(path);
        }
    }
}

pub(crate) fn unique_shortcut_name(title: &str, target: &Path) -> String {
    let fallback;
    let source = if title.trim().is_empty() {
        fallback = path_display_name(target);
        fallback.as_str()
    } else {
        title
    };
    let base = sanitize_shortcut_stem(source);
    format!("{base}.lnk")
}

pub(crate) fn sanitize_shortcut_stem(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        if matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || ch.is_control() {
            output.push(' ');
        } else {
            output.push(ch);
        }
    }
    let output = output.split_whitespace().collect::<Vec<_>>().join(" ");
    if output.is_empty() {
        "Shortcut".to_string()
    } else {
        output.chars().take(80).collect()
    }
}

pub(crate) fn path_display_name(path: &Path) -> String {
    path.file_name()
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Shortcut".to_string())
}

pub(crate) fn shortcut_working_directory(target: &Path) -> String {
    if target.is_dir() {
        target.to_string_lossy().to_string()
    } else {
        target
            .parent()
            .map(|parent| parent.to_string_lossy().to_string())
            .unwrap_or_default()
    }
}

pub(crate) fn failed(hr: i32) -> bool {
    hr < 0
}

pub(crate) fn format_hresult(operation: &str, hr: i32) -> String {
    format!("{operation} failed: 0x{:08X}", hr as u32)
}

pub(crate) unsafe fn release_com(value: *mut c_void) {
    if !value.is_null() {
        let vtbl = *(value as *mut *mut IUnknownVTable);
        ((*vtbl).release)(value);
    }
}

pub(crate) fn shell_execute_error_message(code: isize, path: Option<&Path>) -> String {
    let base = match code {
        0 => "Out of memory or resources.",
        2 => "File was not found.",
        3 => "Path was not found.",
        5 => "Access denied.",
        8 => "Not enough memory.",
        11 => "Invalid executable file.",
        26 => "A sharing violation occurred.",
        27 => "File association is incomplete or invalid.",
        28 => "DDE transaction timed out.",
        29 => "DDE transaction failed.",
        30 => "DDE is busy.",
        31 => "No application is associated with this file type.",
        32 => "Dynamic-link library was not found.",
        _ => "Windows ShellExecute failed.",
    };
    let mut message = format!("{base} ShellExecute code: {code}.");
    if let Some(path) = path {
        if path.is_file() {
            if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
                message.push_str(&format!(" File extension: .{extension}."));
            }
        }
    }
    message
}

pub(crate) fn shell_properties_path(hwnd: HWND, path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err("Target path does not exist.".to_string());
    }
    let operation = wide("properties");
    let target = os_wide(path.as_os_str());
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_INVOKEIDLIST | SEE_MASK_NOASYNC;
        info.hwnd = hwnd;
        info.lpVerb = operation.as_ptr();
        info.lpFile = target.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut info) != 0 {
            return Ok(());
        }

        let result = ShellExecuteW(
            hwnd,
            operation.as_ptr(),
            target.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        ) as isize;
        if result > 32 {
            Ok(())
        } else {
            Err(shell_execute_error_message(result, Some(path)))
        }
    }
}

pub(crate) fn icon_cache_key(path: &Path, is_dir: bool) -> String {
    if is_dir {
        return "dir".to_string();
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "exe" | "lnk" | "ico" | "url" | "appref-ms"
    ) {
        return format!("path:{}", fold_text(&path.to_string_lossy()));
    }
    if extension.is_empty() {
        format!("path:{}", fold_text(&path.to_string_lossy()))
    } else {
        format!("ext:{extension}")
    }
}

pub(crate) fn load_path_icon(path: &Path) -> Option<HICON> {
    let target = os_wide(path.as_os_str());
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    unsafe {
        if SHGetFileInfoW(
            target.as_ptr(),
            0,
            &mut info,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON,
        ) == 0
            || info.hIcon.is_null()
        {
            return None;
        }
        Some(info.hIcon)
    }
}

pub(crate) unsafe fn draw_fallback_path_icon(hdc: HDC, x: i32, y: i32, is_dir: bool) {
    let fill = CreateSolidBrush(if is_dir { 0x0066_CCEE } else { 0x00F4_F4F4 });
    let border = CreateSolidBrush(0x0080_7060);
    if fill.is_null() || border.is_null() {
        if !fill.is_null() {
            DeleteObject(fill as _);
        }
        if !border.is_null() {
            DeleteObject(border as _);
        }
        return;
    }
    if is_dir {
        let tab = RECT {
            left: x + 3,
            top: y + 6,
            right: x + 16,
            bottom: y + 12,
        };
        let body = RECT {
            left: x + 2,
            top: y + 10,
            right: x + 30,
            bottom: y + 27,
        };
        FillRect(hdc, &tab, fill);
        FillRect(hdc, &body, fill);
        FrameRect(hdc, &tab, border);
        FrameRect(hdc, &body, border);
    } else {
        let document = RECT {
            left: x + 6,
            top: y + 3,
            right: x + 26,
            bottom: y + 29,
        };
        FillRect(hdc, &document, fill);
        FrameRect(hdc, &document, border);
    }
    DeleteObject(border as _);
    DeleteObject(fill as _);
}

pub(crate) fn result_target_path(result: &SearchResult) -> Option<PathBuf> {
    match &result.target {
        LaunchTarget::Path(path) => Some(path.clone()),
        LaunchTarget::Plugin(_) => None,
    }
}

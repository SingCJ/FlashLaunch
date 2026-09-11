use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::*;

pub(crate) fn set_current_thread_background_priority() {
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

pub(crate) fn fold_text(value: &str) -> String {
    value
        .chars()
        .map(fold_char)
        .collect::<String>()
        .to_lowercase()
}

pub(crate) fn searchable_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut last_was_space = false;
    for value_char in fold_text(value).chars() {
        let mapped = if value_char.is_alphanumeric() || value_char.is_whitespace() {
            value_char
        } else {
            ' '
        };
        if mapped.is_whitespace() {
            if !last_was_space {
                output.push(' ');
                last_was_space = true;
            }
        } else {
            output.push(mapped);
            last_was_space = false;
        }
    }
    output.trim().to_string()
}

pub(crate) fn fold_char(value: char) -> char {
    match value {
        '\u{00e0}' | '\u{00e1}' | '\u{1ea1}' | '\u{1ea3}' | '\u{00e3}' | '\u{00e2}'
        | '\u{1ea7}' | '\u{1ea5}' | '\u{1ead}' | '\u{1ea9}' | '\u{1eab}' | '\u{0103}'
        | '\u{1eb1}' | '\u{1eaf}' | '\u{1eb7}' | '\u{1eb3}' | '\u{1eb5}' => 'a',
        '\u{00c0}' | '\u{00c1}' | '\u{1ea0}' | '\u{1ea2}' | '\u{00c3}' | '\u{00c2}'
        | '\u{1ea6}' | '\u{1ea4}' | '\u{1eac}' | '\u{1ea8}' | '\u{1eaa}' | '\u{0102}'
        | '\u{1eb0}' | '\u{1eae}' | '\u{1eb6}' | '\u{1eb2}' | '\u{1eb4}' => 'A',
        '\u{00e8}' | '\u{00e9}' | '\u{1eb9}' | '\u{1ebb}' | '\u{1ebd}' | '\u{00ea}'
        | '\u{1ec1}' | '\u{1ebf}' | '\u{1ec7}' | '\u{1ec3}' | '\u{1ec5}' => 'e',
        '\u{00c8}' | '\u{00c9}' | '\u{1eb8}' | '\u{1eba}' | '\u{1ebc}' | '\u{00ca}'
        | '\u{1ec0}' | '\u{1ebe}' | '\u{1ec6}' | '\u{1ec2}' | '\u{1ec4}' => 'E',
        '\u{00ec}' | '\u{00ed}' | '\u{1ecb}' | '\u{1ec9}' | '\u{0129}' => 'i',
        '\u{00cc}' | '\u{00cd}' | '\u{1eca}' | '\u{1ec8}' | '\u{0128}' => 'I',
        '\u{00f2}' | '\u{00f3}' | '\u{1ecd}' | '\u{1ecf}' | '\u{00f5}' | '\u{00f4}'
        | '\u{1ed3}' | '\u{1ed1}' | '\u{1ed9}' | '\u{1ed5}' | '\u{1ed7}' | '\u{01a1}'
        | '\u{1edd}' | '\u{1edb}' | '\u{1ee3}' | '\u{1edf}' | '\u{1ee1}' => 'o',
        '\u{00d2}' | '\u{00d3}' | '\u{1ecc}' | '\u{1ece}' | '\u{00d5}' | '\u{00d4}'
        | '\u{1ed2}' | '\u{1ed0}' | '\u{1ed8}' | '\u{1ed4}' | '\u{1ed6}' | '\u{01a0}'
        | '\u{1edc}' | '\u{1eda}' | '\u{1ee2}' | '\u{1ede}' | '\u{1ee0}' => 'O',
        '\u{00f9}' | '\u{00fa}' | '\u{1ee5}' | '\u{1ee7}' | '\u{0169}' | '\u{01b0}'
        | '\u{1eeb}' | '\u{1ee9}' | '\u{1ef1}' | '\u{1eed}' | '\u{1eef}' => 'u',
        '\u{00d9}' | '\u{00da}' | '\u{1ee4}' | '\u{1ee6}' | '\u{0168}' | '\u{01af}'
        | '\u{1eea}' | '\u{1ee8}' | '\u{1ef0}' | '\u{1eec}' | '\u{1eee}' => 'U',
        '\u{1ef3}' | '\u{00fd}' | '\u{1ef5}' | '\u{1ef7}' | '\u{1ef9}' => 'y',
        '\u{1ef2}' | '\u{00dd}' | '\u{1ef4}' | '\u{1ef6}' | '\u{1ef8}' => 'Y',
        '\u{0111}' => 'd',
        '\u{0110}' => 'D',
        _ => value,
    }
}

pub(crate) fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

pub(crate) fn os_wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

pub(crate) fn copy_wide_fixed(target: &mut [u16], value: &str) {
    if target.is_empty() {
        return;
    }
    target.fill(0);
    let limit = target.len().saturating_sub(1);
    for (slot, unit) in target
        .iter_mut()
        .take(limit)
        .zip(OsStr::new(value).encode_wide())
    {
        *slot = unit;
    }
}

/// Writes NOTIFYICONDATAW.szTip through unaligned writes for x86 packed layouts.
pub(crate) unsafe fn set_notify_icon_tip(
    data: &mut windows_sys::Win32::UI::Shell::NOTIFYICONDATAW,
    value: &str,
) {
    let mut encoded = value.encode_utf16().collect::<Vec<_>>();
    encoded.truncate(127);
    encoded.push(0);
    let destination = std::ptr::addr_of_mut!((*data).szTip) as *mut u16;
    for (index, character) in encoded.into_iter().enumerate() {
        std::ptr::write_unaligned(destination.add(index), character);
    }
}

pub(crate) unsafe fn get_window_text(hwnd: HWND) -> String {
    let length = GetWindowTextLengthW(hwnd);
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
    OsString::from_wide(&buffer[..copied as usize])
        .to_string_lossy()
        .to_string()
}

pub(crate) fn set_window_text(hwnd: HWND, text: &str) {
    let text = wide(text);
    unsafe {
        SetWindowTextW(hwnd, text.as_ptr());
    }
}

pub(crate) unsafe fn configure_report_list_view(hwnd: HWND) {
    configure_report_list_view_with_options(hwnd, true, false);
}

pub(crate) unsafe fn configure_report_list_view_with_options(
    hwnd: HWND,
    checkboxes: bool,
    infotip: bool,
) {
    let mut style =
        LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES | LVS_EX_HEADERDRAGDROP | LVS_EX_DOUBLEBUFFER;
    if checkboxes {
        style |= LVS_EX_CHECKBOXES;
    }
    if infotip {
        style |= LVS_EX_INFOTIP;
    }
    SendMessageW(hwnd, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, style as isize);
}

pub(crate) unsafe fn reset_list_view_columns(hwnd: HWND, columns: &[(&str, i32)]) {
    let widths = fitted_list_view_column_widths(hwnd, columns);
    while SendMessageW(hwnd, LVM_DELETECOLUMN, columns.len(), 0) != 0 {}
    for (index, (title, _)) in columns.iter().enumerate() {
        let mut title = wide(title);
        let mut column: LVCOLUMNW = std::mem::zeroed();
        column.mask = LVCF_FMT | LVCF_WIDTH | LVCF_TEXT;
        column.fmt = LVCFMT_LEFT;
        column.cx = widths[index];
        column.pszText = title.as_mut_ptr();
        column.iSubItem = index as i32;
        if SendMessageW(hwnd, LVM_SETCOLUMNW, index, &column as *const _ as isize) == 0 {
            SendMessageW(hwnd, LVM_INSERTCOLUMNW, index, &column as *const _ as isize);
        }
    }
}

pub(crate) unsafe fn fitted_list_view_column_widths(
    hwnd: HWND,
    columns: &[(&str, i32)],
) -> Vec<i32> {
    let mut widths: Vec<i32> = columns.iter().map(|(_, width)| (*width).max(60)).collect();
    if widths.is_empty() {
        return widths;
    }
    let mut rect: RECT = std::mem::zeroed();
    if GetClientRect(hwnd, &mut rect) != 0 {
        let available_width = (rect.right - rect.left - 4).max(60);
        let fixed_width: i32 = widths.iter().take(widths.len().saturating_sub(1)).sum();
        let last = widths.len() - 1;
        widths[last] = widths[last].max(available_width - fixed_width).max(60);
    }
    widths
}

pub(crate) unsafe fn clear_list_view(hwnd: HWND) {
    SendMessageW(hwnd, LVM_DELETEALLITEMS, 0, 0);
}

pub(crate) unsafe fn list_view_item_count(hwnd: HWND) -> i32 {
    SendMessageW(hwnd, LVM_GETITEMCOUNT, 0, 0) as i32
}

pub(crate) unsafe fn list_view_selected_index(hwnd: HWND) -> i32 {
    SendMessageW(hwnd, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) as i32
}

pub(crate) unsafe fn list_view_selected_indices(hwnd: HWND) -> Vec<usize> {
    let mut indices = Vec::new();
    let mut current = -1isize;
    loop {
        current = SendMessageW(
            hwnd,
            LVM_GETNEXTITEM,
            current as usize,
            LVNI_SELECTED as isize,
        );
        if current < 0 {
            break;
        }
        indices.push(current as usize);
    }
    indices
}

pub(crate) unsafe fn select_all_list_view_items(hwnd: HWND) {
    let mut item: LVITEMW = std::mem::zeroed();
    item.stateMask = LVIS_SELECTED;
    item.state = LVIS_SELECTED;
    SendMessageW(
        hwnd,
        LVM_SETITEMSTATE,
        usize::MAX,
        &item as *const _ as isize,
    );
}

pub(crate) unsafe fn set_list_view_multi_select(hwnd: HWND, multi_select: bool) {
    let mut style = get_window_long(hwnd, GWL_STYLE);
    if multi_select {
        style &= !(LVS_SINGLESEL as isize);
    } else {
        style |= LVS_SINGLESEL as isize;
    }
    set_window_long(hwnd, GWL_STYLE, style);
    SetWindowPos(
        hwnd,
        null_mut(),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
    );
}

pub(crate) unsafe fn select_list_view_item(hwnd: HWND, index: usize) {
    let mut clear: LVITEMW = std::mem::zeroed();
    clear.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
    clear.state = 0;
    SendMessageW(
        hwnd,
        LVM_SETITEMSTATE,
        usize::MAX,
        &clear as *const _ as isize,
    );

    let mut item: LVITEMW = std::mem::zeroed();
    item.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
    item.state = LVIS_SELECTED | LVIS_FOCUSED;
    SendMessageW(hwnd, LVM_SETITEMSTATE, index, &item as *const _ as isize);
}

pub(crate) unsafe fn restore_list_view_selection(hwnd: HWND, indices: &[usize]) {
    let count = list_view_item_count(hwnd).max(0) as usize;
    let mut clear: LVITEMW = std::mem::zeroed();
    clear.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
    clear.state = 0;
    SendMessageW(
        hwnd,
        LVM_SETITEMSTATE,
        usize::MAX,
        &clear as *const _ as isize,
    );

    let mut first_selected = true;
    for &index in indices.iter().filter(|&&index| index < count) {
        let mut item: LVITEMW = std::mem::zeroed();
        item.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
        item.state = LVIS_SELECTED;
        if first_selected {
            item.state |= LVIS_FOCUSED;
            first_selected = false;
        }
        SendMessageW(hwnd, LVM_SETITEMSTATE, index, &item as *const _ as isize);
    }
}

pub(crate) unsafe fn add_list_view_row_with_param(
    hwnd: HWND,
    checked: bool,
    columns: &[&str],
    lparam: LPARAM,
) {
    let index = list_view_item_count(hwnd);
    let mut first = wide(columns.first().copied().unwrap_or_default());
    let mut item: LVITEMW = std::mem::zeroed();
    item.mask = LVIF_TEXT | LVIF_PARAM;
    item.iItem = index;
    item.iSubItem = 0;
    item.pszText = first.as_mut_ptr();
    item.lParam = lparam;
    SendMessageW(hwnd, LVM_INSERTITEMW, 0, &item as *const _ as isize);
    set_list_view_item_checked(hwnd, index, checked);

    for (subitem, value) in columns.iter().enumerate().skip(1) {
        let mut text = wide(value);
        let mut subitem_data: LVITEMW = std::mem::zeroed();
        subitem_data.iSubItem = subitem as i32;
        subitem_data.pszText = text.as_mut_ptr();
        SendMessageW(
            hwnd,
            LVM_SETITEMTEXTW,
            index as usize,
            &subitem_data as *const _ as isize,
        );
    }
}

pub(crate) unsafe fn set_list_view_item_param(hwnd: HWND, index: usize, lparam: LPARAM) {
    let mut item: LVITEMW = std::mem::zeroed();
    item.mask = LVIF_PARAM;
    item.iItem = index as i32;
    item.iSubItem = 0;
    item.lParam = lparam;
    SendMessageW(hwnd, LVM_SETITEMW, 0, &item as *const _ as isize);
}
pub(crate) unsafe fn set_list_view_item_checked(hwnd: HWND, index: i32, checked: bool) {
    let mut item: LVITEMW = std::mem::zeroed();
    item.stateMask = LVIS_STATEIMAGEMASK;
    item.state = if checked {
        LV_CHECKED_STATE
    } else {
        LV_UNCHECKED_STATE
    };
    SendMessageW(
        hwnd,
        LVM_SETITEMSTATE,
        index as usize,
        &item as *const _ as isize,
    );
}

pub(crate) fn set_edit_text_with_caret_at_end(hwnd: HWND, text: &str) {
    set_window_text(hwnd, text);
    unsafe {
        SendMessageW(
            hwnd,
            EM_SETSEL,
            text.encode_utf16().count(),
            text.encode_utf16().count() as isize,
        );
    }
}

pub(crate) fn set_combo_text_with_caret_at_end(hwnd: HWND, text: &str) {
    set_window_text(hwnd, text);
    unsafe {
        let position = text.encode_utf16().count() as u32;
        let selection = ((position & 0xffff) | ((position & 0xffff) << 16)) as isize;
        SetFocus(hwnd);
        SendMessageW(hwnd, CB_SETEDITSEL, 0, selection);
    }
}

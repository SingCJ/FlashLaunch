//! Pointer-sized Win32 (Windows 32-bit) API compatibility helpers.
use windows_sys::Win32::Foundation::HWND;

#[inline]
pub(crate) unsafe fn get_window_long(hwnd: HWND, index: i32) -> isize {
    #[cfg(target_pointer_width = "64")]
    {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, index)
    }
    #[cfg(target_pointer_width = "32")]
    {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongW(hwnd, index) as isize
    }
}

#[inline]
pub(crate) unsafe fn set_window_long(hwnd: HWND, index: i32, value: isize) -> isize {
    #[cfg(target_pointer_width = "64")]
    {
        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(hwnd, index, value)
    }
    #[cfg(target_pointer_width = "32")]
    {
        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongW(hwnd, index, value as i32)
            as isize
    }
}

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::null_mut;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    GetLastError, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, TRUE,
};
use windows_sys::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ, RGBQUAD,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::BELOW_NORMAL_PRIORITY_CLASS;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, DestroyIcon, GetIconInfo, HICON, ICONINFO, LR_DEFAULTCOLOR,
};

use crate::{load_path_icon, OleGuard};

const ICON_HELPER_ARGUMENT: &str = "--icon-helper";
const REQUEST_MAGIC: &[u8; 4] = b"FLIR";
const RESPONSE_MAGIC: &[u8; 4] = b"FLIO";
const PROTOCOL_VERSION: u8 = 1;
const FRAME_HEADER_LEN: usize = 18;
const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_WAIT: Duration = Duration::from_millis(250);
const REQUEST_LOAD: u8 = 0;
const REQUEST_SHUTDOWN: u8 = 1;
const RESPONSE_ICON: u8 = 0;
const RESPONSE_MISSING: u8 = 1;
const RESPONSE_ERROR: u8 = 2;
const RESPONSE_READY: u8 = 3;

pub(crate) fn run_icon_helper_if_requested<I>(args: I) -> Option<i32>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let first = args.next()?;
    if first != OsStr::new(ICON_HELPER_ARGUMENT) {
        return None;
    }
    if args.next().is_some() {
        return Some(2);
    }
    Some(run_icon_helper_server())
}

pub(crate) struct IconHelperClient {
    process: Option<IconHelperProcess>,
    next_request_id: u64,
}

struct IconHelperProcess {
    helper: crate::helper_process::HelperProcess,
}

impl IconHelperClient {
    pub(crate) fn start() -> io::Result<Self> {
        let mut process = spawn_icon_helper()?;
        let startup_result =
            read_frame_with_timeout(process.helper.stdout_mut(), RESPONSE_MAGIC, STARTUP_TIMEOUT)
                .and_then(|(status, response_id, payload)| {
                    validate_ready_response(status, response_id, &payload)
                });
        if let Err(error) = startup_result {
            process.helper.close_stdin();
            let _ = process.helper.terminate_and_wait();
            return Err(io::Error::new(
                error.kind(),
                format!("Icon helper startup failed: {error}"),
            ));
        }
        Ok(Self {
            process: Some(process),
            next_request_id: 1,
        })
    }

    pub(crate) fn load_icon(&mut self, path: &Path) -> io::Result<Option<HICON>> {
        let request_id = self.take_request_id();
        match self.transact(request_id, path) {
            Ok(icon) => Ok(icon),
            Err(error) => {
                self.stop_process();
                Err(io::Error::new(
                    error.kind(),
                    format!(
                        "Icon helper communication failed for {}: {error}",
                        path.display()
                    ),
                ))
            }
        }
    }

    fn transact(&mut self, request_id: u64, path: &Path) -> io::Result<Option<HICON>> {
        let process = self
            .process
            .as_mut()
            .ok_or_else(|| io::Error::other("Icon helper is not running."))?;
        let request = encode_frame(
            REQUEST_MAGIC,
            REQUEST_LOAD,
            request_id,
            &encode_wide_payload(path.as_os_str()),
        )?;
        process.helper.stdin_mut()?.write_all(&request)?;
        process.helper.stdin_mut()?.flush()?;
        let (status, response_id, payload) = read_frame_with_timeout(
            process.helper.stdout_mut(),
            RESPONSE_MAGIC,
            RESPONSE_TIMEOUT,
        )?;
        if response_id != request_id {
            return Err(invalid_data(
                "Icon helper response ID does not match the request.",
            ));
        }
        match status {
            RESPONSE_ICON if !payload.is_empty() => {
                let icon = create_icon_from_resource(&payload)?;
                Ok(Some(icon))
            }
            RESPONSE_MISSING if payload.is_empty() => Ok(None),
            RESPONSE_ERROR => Err(io::Error::other(
                decode_wide_os_payload(&payload)?
                    .to_string_lossy()
                    .into_owned(),
            )),
            _ => Err(invalid_data("Icon helper response payload is invalid.")),
        }
    }

    fn take_request_id(&mut self) -> u64 {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        request_id
    }

    fn stop_process(&mut self) {
        if let Some(mut process) = self.process.take() {
            process.helper.close_stdin();
            let _ = process.helper.terminate_and_wait();
        }
    }

    pub(crate) fn shutdown(&mut self) {
        let Some(mut process) = self.process.take() else {
            return;
        };
        let request_id = self.take_request_id();
        if let Ok(frame) = encode_frame(REQUEST_MAGIC, REQUEST_SHUTDOWN, request_id, &[]) {
            if let Ok(stdin) = process.helper.stdin_mut() {
                let _ = stdin.write_all(&frame);
                let _ = stdin.flush();
            }
        }
        process.helper.close_stdin();
        if process.helper.wait_timeout(SHUTDOWN_WAIT).unwrap_or(false) {
            return;
        }
        let _ = process.helper.terminate_and_wait();
    }
}

impl Drop for IconHelperClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn spawn_icon_helper() -> io::Result<IconHelperProcess> {
    let executable = std::env::current_exe()?;
    let helper = crate::helper_process::HelperProcess::spawn(
        &executable,
        OsStr::new(ICON_HELPER_ARGUMENT),
        BELOW_NORMAL_PRIORITY_CLASS,
    )?;
    Ok(IconHelperProcess { helper })
}

fn run_icon_helper_server() -> i32 {
    if let Err(error) = clear_standard_handle_inheritance() {
        crate::log_helper_fatal_error("Icon", "clearing standard handle inheritance", &error);
        return 2;
    }
    let _ole_guard = unsafe { OleGuard::initialize() };
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let ready = match encode_frame(RESPONSE_MAGIC, RESPONSE_READY, 0, &[]) {
        Ok(ready) => ready,
        Err(error) => {
            crate::log_helper_fatal_error("Icon", "encoding the ready response", &error);
            return 2;
        }
    };
    if let Err(error) = output.write_all(&ready).and_then(|_| output.flush()) {
        crate::log_helper_fatal_error("Icon", "writing the ready response", &error);
        return 2;
    }
    loop {
        let frame = match read_frame(&mut input, REQUEST_MAGIC) {
            Ok(Some(frame)) => frame,
            Ok(None) => return 0,
            Err(error) => {
                crate::log_helper_fatal_error("Icon", "reading a request", &error);
                return 2;
            }
        };
        let (kind, request_id, payload) = frame;
        if kind == REQUEST_SHUTDOWN && payload.is_empty() {
            return 0;
        }
        if kind != REQUEST_LOAD {
            if let Err(error) =
                write_error_response(&mut output, request_id, "Unknown icon helper request.")
            {
                crate::log_helper_fatal_error("Icon", "writing an error response", &error);
                return 2;
            }
            continue;
        }
        let path = match decode_wide_os_payload(&payload) {
            Ok(value) => std::path::PathBuf::from(value),
            Err(error) => {
                if let Err(write_error) =
                    write_error_response(&mut output, request_id, &error.to_string())
                {
                    crate::log_helper_fatal_error(
                        "Icon",
                        "writing a payload error response",
                        &write_error,
                    );
                    return 2;
                }
                continue;
            }
        };
        let response = match load_path_icon(&path) {
            Some(icon) => {
                let encoded = encode_icon_resource(icon);
                unsafe { DestroyIcon(icon) };
                match encoded {
                    Ok(payload) => {
                        encode_frame(RESPONSE_MAGIC, RESPONSE_ICON, request_id, &payload)
                    }
                    Err(error) => encode_frame(
                        RESPONSE_MAGIC,
                        RESPONSE_ERROR,
                        request_id,
                        &encode_wide_payload(OsStr::new(&error.to_string())),
                    ),
                }
            }
            None => encode_frame(RESPONSE_MAGIC, RESPONSE_MISSING, request_id, &[]),
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                crate::log_helper_fatal_error("Icon", "encoding a response", &error);
                return 2;
            }
        };
        if let Err(error) = output.write_all(&response).and_then(|_| output.flush()) {
            crate::log_helper_fatal_error("Icon", "writing a response", &error);
            return 2;
        }
    }
}

fn validate_ready_response(status: u8, response_id: u64, payload: &[u8]) -> io::Result<()> {
    if status == RESPONSE_READY && response_id == 0 && payload.is_empty() {
        Ok(())
    } else {
        Err(invalid_data("Icon helper ready response is invalid."))
    }
}

fn write_error_response(output: &mut impl Write, request_id: u64, message: &str) -> io::Result<()> {
    let frame = encode_frame(
        RESPONSE_MAGIC,
        RESPONSE_ERROR,
        request_id,
        &encode_wide_payload(OsStr::new(message)),
    )?;
    output.write_all(&frame)?;
    output.flush()
}

fn clear_standard_handle_inheritance() -> io::Result<()> {
    for standard_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE] {
        let handle = unsafe { GetStdHandle(standard_handle) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            let code = unsafe { GetLastError() };
            return Err(io::Error::from_raw_os_error(code as i32));
        }
    }
    Ok(())
}

fn encode_frame(magic: &[u8; 4], kind: u8, request_id: u64, payload: &[u8]) -> io::Result<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(invalid_data("Icon helper payload is too large."));
    }
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    frame.extend_from_slice(magic);
    frame.push(PROTOCOL_VERSION);
    frame.push(kind);
    frame.extend_from_slice(&request_id.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

fn read_frame(reader: &mut impl Read, magic: &[u8; 4]) -> io::Result<Option<(u8, u64, Vec<u8>)>> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    let mut first = [0u8; 1];
    match reader.read(&mut first) {
        Ok(0) => return Ok(None),
        Ok(1) => header[0] = first[0],
        Ok(_) => unreachable!(),
        Err(error) => return Err(error),
    }
    reader.read_exact(&mut header[1..])?;
    let payload_len = validate_header(&header, magic)?;
    let mut payload = vec![0u8; payload_len];
    reader.read_exact(&mut payload)?;
    Ok(Some((
        header[5],
        u64::from_le_bytes(header[6..14].try_into().unwrap()),
        payload,
    )))
}

fn read_frame_with_timeout(
    reader: &mut std::fs::File,
    magic: &[u8; 4],
    timeout: Duration,
) -> io::Result<(u8, u64, Vec<u8>)> {
    let deadline = Instant::now() + timeout;
    let mut header = [0u8; FRAME_HEADER_LEN];
    read_exact_with_deadline(reader, &mut header, deadline)?;
    let payload_len = validate_header(&header, magic)?;
    let mut payload = vec![0u8; payload_len];
    read_exact_with_deadline(reader, &mut payload, deadline)?;
    Ok((
        header[5],
        u64::from_le_bytes(header[6..14].try_into().unwrap()),
        payload,
    ))
}

fn read_exact_with_deadline(
    reader: &mut std::fs::File,
    buffer: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    let handle = reader.as_raw_handle() as HANDLE;
    let mut offset = 0usize;
    while offset < buffer.len() {
        let mut available = 0u32;
        if unsafe {
            PeekNamedPipe(
                handle,
                null_mut(),
                0,
                null_mut(),
                &mut available,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if available == 0 {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Icon helper response timed out.",
                ));
            }
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        let count = (available as usize).min(buffer.len() - offset);
        let read = reader.read(&mut buffer[offset..offset + count])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Icon helper closed its output pipe.",
            ));
        }
        offset += read;
    }
    Ok(())
}

fn validate_header(header: &[u8; FRAME_HEADER_LEN], magic: &[u8; 4]) -> io::Result<usize> {
    if &header[..4] != magic {
        return Err(invalid_data("Icon helper frame magic is invalid."));
    }
    if header[4] != PROTOCOL_VERSION {
        return Err(invalid_data("Icon helper frame version is unsupported."));
    }
    let payload_len = u32::from_le_bytes(header[14..18].try_into().unwrap()) as usize;
    if payload_len > MAX_PAYLOAD_BYTES {
        return Err(invalid_data("Icon helper frame payload is too large."));
    }
    Ok(payload_len)
}

fn encode_wide_payload(value: &OsStr) -> Vec<u8> {
    value
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>()
}

fn decode_wide_os_payload(payload: &[u8]) -> io::Result<OsString> {
    if payload.len() % 2 != 0 {
        return Err(invalid_data(
            "Icon helper UTF-16 payload length is invalid.",
        ));
    }
    let units = payload
        .chunks_exact(2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .collect::<Vec<_>>();
    Ok(OsString::from_wide(&units))
}

fn create_icon_from_resource(resource: &[u8]) -> io::Result<HICON> {
    let icon = unsafe {
        CreateIconFromResourceEx(
            resource.as_ptr(),
            resource.len() as u32,
            TRUE,
            0x0003_0000,
            0,
            0,
            LR_DEFAULTCOLOR,
        )
    };
    if icon.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(icon)
    }
}

fn encode_icon_resource(icon: HICON) -> io::Result<Vec<u8>> {
    let mut info: ICONINFO = unsafe { std::mem::zeroed() };
    if unsafe { GetIconInfo(icon, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = if !info.hbmColor.is_null() {
        encode_color_icon(info.hbmColor, info.hbmMask)
    } else {
        encode_monochrome_icon(info.hbmMask)
    };
    if !info.hbmColor.is_null() {
        unsafe { DeleteObject(info.hbmColor as HGDIOBJ) };
    }
    if !info.hbmMask.is_null() {
        unsafe { DeleteObject(info.hbmMask as HGDIOBJ) };
    }
    result
}

fn encode_color_icon(color: HBITMAP, mask: HBITMAP) -> io::Result<Vec<u8>> {
    let color_bitmap = bitmap_details(color)?;
    let width = color_bitmap.bmWidth.abs();
    let height = color_bitmap.bmHeight.abs();
    if width == 0 || height == 0 || mask.is_null() {
        return Err(invalid_data("Icon color bitmap dimensions are invalid."));
    }
    let color_bits = read_bitmap_bits(color, width, height, 32)?;
    let mask_bits = read_bitmap_bits(mask, width, height, 1)?;
    let mut resource = encode_bitmap_header(width, height * 2, 32, color_bits.len() as u32);
    resource.extend_from_slice(&color_bits);
    resource.extend_from_slice(&mask_bits);
    Ok(resource)
}

fn encode_monochrome_icon(mask: HBITMAP) -> io::Result<Vec<u8>> {
    if mask.is_null() {
        return Err(invalid_data("Monochrome icon mask is missing."));
    }
    let mask_bitmap = bitmap_details(mask)?;
    let width = mask_bitmap.bmWidth.abs();
    let total_height = mask_bitmap.bmHeight.abs();
    if width == 0 || total_height == 0 || total_height % 2 != 0 {
        return Err(invalid_data("Monochrome icon dimensions are invalid."));
    }
    let bits = read_bitmap_bits(mask, width, total_height, 1)?;
    let mut resource = encode_bitmap_header(width, total_height, 1, bits.len() as u32);
    resource.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 0]);
    resource.extend_from_slice(&bits);
    Ok(resource)
}

fn bitmap_details(bitmap: HBITMAP) -> io::Result<BITMAP> {
    let mut info: BITMAP = unsafe { std::mem::zeroed() };
    let read = unsafe {
        GetObjectW(
            bitmap as HGDIOBJ,
            std::mem::size_of::<BITMAP>() as i32,
            &mut info as *mut BITMAP as *mut _,
        )
    };
    if read == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(info)
    }
}

#[repr(C)]
struct MonoBitmapInfo {
    header: BITMAPINFOHEADER,
    colors: [RGBQUAD; 2],
}

fn read_bitmap_bits(
    bitmap: HBITMAP,
    width: i32,
    height: i32,
    bit_count: u16,
) -> io::Result<Vec<u8>> {
    let stride = (((width as usize * bit_count as usize) + 31) / 32) * 4;
    let mut bits = vec![0u8; stride * height as usize];
    let mut info = MonoBitmapInfo {
        header: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: height,
            biPlanes: 1,
            biBitCount: bit_count,
            biCompression: BI_RGB,
            biSizeImage: bits.len() as u32,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: if bit_count == 1 { 2 } else { 0 },
            biClrImportant: 0,
        },
        colors: unsafe { std::mem::zeroed() },
    };
    let dc = unsafe { GetDC(null_mut()) };
    if dc.is_null() {
        return Err(io::Error::last_os_error());
    }
    let scan_lines = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            height as u32,
            bits.as_mut_ptr() as *mut _,
            &mut info as *mut MonoBitmapInfo as *mut BITMAPINFO,
            DIB_RGB_COLORS,
        )
    };
    unsafe { ReleaseDC(null_mut(), dc) };
    if scan_lines != height {
        Err(io::Error::last_os_error())
    } else {
        Ok(bits)
    }
}

fn encode_bitmap_header(width: i32, height: i32, bit_count: u16, image_size: u32) -> Vec<u8> {
    let mut header = Vec::with_capacity(40);
    header.extend_from_slice(&40u32.to_le_bytes());
    header.extend_from_slice(&width.to_le_bytes());
    header.extend_from_slice(&height.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&bit_count.to_le_bytes());
    header.extend_from_slice(&BI_RGB.to_le_bytes());
    header.extend_from_slice(&image_size.to_le_bytes());
    header.extend_from_slice(&0i32.to_le_bytes());
    header.extend_from_slice(&0i32.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    header
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use windows_sys::Win32::UI::WindowsAndMessaging::{LoadIconW, IDI_APPLICATION};

    use super::*;

    #[test]
    fn frame_codec_preserves_unicode_path_and_request_id() {
        let path = OsStr::new(r"C:\Dữ liệu\Ứng dụng\công cụ.exe");
        let encoded =
            encode_frame(REQUEST_MAGIC, REQUEST_LOAD, 42, &encode_wide_payload(path)).unwrap();
        let mut cursor = Cursor::new(encoded);
        let (kind, request_id, payload) = read_frame(&mut cursor, REQUEST_MAGIC).unwrap().unwrap();
        assert_eq!(kind, REQUEST_LOAD);
        assert_eq!(request_id, 42);
        assert_eq!(decode_wide_os_payload(&payload).unwrap(), path);
    }

    #[test]
    fn frame_codec_rejects_bad_magic_version_and_length() {
        let valid = encode_frame(REQUEST_MAGIC, REQUEST_LOAD, 1, &[]).unwrap();
        let mut bad_magic = valid.clone();
        bad_magic[0] = b'X';
        assert!(read_frame(&mut Cursor::new(bad_magic), REQUEST_MAGIC).is_err());

        let mut bad_version = valid.clone();
        bad_version[4] = PROTOCOL_VERSION + 1;
        assert!(read_frame(&mut Cursor::new(bad_version), REQUEST_MAGIC).is_err());

        let mut bad_length = valid;
        bad_length[14..18].copy_from_slice(&1u32.to_le_bytes());
        assert!(read_frame(&mut Cursor::new(bad_length), REQUEST_MAGIC).is_err());
    }

    #[test]
    fn ready_response_requires_reserved_status_id_and_empty_payload() {
        assert!(validate_ready_response(RESPONSE_READY, 0, &[]).is_ok());
        assert!(validate_ready_response(RESPONSE_ICON, 0, &[]).is_err());
        assert!(validate_ready_response(RESPONSE_READY, 1, &[]).is_err());
        assert!(validate_ready_response(RESPONSE_READY, 0, &[1]).is_err());
    }

    #[test]
    fn icon_resource_round_trip_creates_process_local_icon() {
        let source = unsafe { LoadIconW(null_mut(), IDI_APPLICATION) };
        assert!(!source.is_null());
        let resource = encode_icon_resource(source).unwrap();
        let recreated = create_icon_from_resource(&resource).unwrap();
        assert!(!recreated.is_null());
        unsafe { DestroyIcon(recreated) };
    }
}

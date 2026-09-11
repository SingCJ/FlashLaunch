use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    GetLastError, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, HWND, INVALID_HANDLE_VALUE,
    MAX_PATH,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::UI::Shell::{
    ILFree, SHBrowseForFolderW, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS,
    BROWSEINFOW,
};

use crate::{
    is_shortcut_file, linked_target_folder, resolve_shortcut_target, shell_explore_path_result,
    shell_launch_path_result, OleGuard,
};

const HELPER_ARGUMENT: &str = "--shell-helper";
const REQUEST_MAGIC: &[u8; 4] = b"FLSQ";
const RESPONSE_MAGIC: &[u8; 4] = b"FLSR";
const PROTOCOL_VERSION: u8 = 1;
const FRAME_HEADER_LEN: usize = 18;
const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const REQUEST_SHUTDOWN: u8 = 0xFF;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const SHELL_UI_RESPONSE_TIMEOUT: Duration = Duration::from_secs(600);
const SHUTDOWN_WAIT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShellOperation {
    Launch,
    Open,
    OpenLinkedLocation,
    Properties,
    ContextMenu,
}

impl ShellOperation {
    fn code(self) -> u8 {
        match self {
            Self::Launch => 0,
            Self::Open => 1,
            Self::OpenLinkedLocation => 2,
            Self::Properties => 3,
            Self::ContextMenu => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Launch),
            1 => Some(Self::Open),
            2 => Some(Self::OpenLinkedLocation),
            3 => Some(Self::Properties),
            4 => Some(Self::ContextMenu),
            _ => None,
        }
    }

    fn response_timeout(self) -> Duration {
        if matches!(self, Self::Properties | Self::ContextMenu) {
            SHELL_UI_RESPONSE_TIMEOUT
        } else {
            RESPONSE_TIMEOUT
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ShellHelperResult {
    Success,
    MissingShortcutTarget(PathBuf),
    NoLinkedLocation,
    SelectedPath(PathBuf),
    Cancelled,
    Error(String),
}

pub(crate) fn run_shell_helper_if_requested<I>(args: I) -> Option<i32>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let first = args.next()?;
    if first != OsStr::new(HELPER_ARGUMENT) {
        return None;
    }
    if args.next().is_some() {
        return Some(2);
    }
    Some(run_shell_helper_server())
}

pub(crate) struct ShellHelperClient {
    process: crate::helper_process::HelperProcess,
    next_request_id: u64,
}

impl ShellHelperClient {
    pub(crate) fn start() -> io::Result<Self> {
        let executable = std::env::current_exe()?;
        let process = crate::helper_process::HelperProcess::spawn(
            &executable,
            OsStr::new(HELPER_ARGUMENT),
            0,
        )?;
        Ok(Self {
            process,
            next_request_id: 1,
        })
    }

    pub(crate) fn transact(
        &mut self,
        operation: ShellOperation,
        path: &Path,
    ) -> io::Result<ShellHelperResult> {
        let request_id = self.take_request_id();
        let frame = encode_frame(
            REQUEST_MAGIC,
            operation.code(),
            request_id,
            &encode_wide_payload(path.as_os_str()),
        )?;
        self.process.stdin_mut()?.write_all(&frame)?;
        self.process.stdin_mut()?.flush()?;
        let (status, response_id, payload) = read_frame_with_timeout(
            self.process.stdout_mut(),
            RESPONSE_MAGIC,
            operation.response_timeout(),
        )?;
        if response_id != request_id {
            return Err(invalid_data(
                "Shell helper response ID does not match the request.",
            ));
        }
        decode_result(status, &payload)
    }

    fn take_request_id(&mut self) -> u64 {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        request_id
    }

    pub(crate) fn shutdown(mut self) {
        let request_id = self.take_request_id();
        if let Ok(frame) = encode_frame(REQUEST_MAGIC, REQUEST_SHUTDOWN, request_id, &[]) {
            if let Ok(stdin) = self.process.stdin_mut() {
                let _ = stdin.write_all(&frame);
                let _ = stdin.flush();
            }
        }
        self.process.close_stdin();
        if self.process.wait_timeout(SHUTDOWN_WAIT).unwrap_or(false) {
            return;
        }
        let _ = self.process.terminate_and_wait();
    }

    pub(crate) fn abort(&mut self) {
        self.process.close_stdin();
        let _ = self.process.terminate_and_wait();
    }
}

pub(crate) fn run_browse_folder_direct(owner: HWND, title: &str) -> Option<PathBuf> {
    match browse_for_folder(owner, OsStr::new(title)) {
        ShellHelperResult::SelectedPath(path) => Some(path),
        _ => None,
    }
}

fn run_shell_helper_server() -> i32 {
    if let Err(error) = clear_standard_handle_inheritance() {
        crate::log_helper_fatal_error("Shell", "clearing standard handle inheritance", &error);
        return 2;
    }
    let _ole_guard = unsafe { OleGuard::initialize() };
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    loop {
        let (kind, request_id, payload) = match read_frame(&mut input, REQUEST_MAGIC) {
            Ok(Some(frame)) => frame,
            Ok(None) => return 0,
            Err(error) => {
                crate::log_helper_fatal_error("Shell", "reading a request", &error);
                return 2;
            }
        };
        if kind == REQUEST_SHUTDOWN && payload.is_empty() {
            return 0;
        }
        let result = match ShellOperation::from_code(kind) {
            Some(operation) => match decode_wide_payload(&payload) {
                Ok(path) => execute_operation(operation, &PathBuf::from(path)),
                Err(error) => ShellHelperResult::Error(error.to_string()),
            },
            None => ShellHelperResult::Error("Unknown Shell helper operation.".to_string()),
        };
        let (status, response_payload) = encode_result(&result);
        let frame = match encode_frame(RESPONSE_MAGIC, status, request_id, &response_payload) {
            Ok(frame) => frame,
            Err(error) => {
                crate::log_helper_fatal_error("Shell", "encoding a response", &error);
                return 2;
            }
        };
        if let Err(error) = output.write_all(&frame).and_then(|_| output.flush()) {
            crate::log_helper_fatal_error("Shell", "writing a response", &error);
            return 2;
        }
    }
}

fn clear_standard_handle_inheritance() -> io::Result<()> {
    for standard_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE] {
        let handle = unsafe { GetStdHandle(standard_handle) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { GetLastError() } as i32
            ));
        }
    }
    Ok(())
}

fn encode_frame(magic: &[u8; 4], kind: u8, request_id: u64, payload: &[u8]) -> io::Result<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD_BYTES || payload.len() > u32::MAX as usize {
        return Err(invalid_data("Shell helper payload is too large."));
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

fn read_frame<R: Read>(
    reader: &mut R,
    expected_magic: &[u8; 4],
) -> io::Result<Option<(u8, u64, Vec<u8>)>> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    let mut first = [0u8; 1];
    let read = reader.read(&mut first)?;
    if read == 0 {
        return Ok(None);
    }
    header[0] = first[0];
    reader.read_exact(&mut header[1..])?;
    if &header[..4] != expected_magic {
        return Err(invalid_data("Shell helper frame magic is invalid."));
    }
    if header[4] != PROTOCOL_VERSION {
        return Err(invalid_data(
            "Shell helper protocol version is unsupported.",
        ));
    }
    let kind = header[5];
    let request_id = u64::from_le_bytes(header[6..14].try_into().unwrap());
    let payload_len = u32::from_le_bytes(header[14..18].try_into().unwrap()) as usize;
    if payload_len > MAX_PAYLOAD_BYTES {
        return Err(invalid_data("Shell helper frame payload is too large."));
    }
    let mut payload = vec![0u8; payload_len];
    reader.read_exact(&mut payload)?;
    Ok(Some((kind, request_id, payload)))
}

fn read_frame_with_timeout(
    reader: &mut std::fs::File,
    expected_magic: &[u8; 4],
    timeout: Duration,
) -> io::Result<(u8, u64, Vec<u8>)> {
    let deadline = Instant::now() + timeout;
    let mut header = [0u8; FRAME_HEADER_LEN];
    read_exact_with_deadline(reader, &mut header, deadline)?;
    if &header[..4] != expected_magic {
        return Err(invalid_data("Shell helper frame magic is invalid."));
    }
    if header[4] != PROTOCOL_VERSION {
        return Err(invalid_data(
            "Shell helper protocol version is unsupported.",
        ));
    }
    let payload_len = u32::from_le_bytes(header[14..18].try_into().unwrap()) as usize;
    if payload_len > MAX_PAYLOAD_BYTES {
        return Err(invalid_data("Shell helper frame payload is too large."));
    }
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
                    "Shell helper response timed out.",
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
                "Shell helper closed its output pipe.",
            ));
        }
        offset += read;
    }
    Ok(())
}

fn encode_result(result: &ShellHelperResult) -> (u8, Vec<u8>) {
    match result {
        ShellHelperResult::Success => (0, Vec::new()),
        ShellHelperResult::MissingShortcutTarget(path) => {
            (1, encode_wide_payload(path.as_os_str()))
        }
        ShellHelperResult::NoLinkedLocation => (2, Vec::new()),
        ShellHelperResult::Error(error) => (3, encode_wide_payload(OsStr::new(error))),
        ShellHelperResult::SelectedPath(path) => (4, encode_wide_payload(path.as_os_str())),
        ShellHelperResult::Cancelled => (5, Vec::new()),
    }
}

fn decode_result(status: u8, payload: &[u8]) -> io::Result<ShellHelperResult> {
    match status {
        0 if payload.is_empty() => Ok(ShellHelperResult::Success),
        1 if !payload.is_empty() => Ok(ShellHelperResult::MissingShortcutTarget(PathBuf::from(
            decode_wide_payload(payload)?,
        ))),
        2 if payload.is_empty() => Ok(ShellHelperResult::NoLinkedLocation),
        3 => Ok(ShellHelperResult::Error(
            decode_wide_payload(payload)?.to_string_lossy().into_owned(),
        )),
        4 if !payload.is_empty() => Ok(ShellHelperResult::SelectedPath(PathBuf::from(
            decode_wide_payload(payload)?,
        ))),
        5 if payload.is_empty() => Ok(ShellHelperResult::Cancelled),
        _ => Err(invalid_data("Shell helper response payload is invalid.")),
    }
}

fn encode_wide_payload(value: &OsStr) -> Vec<u8> {
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

fn decode_wide_payload(payload: &[u8]) -> io::Result<OsString> {
    if payload.len() % 2 != 0 {
        return Err(invalid_data(
            "Shell helper UTF-16 payload has an odd length.",
        ));
    }
    let units = payload
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Ok(OsString::from_wide(&units))
}

fn browse_for_folder(owner: HWND, title: &OsStr) -> ShellHelperResult {
    let title = title
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut display = [0u16; MAX_PATH as usize];
    let info = BROWSEINFOW {
        hwndOwner: owner,
        pidlRoot: null_mut(),
        pszDisplayName: display.as_mut_ptr(),
        lpszTitle: title.as_ptr(),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        lpfn: None,
        lParam: 0,
        iImage: 0,
    };
    let pidl = unsafe { SHBrowseForFolderW(&info) };
    if pidl.is_null() {
        return ShellHelperResult::Cancelled;
    }
    let mut path_buffer = [0u16; MAX_PATH as usize];
    let ok = unsafe { SHGetPathFromIDListW(pidl, path_buffer.as_mut_ptr()) } != 0;
    unsafe { ILFree(pidl) };
    if !ok {
        return ShellHelperResult::Error("Could not read the selected folder path.".to_string());
    }
    let length = path_buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(path_buffer.len());
    ShellHelperResult::SelectedPath(PathBuf::from(OsString::from_wide(&path_buffer[..length])))
}

fn execute_operation(operation: ShellOperation, path: &Path) -> ShellHelperResult {
    match operation {
        ShellOperation::Launch => {
            if is_shortcut_file(path) {
                if let Some(target) = resolve_shortcut_target(path) {
                    if !target.exists() {
                        return ShellHelperResult::MissingShortcutTarget(target);
                    }
                }
            }
            shell_launch_path_result(null_mut(), path)
                .map(|_| ShellHelperResult::Success)
                .unwrap_or_else(ShellHelperResult::Error)
        }
        ShellOperation::Open => shell_explore_path_result(null_mut(), path)
            .map(|_| ShellHelperResult::Success)
            .unwrap_or_else(ShellHelperResult::Error),
        ShellOperation::OpenLinkedLocation => {
            let Some(folder) = linked_target_folder(path) else {
                return ShellHelperResult::NoLinkedLocation;
            };
            shell_explore_path_result(null_mut(), &folder)
                .map(|_| ShellHelperResult::Success)
                .unwrap_or_else(ShellHelperResult::Error)
        }
        ShellOperation::Properties => crate::show_shell_properties_dialog(path)
            .map(|_| ShellHelperResult::Success)
            .unwrap_or_else(ShellHelperResult::Error),
        ShellOperation::ContextMenu => crate::show_shell_context_menu(path)
            .map(|_| ShellHelperResult::Success)
            .unwrap_or_else(ShellHelperResult::Error),
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_codes_round_trip() {
        for operation in [
            ShellOperation::Launch,
            ShellOperation::Open,
            ShellOperation::OpenLinkedLocation,
            ShellOperation::Properties,
            ShellOperation::ContextMenu,
        ] {
            assert_eq!(ShellOperation::from_code(operation.code()), Some(operation));
        }
        assert_eq!(ShellOperation::from_code(99), None);
        assert_eq!(
            ShellOperation::ContextMenu.response_timeout(),
            SHELL_UI_RESPONSE_TIMEOUT
        );
        assert_eq!(
            ShellOperation::Properties.response_timeout(),
            SHELL_UI_RESPONSE_TIMEOUT
        );
    }

    #[test]
    fn request_frame_preserves_unicode_path() {
        let path = OsStr::new(r"C:\Dữ liệu\Ứng dụng\công cụ.exe");
        let frame = encode_frame(
            REQUEST_MAGIC,
            ShellOperation::Launch.code(),
            42,
            &encode_wide_payload(path),
        )
        .unwrap();
        let (_, request_id, payload) = read_frame(&mut frame.as_slice(), REQUEST_MAGIC)
            .unwrap()
            .unwrap();
        assert_eq!(request_id, 42);
        assert_eq!(decode_wide_payload(&payload).unwrap(), path);
    }

    #[test]
    fn result_codec_round_trips_all_statuses() {
        let results = [
            ShellHelperResult::Success,
            ShellHelperResult::MissingShortcutTarget(PathBuf::from(r"C:\missing.exe")),
            ShellHelperResult::NoLinkedLocation,
            ShellHelperResult::SelectedPath(PathBuf::from(r"C:\Folder")),
            ShellHelperResult::Cancelled,
            ShellHelperResult::Error("Detailed helper error.".to_string()),
        ];
        for expected in results {
            let (status, payload) = encode_result(&expected);
            assert_eq!(decode_result(status, &payload).unwrap(), expected);
        }
    }

    #[test]
    fn frame_rejects_invalid_magic_and_version() {
        let mut frame = encode_frame(REQUEST_MAGIC, 0, 1, &[]).unwrap();
        frame[0] = b'X';
        assert!(read_frame(&mut frame.as_slice(), REQUEST_MAGIC).is_err());

        let mut frame = encode_frame(REQUEST_MAGIC, 0, 1, &[]).unwrap();
        frame[4] = PROTOCOL_VERSION + 1;
        assert!(read_frame(&mut frame.as_slice(), REQUEST_MAGIC).is_err());
    }
}

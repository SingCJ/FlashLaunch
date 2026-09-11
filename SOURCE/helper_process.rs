use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, SetHandleInformation, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, TRUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_NO_WINDOW, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    STARTF_FORCEOFFFEEDBACK, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

pub(crate) const HELPER_STARTUP_FLAGS: u32 = STARTF_FORCEOFFFEEDBACK | STARTF_USESTDHANDLES;

pub(crate) struct HelperProcess {
    process: OwnedHandle,
    stdin: Option<File>,
    stdout: File,
}

impl HelperProcess {
    pub(crate) fn spawn(
        executable: &Path,
        argument: &OsStr,
        priority_class: u32,
    ) -> io::Result<Self> {
        let mut pipes = ParentChildPipes::create()?;
        let null_error = open_inheritable_null()?;
        let executable_wide = encode_null_terminated(executable.as_os_str());
        let mut command_line = build_command_line(executable, &[argument]);
        let inherited_handles = [
            pipes.child_stdin_read.raw(),
            pipes.child_stdout_write.raw(),
            null_error.raw(),
        ];
        let attribute_list = ProcessAttributeList::for_handle_list(&inherited_handles)?;
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = HELPER_STARTUP_FLAGS;
        startup.StartupInfo.hStdInput = pipes.child_stdin_read.raw();
        startup.StartupInfo.hStdOutput = pipes.child_stdout_write.raw();
        startup.StartupInfo.hStdError = null_error.raw();
        startup.lpAttributeList = attribute_list.raw();
        let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let created = unsafe {
            CreateProcessW(
                executable_wide.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                TRUE,
                CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT | priority_class,
                null(),
                null(),
                &startup.StartupInfo,
                &mut process_info,
            )
        };
        if created == 0 {
            return Err(io::Error::last_os_error());
        }

        let process = OwnedHandle(process_info.hProcess);
        let thread = OwnedHandle(process_info.hThread);
        drop(thread);
        drop(pipes.child_stdin_read);
        drop(pipes.child_stdout_write);
        drop(null_error);

        let stdin = unsafe { File::from_raw_handle(pipes.parent_stdin_write.take() as RawHandle) };
        let stdout = unsafe { File::from_raw_handle(pipes.parent_stdout_read.take() as RawHandle) };
        Ok(Self {
            process,
            stdin: Some(stdin),
            stdout,
        })
    }

    pub(crate) fn stdin_mut(&mut self) -> io::Result<&mut File> {
        self.stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "Helper stdin is closed."))
    }

    pub(crate) fn stdout_mut(&mut self) -> &mut File {
        &mut self.stdout
    }

    pub(crate) fn close_stdin(&mut self) {
        self.stdin.take();
    }

    pub(crate) fn has_exited(&self) -> io::Result<bool> {
        let mut exit_code = 0u32;
        if unsafe { GetExitCodeProcess(self.process.raw(), &mut exit_code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(exit_code != windows_sys::Win32::Foundation::STILL_ACTIVE as u32)
    }

    pub(crate) fn wait_timeout(&self, timeout: Duration) -> io::Result<bool> {
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        match unsafe { WaitForSingleObject(self.process.raw(), timeout_ms) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub(crate) fn terminate_and_wait(&self) -> io::Result<()> {
        if self.has_exited()? {
            return Ok(());
        }
        if unsafe { TerminateProcess(self.process.raw(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { WaitForSingleObject(self.process.raw(), INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

struct ParentChildPipes {
    child_stdin_read: OwnedHandle,
    parent_stdin_write: OwnedHandle,
    parent_stdout_read: OwnedHandle,
    child_stdout_write: OwnedHandle,
}

struct ProcessAttributeList {
    storage: Vec<usize>,
}

impl ProcessAttributeList {
    fn for_handle_list(handles: &[HANDLE]) -> io::Result<Self> {
        let mut byte_count = 0usize;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut byte_count);
        }
        if byte_count == 0 {
            return Err(io::Error::last_os_error());
        }
        let word_count = byte_count.div_ceil(std::mem::size_of::<usize>());
        let list = Self {
            storage: vec![0usize; word_count],
        };
        if unsafe { InitializeProcThreadAttributeList(list.raw(), 1, 0, &mut byte_count) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe {
            UpdateProcThreadAttribute(
                list.raw(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                std::mem::size_of_val(handles),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(list)
    }

    fn raw(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }
}

impl Drop for ProcessAttributeList {
    fn drop(&mut self) {
        if !self.storage.is_empty() {
            unsafe { DeleteProcThreadAttributeList(self.raw()) };
        }
    }
}

impl ParentChildPipes {
    fn create() -> io::Result<Self> {
        let mut security = inheritable_security_attributes();
        let (child_stdin_read, parent_stdin_write) = create_pipe(&mut security)?;
        clear_inheritance(parent_stdin_write.raw())?;
        let (parent_stdout_read, child_stdout_write) = create_pipe(&mut security)?;
        clear_inheritance(parent_stdout_read.raw())?;
        Ok(Self {
            child_stdin_read,
            parent_stdin_write,
            parent_stdout_read,
            child_stdout_write,
        })
    }
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }

    fn raw(&self) -> HANDLE {
        self.0
    }

    fn take(&mut self) -> HANDLE {
        std::mem::replace(&mut self.0, null_mut())
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn create_pipe(security: &mut SECURITY_ATTRIBUTES) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = null_mut();
    let mut write = null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, security, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((OwnedHandle(read), OwnedHandle(write)))
}

fn clear_inheritance(handle: HANDLE) -> io::Result<()> {
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn open_inheritable_null() -> io::Result<OwnedHandle> {
    let mut security = inheritable_security_attributes();
    let null_path = encode_null_terminated(OsStr::new("NUL"));
    let handle = unsafe {
        CreateFileW(
            null_path.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &mut security,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    OwnedHandle::new(handle)
}

fn inheritable_security_attributes() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: TRUE,
    }
}

fn encode_null_terminated(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn build_command_line(executable: &Path, arguments: &[&OsStr]) -> Vec<u16> {
    let mut command_line = Vec::new();
    append_quoted_argument(&mut command_line, executable.as_os_str(), true);
    for argument in arguments {
        command_line.push(b' ' as u16);
        append_quoted_argument(&mut command_line, argument, false);
    }
    command_line.push(0);
    command_line
}

fn append_quoted_argument(output: &mut Vec<u16>, argument: &OsStr, always_quote: bool) {
    let units = argument.encode_wide().collect::<Vec<_>>();
    let needs_quotes = always_quote
        || units.is_empty()
        || units
            .iter()
            .any(|unit| *unit == b' ' as u16 || *unit == b'\t' as u16 || *unit == b'"' as u16);
    if !needs_quotes {
        output.extend_from_slice(&units);
        return;
    }

    output.push(b'"' as u16);
    let mut backslashes = 0usize;
    for unit in units {
        if unit == b'\\' as u16 {
            backslashes += 1;
        } else if unit == b'"' as u16 {
            output.extend(std::iter::repeat(b'\\' as u16).take(backslashes * 2 + 1));
            output.push(unit);
            backslashes = 0;
        } else {
            output.extend(std::iter::repeat(b'\\' as u16).take(backslashes));
            output.push(unit);
            backslashes = 0;
        }
    }
    output.extend(std::iter::repeat(b'\\' as u16).take(backslashes * 2));
    output.push(b'"' as u16);
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::Foundation::GetHandleInformation;

    use super::*;

    #[test]
    fn command_line_quotes_executable_path_with_spaces() {
        let command_line = build_command_line(
            Path::new(r"C:\Program Files\Flash Launch\Flash Launch.exe"),
            &[OsStr::new("--icon-helper")],
        );
        let decoded = OsString::from_wide(&command_line[..command_line.len() - 1]);
        assert_eq!(
            decoded,
            OsStr::new(r#""C:\Program Files\Flash Launch\Flash Launch.exe" --icon-helper"#)
        );
    }

    #[test]
    fn startup_flags_disable_feedback_and_use_standard_handles() {
        assert_eq!(
            HELPER_STARTUP_FLAGS & (STARTF_FORCEOFFFEEDBACK | STARTF_USESTDHANDLES),
            STARTF_FORCEOFFFEEDBACK | STARTF_USESTDHANDLES
        );
    }

    #[test]
    fn parent_pipe_handles_are_not_inheritable() {
        let pipes = ParentChildPipes::create().unwrap();
        for handle in [
            pipes.parent_stdin_write.raw(),
            pipes.parent_stdout_read.raw(),
        ] {
            let mut flags = 0u32;
            assert_ne!(unsafe { GetHandleInformation(handle, &mut flags) }, 0);
            assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
        }
        for handle in [pipes.child_stdin_read.raw(), pipes.child_stdout_write.raw()] {
            let mut flags = 0u32;
            assert_ne!(unsafe { GetHandleInformation(handle, &mut flags) }, 0);
            assert_ne!(flags & HANDLE_FLAG_INHERIT, 0);
        }
    }
}

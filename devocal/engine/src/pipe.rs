//! Named-pipe server for the app link: `\\.\pipe\tune-love-devocal-<app pid>`, one JSON object
//! per line (see `devocal_core::protocol`).
//!
//! Security:
//! - `FILE_FLAG_FIRST_PIPE_INSTANCE`: creating the pipe fails if the name already exists, so no
//!   other process can have squatted it first; a single instance (`nMaxInstances = 1`).
//! - `PIPE_REJECT_REMOTE_CLIENTS`: local clients only.
//! - DACL `D:P(A;;GA;;;<current user SID>)`: protected, generic-all for the current user only
//!   (SID from `GetTokenInformation(TokenUser)` + `ConvertSidToStringSidW`).
//! - [`PipeServer::accept`] checks `GetNamedPipeClientProcessId` against the app's pid; any
//!   other client is disconnected and the server keeps waiting.
//!
//! The pipe is opened for overlapped I/O: Windows serialises synchronous I/O on one file
//! object, so a read pending on the reader thread would otherwise block every write. Each call
//! still waits for its own completion, so to callers `read_line` / `write_line` are blocking.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::{HRESULT, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, HANDLE, HLOCAL, WAIT_OBJECT_0, WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcessToken, WaitForSingleObject,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// In and out buffer size of the pipe.
pub const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
/// A longer line without a newline is an error (the caller drops the connection).
pub const MAX_LINE_BYTES: usize = 1 << 20;
/// How often `accept` checks its timeout and `keep_waiting`.
const ACCEPT_POLL_MS: u32 = 50;
const READ_CHUNK: usize = 4096;

/// `\\.\pipe\tune-love-devocal-<app pid>`.
pub fn pipe_name(app_pid: u32) -> String {
    format!(r"\\.\pipe\tune-love-devocal-{app_pid}")
}

fn is(e: &windows::core::Error, code: WIN32_ERROR) -> bool {
    e.code() == HRESULT::from_win32(code.0)
}

/// A handle closed on drop.
struct Owned(HANDLE);

// SAFETY: pipe and event handles may be used from any thread; all I/O on the pipe is
// overlapped with a per-call OVERLAPPED.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

fn manual_event() -> windows::core::Result<Owned> {
    unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map(Owned)
}

/// DACL that grants generic-all to the current user only: `D:P(A;;GA;;;<SID>)`.
pub fn owner_only_sddl() -> Result<String, String> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| format!("OpenProcessToken: {e}"))?;
    let token = Owned(token);
    let mut len = 0u32;
    // Sizing call: fails with ERROR_INSUFFICIENT_BUFFER and sets `len`.
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut len) };
    if len == 0 {
        return Err("GetTokenInformation(TokenUser) returned no size".into());
    }
    // u64 storage keeps TOKEN_USER (pointer-aligned) correctly aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )
    }
    .map_err(|e| format!("GetTokenInformation(TokenUser): {e}"))?;
    // SAFETY: the buffer holds a TOKEN_USER written by the call above and outlives `user`.
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    let mut sid = PWSTR::null();
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) }
        .map_err(|e| format!("ConvertSidToStringSidW: {e}"))?;
    let text = unsafe { sid.to_string() };
    let _ = unsafe { LocalFree(Some(HLOCAL(sid.0.cast()))) };
    let text = text.map_err(|e| format!("SID string: {e}"))?;
    Ok(format!("D:P(A;;GA;;;{text})"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    /// The expected client is connected.
    Connected,
    /// No accepted client before the timeout.
    TimedOut,
    /// `keep_waiting` returned false.
    Cancelled,
}

pub struct PipeServer {
    pipe: Owned,
    name: String,
    /// Bytes read but not yet returned as a line (only the reader uses it).
    read_buf: Mutex<Vec<u8>>,
    /// Serialises writers so lines never interleave.
    write_lock: Mutex<()>,
}

impl PipeServer {
    /// Creates the single pipe instance (see the module docs for the security settings).
    /// Fails if a pipe with this name already exists.
    pub fn create(name: &str) -> Result<Self, String> {
        let sddl = owner_only_sddl()?;
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl.as_str()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(|e| format!("security descriptor {sddl}: {e}"))?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let handle = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(name),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_BUFFER_BYTES,
                PIPE_BUFFER_BYTES,
                0,
                Some(&sa),
            )
        };
        let err = windows::core::Error::from_thread();
        let _ = unsafe { LocalFree(Some(HLOCAL(sd.0))) };
        if handle.is_invalid() {
            return Err(format!("CreateNamedPipeW({name}): {err}"));
        }
        Ok(Self {
            pipe: Owned(handle),
            name: name.to_string(),
            read_buf: Mutex::new(Vec::new()),
            write_lock: Mutex::new(()),
        })
    }

    /// Waits for a client whose process id is `expected_client_pid`. Any other client is
    /// disconnected and the wait continues. Returns `TimedOut` after `timeout` and
    /// `Cancelled` as soon as `keep_waiting` returns false (both checked every 50 ms).
    pub fn accept(
        &self,
        expected_client_pid: u32,
        timeout: Duration,
        keep_waiting: &mut dyn FnMut() -> bool,
    ) -> Result<Accepted, String> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.wait_connect(deadline, keep_waiting)? {
                Accepted::Connected => {}
                other => return Ok(other),
            }
            let mut pid = 0u32;
            let known = unsafe { GetNamedPipeClientProcessId(self.pipe.0, &mut pid) }.is_ok();
            if known && pid == expected_client_pid {
                return Ok(Accepted::Connected);
            }
            eprintln!(
                "devocal pipe {}: rejected client pid {} (expected {expected_client_pid})",
                self.name,
                if known {
                    pid.to_string()
                } else {
                    "unknown".into()
                }
            );
            unsafe { DisconnectNamedPipe(self.pipe.0) }
                .map_err(|e| format!("DisconnectNamedPipe: {e}"))?;
        }
    }

    /// One overlapped `ConnectNamedPipe`, waited for until a client connects, the deadline
    /// passes or `keep_waiting` says stop.
    fn wait_connect(
        &self,
        deadline: Instant,
        keep_waiting: &mut dyn FnMut() -> bool,
    ) -> Result<Accepted, String> {
        loop {
            let event = manual_event().map_err(|e| format!("CreateEventW: {e}"))?;
            let mut ov = OVERLAPPED {
                hEvent: event.0,
                ..Default::default()
            };
            match unsafe { ConnectNamedPipe(self.pipe.0, Some(&mut ov)) } {
                Ok(()) => return Ok(Accepted::Connected),
                Err(e) if is(&e, ERROR_PIPE_CONNECTED) => return Ok(Accepted::Connected),
                Err(e) if is(&e, ERROR_NO_DATA) => {
                    // A client connected and already closed its end: reset and wait again.
                    let _ = unsafe { DisconnectNamedPipe(self.pipe.0) };
                    continue;
                }
                Err(e) if is(&e, ERROR_IO_PENDING) => {}
                Err(e) => return Err(format!("ConnectNamedPipe: {e}")),
            }
            loop {
                if unsafe { WaitForSingleObject(event.0, ACCEPT_POLL_MS) } == WAIT_OBJECT_0 {
                    let mut n = 0u32;
                    match unsafe { GetOverlappedResult(self.pipe.0, &ov, &mut n, false) } {
                        Ok(()) => return Ok(Accepted::Connected),
                        Err(e) if is(&e, ERROR_NO_DATA) => {
                            let _ = unsafe { DisconnectNamedPipe(self.pipe.0) };
                            break;
                        }
                        Err(e) => return Err(format!("ConnectNamedPipe: {e}")),
                    }
                }
                let stop = if !keep_waiting() {
                    Some(Accepted::Cancelled)
                } else if Instant::now() >= deadline {
                    Some(Accepted::TimedOut)
                } else {
                    None
                };
                if let Some(stop) = stop {
                    // Cancel and wait for the cancellation so `ov` is no longer in use. A
                    // client that connected in the meantime still goes through the pid check.
                    let _ = unsafe { CancelIoEx(self.pipe.0, Some(&ov)) };
                    let mut n = 0u32;
                    return Ok(
                        match unsafe { GetOverlappedResult(self.pipe.0, &ov, &mut n, true) } {
                            Ok(()) => Accepted::Connected,
                            Err(_) => stop,
                        },
                    );
                }
            }
        }
    }

    /// Runs one overlapped operation to completion and returns the bytes transferred.
    fn complete(
        &self,
        start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> windows::core::Result<u32> {
        let event = manual_event()?;
        let mut ov = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        if let Err(e) = start(&mut ov) {
            if !is(&e, ERROR_IO_PENDING) {
                return Err(e);
            }
        }
        let mut n = 0u32;
        unsafe { GetOverlappedResult(self.pipe.0, &ov, &mut n, true) }?;
        Ok(n)
    }

    /// The next line without its `\n` (and a trailing `\r`); `Ok(None)` at end of stream
    /// (the client closed its end or disconnected). A last line without a newline is
    /// returned before `None`. Invalid UTF-8 is replaced (the decoder then rejects the line).
    pub fn read_line(&self) -> Result<Option<String>, String> {
        let mut buf = self
            .read_buf
            .lock()
            .map_err(|_| "pipe read buffer poisoned".to_string())?;
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = buf.drain(..=pos).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            if buf.len() > MAX_LINE_BYTES {
                return Err(format!("line longer than {MAX_LINE_BYTES} bytes"));
            }
            let read = self
                .complete(|ov| unsafe { ReadFile(self.pipe.0, Some(&mut chunk), None, Some(ov)) });
            match read {
                Ok(n) => buf.extend_from_slice(&chunk[..n as usize]),
                Err(e)
                    if is(&e, ERROR_BROKEN_PIPE)
                        || is(&e, ERROR_PIPE_NOT_CONNECTED)
                        || is(&e, ERROR_HANDLE_EOF) =>
                {
                    if buf.is_empty() {
                        return Ok(None);
                    }
                    let line = std::mem::take(&mut *buf);
                    return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
                }
                Err(e) => return Err(format!("ReadFile: {e}")),
            }
        }
    }

    /// Writes `line` completely (the caller includes the `\n`; `protocol::encode` does).
    pub fn write_line(&self, line: &str) -> Result<(), String> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| "pipe write lock poisoned".to_string())?;
        let mut rest = line.as_bytes();
        while !rest.is_empty() {
            let n = self
                .complete(|ov| unsafe { WriteFile(self.pipe.0, Some(rest), None, Some(ov)) })
                .map_err(|e| format!("WriteFile: {e}"))?;
            if n == 0 {
                return Err("WriteFile wrote nothing".into());
            }
            rest = &rest[n as usize..];
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
    };
    use windows::Win32::System::Pipes::PeekNamedPipe;

    fn test_name(tag: &str) -> String {
        static N: AtomicUsize = AtomicUsize::new(0);
        format!(
            r"\\.\pipe\tune-love-devocal-test-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }

    struct Client(HANDLE);

    impl Client {
        fn connect(name: &str) -> Self {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let h = unsafe {
                    CreateFileW(
                        &HSTRING::from(name),
                        GENERIC_READ.0 | GENERIC_WRITE.0,
                        FILE_SHARE_NONE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAGS_AND_ATTRIBUTES(0),
                        None,
                    )
                };
                match h {
                    Ok(h) => return Client(h),
                    Err(e) if Instant::now() < deadline => {
                        let _ = e;
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("client connect failed: {e}"),
                }
            }
        }

        /// True once the server side has disconnected this client (polled for up to 2 s).
        fn disconnected_within_2s(&self) -> bool {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut avail = 0u32;
                if unsafe { PeekNamedPipe(self.0, None, 0, None, Some(&mut avail), None) }.is_err()
                {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        }

        fn write(&self, s: &str) {
            let mut n = 0u32;
            unsafe { WriteFile(self.0, Some(s.as_bytes()), Some(&mut n), None) }.unwrap();
            assert_eq!(n as usize, s.len());
        }

        fn read_some(&self) -> String {
            let mut buf = [0u8; 256];
            let mut n = 0u32;
            unsafe { ReadFile(self.0, Some(&mut buf), Some(&mut n), None) }.unwrap();
            String::from_utf8_lossy(&buf[..n as usize]).into_owned()
        }
    }

    impl Drop for Client {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    #[test]
    fn pipe_name_format() {
        assert_eq!(pipe_name(1234), r"\\.\pipe\tune-love-devocal-1234");
        assert_eq!(pipe_name(7), r"\\.\pipe\tune-love-devocal-7");
    }

    #[test]
    fn pipe_sddl_grants_only_the_current_user() {
        let sddl = owner_only_sddl().unwrap();
        assert!(sddl.starts_with("D:P(A;;GA;;;S-1-"), "{sddl}");
        assert!(sddl.ends_with(')'), "{sddl}");
        assert_eq!(sddl.matches("(A;").count(), 1, "{sddl}");
    }

    #[test]
    fn second_instance_is_refused() {
        let name = test_name("first");
        let _first = PipeServer::create(&name).unwrap();
        assert!(PipeServer::create(&name).is_err());
    }

    #[test]
    fn pipe_rejects_wrong_client_pid() {
        let name = test_name("wrong-pid");
        let server = Arc::new(PipeServer::create(&name).unwrap());
        let s = server.clone();
        let expected = std::process::id() + 1;
        let accept = std::thread::spawn(move || {
            s.accept(expected, Duration::from_millis(1500), &mut || true)
        });
        let client = Client::connect(&name);
        assert!(
            client.disconnected_within_2s(),
            "a client from another pid must be disconnected"
        );
        assert_eq!(accept.join().unwrap(), Ok(Accepted::TimedOut));
    }

    #[test]
    fn accept_can_be_cancelled() {
        let name = test_name("cancel");
        let server = PipeServer::create(&name).unwrap();
        let start = Instant::now();
        let r = server.accept(std::process::id(), Duration::from_secs(10), &mut || {
            start.elapsed() < Duration::from_millis(100)
        });
        assert_eq!(r, Ok(Accepted::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn matching_client_round_trips_lines_until_eof() {
        let name = test_name("round-trip");
        let server = Arc::new(PipeServer::create(&name).unwrap());
        let s = server.clone();
        let pid = std::process::id();
        let accept =
            std::thread::spawn(move || s.accept(pid, Duration::from_secs(5), &mut || true));
        let client = Client::connect(&name);
        assert_eq!(accept.join().unwrap(), Ok(Accepted::Connected));

        server.write_line("{\"event\":\"x\"}\n").unwrap();
        assert_eq!(client.read_some(), "{\"event\":\"x\"}\n");

        client.write("first\nsec");
        client.write("ond\r\n\n");
        assert_eq!(server.read_line().unwrap().as_deref(), Some("first"));
        assert_eq!(server.read_line().unwrap().as_deref(), Some("second"));
        assert_eq!(server.read_line().unwrap().as_deref(), Some(""));
        drop(client);
        assert_eq!(server.read_line().unwrap(), None);
    }
}

//! The app's end of the engine connection.
//!
//! [`EngineLink`] is what the [`Supervisor`](super::supervisor::Supervisor) talks to;
//! [`ProcessLink`] is the real one: it starts `devocal-engine.exe --app-pid <our pid>
//! --restore-file <path>` (no console window, stderr appended to a log file), connects to
//! `\\.\pipe\tune-love-devocal-<our pid>` and checks that the pipe's server process is the
//! child it just started (a squatter defence; the engine in turn accepts only our pid).
//!
//! The pipe is opened for overlapped I/O: Windows serialises synchronous I/O on one file
//! object, so the reader thread's pending read would otherwise block every write. A reader
//! thread decodes one event per line into a channel; an undecodable line becomes
//! `Error{Protocol}` so a version mismatch during the hello exchange is noticed.

use devocal_core::protocol::{Command, Event};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub trait EngineLink {
    /// Sends one command. An error means the command could not be encoded or the link is
    /// broken.
    fn send(&mut self, cmd: &Command) -> Result<(), String>;
    /// The next event, if one has arrived.
    fn try_recv(&mut self) -> Option<Event>;
    /// The engine process has ended.
    fn exited(&self) -> bool;
    /// Ends the engine process now.
    fn kill(&mut self);
}

/// `\\.\pipe\tune-love-devocal-<app pid>` (must match the engine's `pipe_name`).
pub fn pipe_name(app_pid: u32) -> String {
    format!(r"\\.\pipe\tune-love-devocal-{app_pid}")
}

/// The engine log is rotated when it is larger than this at engine start.
pub const LOG_ROTATE_BYTES: u64 = 1024 * 1024;

/// `<path>.1`.
pub fn rotated_log(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// Opens the engine log for appending. If it is larger than [`LOG_ROTATE_BYTES`] it becomes
/// `<path>.1` first (replacing an older `.1`), so at most one old copy is kept.
pub fn open_log(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let old = rotated_log(path);
        let _ = std::fs::remove_file(&old);
        // If the rename fails the log just keeps growing; the engine must still start.
        let _ = std::fs::rename(path, &old);
    }
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(windows)]
pub use win::{PipeClient, ProcessLink, CONNECT_TIMEOUT};

#[cfg(windows)]
mod win {
    use super::{open_log, pipe_name, EngineLink};
    use devocal_core::protocol::{decode_event, encode, Command, ErrorCode, Event};
    use std::io::Write;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::process::{Child, Stdio};
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use windows::core::{HRESULT, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_PIPE_BUSY, GENERIC_READ,
        GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, WIN32_ERROR,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING,
    };
    use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows::Win32::System::Threading::{CreateEventW, TerminateProcess, WaitForSingleObject};
    use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

    /// How long the engine has to create its pipe (the engine waits as long for us).
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    /// After the pipe closed, the engine gets this long to finish its release and exit before
    /// it is terminated.
    const LINGER_AFTER_CLOSE: Duration = Duration::from_secs(2);
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const READ_CHUNK: usize = 4096;
    const MAX_LINE_BYTES: usize = 1 << 20;

    fn is(e: &windows::core::Error, code: WIN32_ERROR) -> bool {
        e.code() == HRESULT::from_win32(code.0)
    }

    /// A handle closed on drop.
    struct Owned(HANDLE);

    // SAFETY: pipe and event handles may be used from any thread; all pipe I/O is overlapped
    // with a per-call OVERLAPPED.
    unsafe impl Send for Owned {}
    unsafe impl Sync for Owned {}

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// Runs one overlapped operation to completion and returns the bytes transferred.
    fn complete(
        pipe: HANDLE,
        start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> windows::core::Result<u32> {
        let event = Owned(unsafe { CreateEventW(None, true, false, PCWSTR::null()) }?);
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
        unsafe { GetOverlappedResult(pipe, &ov, &mut n, true) }?;
        Ok(n)
    }

    /// Client end of the engine pipe: overlapped writes, a reader thread for events.
    pub struct PipeClient {
        pipe: Arc<Owned>,
        rx: Receiver<Event>,
        /// When the reader saw the end of the stream (or a read error).
        closed_at: Arc<Mutex<Option<Instant>>>,
    }

    impl PipeClient {
        /// Connects to `name`, retrying while the pipe does not exist yet or is busy, until
        /// `timeout`. `still_starting` is asked between attempts and can abort (e.g. the
        /// engine already exited). The pipe's server must be `expected_server_pid`.
        pub fn connect(
            name: &str,
            expected_server_pid: u32,
            timeout: Duration,
            still_starting: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<Self, String> {
            let deadline = Instant::now() + timeout;
            let wide = HSTRING::from(name);
            let pipe = loop {
                let opened = unsafe {
                    CreateFileW(
                        &wide,
                        GENERIC_READ.0 | GENERIC_WRITE.0,
                        FILE_SHARE_NONE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAG_OVERLAPPED,
                        None,
                    )
                };
                match opened {
                    Ok(h) => break Owned(h),
                    Err(e) if is(&e, ERROR_FILE_NOT_FOUND) || is(&e, ERROR_PIPE_BUSY) => {
                        still_starting()?;
                        if Instant::now() >= deadline {
                            return Err(format!(
                                "timed out after {} s connecting to {name}: {e}",
                                timeout.as_secs()
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(e) => return Err(format!("connecting to {name}: {e}")),
                }
            };
            let mut server_pid = 0u32;
            unsafe { GetNamedPipeServerProcessId(pipe.0, &mut server_pid) }
                .map_err(|e| format!("GetNamedPipeServerProcessId: {e}"))?;
            if server_pid != expected_server_pid {
                return Err(format!(
                    "{name} is served by pid {server_pid}, not the engine (pid \
                     {expected_server_pid}); refusing it"
                ));
            }
            let pipe = Arc::new(pipe);
            let (tx, rx) = mpsc::channel();
            let closed_at: Arc<Mutex<Option<Instant>>> = Arc::default();
            let (reader, closed) = (pipe.clone(), closed_at.clone());
            std::thread::Builder::new()
                .name("devocal-link-read".into())
                .spawn(move || {
                    read_events(&reader, &tx);
                    *closed.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                })
                .map_err(|e| format!("spawning the pipe reader: {e}"))?;
            Ok(Self {
                pipe,
                rx,
                closed_at,
            })
        }

        pub fn send_line(&self, line: &str) -> Result<(), String> {
            let mut rest = line.as_bytes();
            while !rest.is_empty() {
                let n = complete(self.pipe.0, |ov| unsafe {
                    WriteFile(self.pipe.0, Some(rest), None, Some(ov))
                })
                .map_err(|e| format!("pipe write: {e}"))?;
                if n == 0 {
                    return Err("pipe write wrote nothing".into());
                }
                rest = &rest[n as usize..];
            }
            Ok(())
        }

        pub fn try_recv(&self) -> Option<Event> {
            self.rx.try_recv().ok()
        }

        /// How long ago the reader saw the pipe close, if it has.
        pub fn closed_for(&self) -> Option<Duration> {
            self.closed_at
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .map(|t| t.elapsed())
        }
    }

    /// Reads lines until the pipe closes; every line becomes one event.
    fn read_events(pipe: &Owned, tx: &mpsc::Sender<Event>) {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line);
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let ev = decode_event(text).unwrap_or_else(|e| Event::Error {
                    code: ErrorCode::Protocol,
                    message: format!("undecodable engine message: {e}"),
                });
                if tx.send(ev).is_err() {
                    return;
                }
            }
            if buf.len() > MAX_LINE_BYTES {
                let _ = tx.send(Event::Error {
                    code: ErrorCode::Protocol,
                    message: format!("engine line longer than {MAX_LINE_BYTES} bytes"),
                });
                return;
            }
            match complete(pipe.0, |ov| unsafe {
                ReadFile(pipe.0, Some(&mut chunk), None, Some(ov))
            }) {
                Ok(n) => buf.extend_from_slice(&chunk[..n as usize]),
                Err(_) => return, // closed (broken pipe) or failed: the engine is gone
            }
        }
    }

    /// A running `devocal-engine.exe` and its pipe.
    pub struct ProcessLink {
        child: Child,
        client: PipeClient,
    }

    impl ProcessLink {
        /// Starts the engine and connects (see the module docs). The engine's stderr goes
        /// to `log` (appended, rotated at 1 MB).
        pub fn spawn(exe: &Path, restore_file: &Path, log: &Path) -> Result<Self, String> {
            if !exe.is_file() {
                return Err(format!("devocal-engine.exe not found at {}", exe.display()));
            }
            let app_pid = std::process::id();
            let stderr = match open_log(log) {
                Ok(mut f) => {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis());
                    let _ = writeln!(
                        f,
                        "--- devocal-engine start, unix ms {now}, app pid {app_pid} ---"
                    );
                    Stdio::from(f)
                }
                Err(e) => {
                    eprintln!("devocal: engine log {} unavailable: {e}", log.display());
                    Stdio::null()
                }
            };
            let mut child = std::process::Command::new(exe)
                .arg("--app-pid")
                .arg(app_pid.to_string())
                .arg("--restore-file")
                .arg(restore_file)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(stderr)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map_err(|e| format!("starting {}: {e}", exe.display()))?;
            let child_pid = child.id();
            let mut still_starting = || match child.try_wait() {
                Ok(Some(status)) => Err(format!("the engine exited during start-up ({status})")),
                Ok(None) => Ok(()),
                Err(e) => Err(format!("checking the engine process: {e}")),
            };
            match PipeClient::connect(
                &pipe_name(app_pid),
                child_pid,
                CONNECT_TIMEOUT,
                &mut still_starting,
            ) {
                Ok(client) => Ok(Self { child, client }),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    Err(e)
                }
            }
        }

        fn process(&self) -> HANDLE {
            HANDLE(self.child.as_raw_handle())
        }
    }

    impl EngineLink for ProcessLink {
        fn send(&mut self, cmd: &Command) -> Result<(), String> {
            let line = encode(cmd).map_err(|e| format!("encode: {e}"))?;
            self.client.send_line(&line)
        }

        fn try_recv(&mut self) -> Option<Event> {
            self.client.try_recv()
        }

        fn exited(&self) -> bool {
            if unsafe { WaitForSingleObject(self.process(), 0) } == WAIT_OBJECT_0 {
                return true;
            }
            // The pipe closed but the process lingers: a closed link is useless, and a hung
            // engine must not block the restore and restart forever.
            if self
                .client
                .closed_for()
                .is_some_and(|d| d >= LINGER_AFTER_CLOSE)
            {
                let _ = unsafe { TerminateProcess(self.process(), 1) };
                let _ = unsafe { WaitForSingleObject(self.process(), 1_000) };
                return true;
            }
            false
        }

        fn kill(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(not(windows))]
pub struct ProcessLink;

#[cfg(not(windows))]
impl ProcessLink {
    pub fn spawn(_exe: &Path, _restore_file: &Path, _log: &Path) -> Result<Self, String> {
        Err("the devocal engine is only available on Windows".into())
    }
}

#[cfg(not(windows))]
impl EngineLink for ProcessLink {
    fn send(&mut self, _cmd: &Command) -> Result<(), String> {
        Err("no engine".into())
    }
    fn try_recv(&mut self) -> Option<Event> {
        None
    }
    fn exited(&self) -> bool {
        true
    }
    fn kill(&mut self) {}
}

#[cfg(test)]
pub mod fake {
    //! In-memory engine link for tests: records commands, replays queued events, and can
    //! simulate the engine exiting or hanging.
    use super::EngineLink;
    use devocal_core::protocol::{Command, Event};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, MutexGuard};

    #[derive(Default)]
    pub struct FakeEngineInner {
        pub sent: Vec<Command>,
        pub events: VecDeque<Event>,
        pub exited: bool,
        pub killed: bool,
        /// Exit as soon as `shutdown` is received (a well-behaved engine).
        pub exit_on_shutdown: bool,
    }

    /// Test-side handle to one fake engine process.
    #[derive(Clone, Default)]
    pub struct FakeEngine(Arc<Mutex<FakeEngineInner>>);

    impl FakeEngine {
        pub fn lock(&self) -> MutexGuard<'_, FakeEngineInner> {
            self.0.lock().unwrap()
        }
        pub fn sent(&self) -> Vec<Command> {
            self.lock().sent.clone()
        }
        pub fn clear_sent(&self) {
            self.lock().sent.clear();
        }
        pub fn push(&self, ev: Event) {
            self.lock().events.push_back(ev);
        }
        pub fn exit(&self) {
            self.lock().exited = true;
        }
        pub fn killed(&self) -> bool {
            self.lock().killed
        }
        pub fn link(&self) -> FakeLink {
            FakeLink(self.clone())
        }
    }

    pub struct FakeLink(FakeEngine);

    impl EngineLink for FakeLink {
        fn send(&mut self, cmd: &Command) -> Result<(), String> {
            let mut i = self.0.lock();
            if i.exited {
                return Err("the engine has exited".into());
            }
            i.sent.push(cmd.clone());
            if i.exit_on_shutdown && *cmd == Command::Shutdown {
                i.exited = true;
            }
            Ok(())
        }
        fn try_recv(&mut self) -> Option<Event> {
            self.0.lock().events.pop_front()
        }
        fn exited(&self) -> bool {
            self.0.lock().exited
        }
        fn kill(&mut self) {
            let mut i = self.0.lock();
            i.killed = true;
            i.exited = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_matches_the_engine() {
        assert_eq!(pipe_name(1234), r"\\.\pipe\tune-love-devocal-1234");
    }

    #[test]
    fn log_is_appended_and_rotated_once_over_one_megabyte() {
        let dir = crate::devocal::tests::temp_dir("log");
        let log = dir.join("logs").join("devocal-engine.log");
        {
            let mut f = open_log(&log).unwrap();
            io::Write::write_all(&mut f, b"first\n").unwrap();
        }
        {
            let mut f = open_log(&log).unwrap();
            io::Write::write_all(&mut f, b"second\n").unwrap();
        }
        assert_eq!(std::fs::read(&log).unwrap(), b"first\nsecond\n");
        assert!(!rotated_log(&log).exists());

        std::fs::write(&log, vec![b'a'; LOG_ROTATE_BYTES as usize + 1]).unwrap();
        std::fs::write(rotated_log(&log), b"older").unwrap();
        {
            let mut f = open_log(&log).unwrap();
            io::Write::write_all(&mut f, b"fresh\n").unwrap();
        }
        assert_eq!(std::fs::read(&log).unwrap(), b"fresh\n");
        assert_eq!(
            std::fs::metadata(rotated_log(&log)).unwrap().len(),
            LOG_ROTATE_BYTES + 1,
            "the old .1 copy is replaced"
        );
    }

    #[test]
    fn spawn_reports_a_missing_engine_without_starting_anything() {
        let dir = crate::devocal::tests::temp_dir("no-engine");
        let err = ProcessLink::spawn(
            &dir.join("devocal-engine.exe"),
            &dir.join("devocal-restore.json"),
            &dir.join("logs").join("devocal-engine.log"),
        )
        .err()
        .expect("no engine binary");
        assert!(err.contains("not found"), "{err}");
    }

    #[cfg(windows)]
    mod pipe {
        use super::super::*;
        use devocal_core::protocol::{decode_command, encode, Event, Phase};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};
        use windows::core::HSTRING;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile, PIPE_ACCESS_DUPLEX};
        use windows::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
        };

        fn test_name() -> String {
            static N: AtomicUsize = AtomicUsize::new(0);
            format!(
                r"\\.\pipe\tune-love-devocal-linktest-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            )
        }

        /// A plain synchronous pipe server in this process (stands in for the engine).
        struct Server(HANDLE);
        unsafe impl Send for Server {}

        impl Server {
            fn create(name: &str) -> Self {
                let h = unsafe {
                    CreateNamedPipeW(
                        &HSTRING::from(name),
                        PIPE_ACCESS_DUPLEX,
                        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                        1,
                        4096,
                        4096,
                        0,
                        None,
                    )
                };
                assert!(!h.is_invalid());
                Server(h)
            }
            fn write(&self, s: &str) {
                let mut n = 0u32;
                unsafe { WriteFile(self.0, Some(s.as_bytes()), Some(&mut n), None) }.unwrap();
                assert_eq!(n as usize, s.len());
            }
            fn read_some(&self) -> String {
                let mut buf = [0u8; 512];
                let mut n = 0u32;
                unsafe { ReadFile(self.0, Some(&mut buf), Some(&mut n), None) }.unwrap();
                String::from_utf8_lossy(&buf[..n as usize]).into_owned()
            }
        }

        impl Drop for Server {
            fn drop(&mut self) {
                let _ = unsafe { CloseHandle(self.0) };
            }
        }

        fn ok() -> Result<(), String> {
            Ok(())
        }

        #[test]
        fn rejects_a_pipe_served_by_another_process() {
            let name = test_name();
            let _server = Server::create(&name);
            let err = PipeClient::connect(
                &name,
                std::process::id() + 1,
                Duration::from_secs(1),
                &mut ok,
            )
            .err()
            .expect("server pid differs");
            assert!(err.contains("refusing"), "{err}");
        }

        #[test]
        fn retries_until_the_pipe_exists_and_round_trips_lines() {
            let name = test_name();
            let n2 = name.clone();
            let server = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                Server::create(&n2)
            });
            let client =
                PipeClient::connect(&name, std::process::id(), Duration::from_secs(5), &mut ok)
                    .expect("connects once the pipe exists");
            let server = server.join().unwrap();

            let line = encode(&Command::Release).unwrap();
            client.send_line(&line).unwrap();
            assert_eq!(
                decode_command(&server.read_some()).unwrap(),
                Command::Release
            );

            let ev = Event::State {
                phase: Phase::Active,
                mode: None,
                fallback_reason: None,
                attached_pid: Some(5),
            };
            server.write(&encode(&ev).unwrap());
            server.write("{\"protocol\":2,\"event\":\"state\"}\n");
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut got = Vec::new();
            while got.len() < 2 && Instant::now() < deadline {
                if let Some(e) = client.try_recv() {
                    got.push(e);
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            assert_eq!(got[0], ev);
            assert!(matches!(
                got[1],
                Event::Error {
                    code: devocal_core::protocol::ErrorCode::Protocol,
                    ..
                }
            ));
            assert!(client.closed_for().is_none());
            drop(server);
            let deadline = Instant::now() + Duration::from_secs(2);
            while client.closed_for().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(client.closed_for().is_some(), "reader notices the close");
        }

        #[test]
        fn gives_up_when_the_engine_exits_during_start_up() {
            let name = test_name();
            let mut gone = || Err("the engine exited during start-up".to_string());
            let err =
                PipeClient::connect(&name, std::process::id(), Duration::from_secs(5), &mut gone)
                    .err()
                    .unwrap();
            assert!(err.contains("exited"), "{err}");
        }
    }
}

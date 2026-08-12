// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
    ffi::OsStr,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    sync::{Arc, Mutex, RwLock},
    thread::spawn,
};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

const NEWLINE_BYTE: u8 = b'\n';

use tauri::async_runtime::{block_on as block_on_task, channel, Receiver, Sender};

pub use encoding_rs::Encoding;
use os_pipe::{pipe, PipeReader, PipeWriter};
use serde::Serialize;
use tauri::utils::platform;

/// Payload for the [`CommandEvent::Terminated`] command event.
#[derive(Debug, Clone, Serialize)]
pub struct TerminatedPayload {
    /// Exit code of the process.
    pub code: Option<i32>,
    /// If the process was terminated by a signal, represents that signal.
    pub signal: Option<i32>,
}

/// A event sent to the command callback.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CommandEvent {
    /// If configured for raw output, all bytes written to stderr.
    /// Otherwise, bytes until a newline (\n) or carriage return (\r) is found.
    Stderr(Vec<u8>),
    /// If configured for raw output, all bytes written to stdout.
    /// Otherwise, bytes until a newline (\n) or carriage return (\r) is found.
    Stdout(Vec<u8>),
    /// An error happened waiting for the command to finish or converting the stdout/stderr bytes to a UTF-8 string.
    Error(String),
    /// Command process terminated.
    Terminated(TerminatedPayload),
}

/// The type to spawn commands.
#[derive(Debug)]
pub struct Command {
    cmd: StdCommand,
    raw_out: bool,
    process_group: bool,
}

/// Spawned child process.
pub struct CommandChild {
    inner: Arc<Mutex<Box<dyn process_wrap::std::StdChildWrapper>>>,
    pid: u32,
    stdin_writer: PipeWriter,
}

impl CommandChild {
    /// Writes to process stdin.
    pub fn write(&mut self, buf: &[u8]) -> crate::Result<()> {
        self.stdin_writer.write_all(buf)?;
        Ok(())
    }

    /// Sends a kill signal to the child, then waits for it to exit.
    /// When the child was spawned with `process_group` enabled, this kills the
    /// entire process group (POSIX) or job object (Windows).
    pub fn kill(self) -> crate::Result<()> {
        self.inner.lock().unwrap().start_kill()?;
        // `StdChildWrapper::wait` can block with the lock held (the Windows
        // job-object wait parks in GetQueuedCompletionStatus), which would
        // hang every other user of the child. Instead block on the raw
        // process outside the lock — `self` keeps the underlying handle
        // alive, so the pid cannot be recycled — then let the wrapper reap
        // under the lock. The wait thread may win that race; both sides
        // tolerate the other having reaped first.
        ExitWaiter::new(self.pid)?.wait()?;
        loop {
            if self.inner.lock().unwrap().try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Returns the process pid.
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

/// Describes the result of a process after it has terminated.
#[derive(Debug)]
pub struct ExitStatus {
    // This field is intentionally left private.
    // See: https://github.com/tauri-apps/plugins-workspace/pull/3115.
    code: Option<i32>,
}

impl ExitStatus {
    /// Returns the exit code of the process, if any.
    pub fn code(&self) -> Option<i32> {
        self.code
    }

    /// Returns true if exit status is zero. Signal termination is not considered a success, and success is defined as a zero exit status.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// The output of a finished process.
#[derive(Debug)]
pub struct Output {
    /// The status (exit code) of the process.
    pub status: ExitStatus,
    /// The data that the process wrote to stdout.
    pub stdout: Vec<u8>,
    /// The data that the process wrote to stderr.
    pub stderr: Vec<u8>,
}

fn relative_command_path(command: &Path) -> crate::Result<PathBuf> {
    let exe_path = platform::current_exe()?;

    let exe_dir = exe_path
        .parent()
        .ok_or(crate::Error::CurrentExeHasNoParent)?;

    // If a test is being run, the executable is in the "deps" directory, so we need to go up one level.
    let base_dir = if exe_dir.ends_with("deps") {
        exe_dir.parent().unwrap_or(exe_dir)
    } else {
        exe_dir
    };

    let mut command_path = base_dir.join(command);

    #[cfg(windows)]
    {
        let already_exe = command_path.extension().is_some_and(|ext| ext == "exe");
        if !already_exe {
            // do not use with_extension to retain dots in the command filename
            command_path.as_mut_os_string().push(".exe");
        }
    }

    #[cfg(not(windows))]
    {
        if command_path.extension().is_some_and(|ext| ext == "exe") {
            command_path.set_extension("");
        }
    }

    Ok(command_path)
}

impl From<Command> for StdCommand {
    fn from(cmd: Command) -> StdCommand {
        cmd.cmd
    }
}

impl Command {
    pub(crate) fn new<S: AsRef<OsStr>>(program: S) -> Self {
        log::debug!(
            "Creating sidecar {}",
            program.as_ref().to_str().unwrap_or("")
        );
        let mut command = StdCommand::new(program);

        command.stdout(Stdio::piped());
        command.stdin(Stdio::piped());
        command.stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW.0);

        Self {
            cmd: command,
            raw_out: false,
            process_group: false,
        }
    }

    pub(crate) fn new_sidecar<S: AsRef<Path>>(program: S) -> crate::Result<Self> {
        Ok(Self::new(relative_command_path(program.as_ref())?))
    }

    /// Appends an argument to the command.
    #[must_use]
    pub fn arg<S: AsRef<OsStr>>(mut self, arg: S) -> Self {
        self.cmd.arg(arg);
        self
    }

    /// Appends arguments to the command.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.cmd.args(args);
        self
    }

    /// Clears the entire environment map for the child process.
    #[must_use]
    pub fn env_clear(mut self) -> Self {
        self.cmd.env_clear();
        self
    }

    /// Inserts or updates an explicit environment variable mapping.
    #[must_use]
    pub fn env<K, V>(mut self, key: K, value: V) -> Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.cmd.env(key, value);
        self
    }

    /// Adds or updates multiple environment variable mappings.
    #[must_use]
    pub fn envs<I, K, V>(mut self, envs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.cmd.envs(envs);
        self
    }

    /// Sets the working directory for the child process.
    #[must_use]
    pub fn current_dir<P: AsRef<Path>>(mut self, current_dir: P) -> Self {
        self.cmd.current_dir(current_dir);
        self
    }

    /// Configures the reader to output bytes from the child process exactly as received
    pub fn set_raw_out(mut self, raw_out: bool) -> Self {
        self.raw_out = raw_out;
        self
    }

    /// Configures the command to spawn in a new process group (POSIX) or job object (Windows).
    ///
    /// When enabled, killing the child process will also kill all processes in the group,
    /// which is useful for programs that spawn child processes (e.g. PyInstaller wrappers).
    #[must_use]
    pub fn set_process_group(mut self, process_group: bool) -> Self {
        self.process_group = process_group;
        self
    }

    /// Spawns the command.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use tauri_plugin_shell::{process::CommandEvent, ShellExt};
    /// tauri::Builder::default()
    ///   .setup(|app| {
    ///     let handle = app.handle().clone();
    ///     tauri::async_runtime::spawn(async move {
    ///       let (mut rx, mut child) = handle
    ///         .shell()
    ///         .command("cargo")
    ///         .args(["tauri", "dev"])
    ///         .spawn()
    ///         .expect("Failed to spawn cargo");
    ///
    ///       let mut i = 0;
    ///       while let Some(event) = rx.recv().await {
    ///         if let CommandEvent::Stdout(line) = event {
    ///           println!("got: {}", String::from_utf8(line).unwrap());
    ///           i += 1;
    ///           if i == 4 {
    ///             child.write("message from Rust\n".as_bytes()).unwrap();
    ///             i = 0;
    ///           }
    ///         }
    ///       }
    ///     });
    ///     Ok(())
    ///   });
    /// ```
    ///
    /// Depending on the command you spawn, it might output in a specific encoding, to parse the output lines in this case:
    ///
    /// ```rust,no_run
    /// use tauri_plugin_shell::{process::{CommandEvent, Encoding}, ShellExt};
    /// tauri::Builder::default()
    ///   .setup(|app| {
    ///     let handle = app.handle().clone();
    ///     tauri::async_runtime::spawn(async move {
    ///       let (mut rx, mut child) = handle
    ///         .shell()
    ///         .command("some-program")
    ///         .arg("some-arg")
    ///         .spawn()
    ///         .expect("Failed to spawn some-program");
    ///
    ///       let encoding = Encoding::for_label(b"windows-1252").unwrap();
    ///       while let Some(event) = rx.recv().await {
    ///         if let CommandEvent::Stdout(line) = event {
    ///           let (decoded, _, _) = encoding.decode(&line);
    ///           println!("got: {decoded}");
    ///         }
    ///       }
    ///     });
    ///     Ok(())
    ///   });
    /// ```
    pub fn spawn(self) -> crate::Result<(Receiver<CommandEvent>, CommandChild)> {
        let raw = self.raw_out;
        let process_group = self.process_group;
        let mut command: StdCommand = self.into();
        let (stdout_reader, stdout_writer) = pipe()?;
        let (stderr_reader, stderr_writer) = pipe()?;
        let (stdin_reader, stdin_writer) = pipe()?;
        command.stdout(stdout_writer);
        command.stderr(stderr_writer);
        command.stdin(stdin_reader);

        let guard = Arc::new(RwLock::new(()));
        let (tx, rx) = channel(1);

        spawn_pipe_reader(
            tx.clone(),
            guard.clone(),
            stdout_reader,
            CommandEvent::Stdout,
            raw,
        );
        spawn_pipe_reader(
            tx.clone(),
            guard.clone(),
            stderr_reader,
            CommandEvent::Stderr,
            raw,
        );

        // Always go through process-wrap so the spawn path is uniform across
        // platforms; the `process_group` switch is just an optional wrapper
        // rather than a separate child type.
        let mut cmd_wrap = process_wrap::std::StdCommandWrap::from(command);

        if process_group {
            #[cfg(unix)]
            cmd_wrap.wrap(process_wrap::std::ProcessGroup::leader());

            #[cfg(windows)]
            {
                cmd_wrap.wrap(process_wrap::std::CreationFlags(CREATE_NO_WINDOW));
                cmd_wrap.wrap(process_wrap::std::JobObject);
            }
        }

        let wrapped_child = cmd_wrap.spawn()?;
        let pid = wrapped_child.id();
        // Constructed while the child is definitely alive; see `ExitWaiter`.
        let waiter = ExitWaiter::new(pid)?;
        let inner = Arc::new(Mutex::new(wrapped_child));
        let inner_wait = inner.clone();

        spawn_wait_thread(move || wait_on_child(&inner_wait, waiter), tx, guard);

        Ok((
            rx,
            CommandChild {
                inner,
                pid,
                stdin_writer,
            },
        ))
    }

    /// Executes a command as a child process, waiting for it to finish and collecting its exit status.
    /// Stdin, stdout and stderr are ignored.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tauri_plugin_shell::ShellExt;
    /// tauri::Builder::default()
    ///   .setup(|app| {
    ///     let status = tauri::async_runtime::block_on(async move { app.shell().command("which").args(["ls"]).status().await.unwrap() });
    ///     println!("`which` finished with status: {:?}", status.code());
    ///     Ok(())
    ///   });
    /// ```
    pub async fn status(self) -> crate::Result<ExitStatus> {
        let (mut rx, _child) = self.spawn()?;
        let mut code = None;
        #[allow(clippy::collapsible_match)]
        while let Some(event) = rx.recv().await {
            if let CommandEvent::Terminated(payload) = event {
                code = payload.code;
            }
        }
        Ok(ExitStatus { code })
    }

    /// Executes the command as a child process, waiting for it to finish and collecting all of its output.
    /// Stdin is ignored.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use tauri_plugin_shell::ShellExt;
    /// tauri::Builder::default()
    ///   .setup(|app| {
    ///     let output = tauri::async_runtime::block_on(async move { app.shell().command("echo").args(["TAURI"]).output().await.unwrap() });
    ///     assert!(output.status.success());
    ///     assert_eq!(String::from_utf8(output.stdout).unwrap(), "TAURI");
    ///     Ok(())
    ///   });
    /// ```
    pub async fn output(self) -> crate::Result<Output> {
        let (mut rx, _child) = self.spawn()?;

        let mut code = None;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Terminated(payload) => {
                    code = payload.code;
                }
                CommandEvent::Stdout(line) => {
                    stdout.extend(line);
                    stdout.push(NEWLINE_BYTE);
                }
                CommandEvent::Stderr(line) => {
                    stderr.extend(line);
                    stderr.push(NEWLINE_BYTE);
                }
                CommandEvent::Error(_) => {}
            }
        }
        Ok(Output {
            status: ExitStatus { code },
            stdout,
            stderr,
        })
    }
}

fn read_raw_bytes<F: Fn(Vec<u8>) -> CommandEvent + Send + Copy + 'static>(
    mut reader: BufReader<PipeReader>,
    tx: Sender<CommandEvent>,
    wrapper: F,
) {
    loop {
        let result = reader.fill_buf();
        match result {
            Ok(buf) => {
                let length = buf.len();
                if length == 0 {
                    break;
                }
                let tx_ = tx.clone();
                let _ = block_on_task(async move { tx_.send(wrapper(buf.to_vec())).await });
                reader.consume(length);
            }
            Err(e) => {
                let tx_ = tx.clone();
                let _ = block_on_task(
                    async move { tx_.send(CommandEvent::Error(e.to_string())).await },
                );
            }
        }
    }
}

fn read_line<F: Fn(Vec<u8>) -> CommandEvent + Send + Copy + 'static>(
    mut reader: BufReader<PipeReader>,
    tx: Sender<CommandEvent>,
    wrapper: F,
) {
    loop {
        let mut buf = Vec::new();
        match tauri::utils::io::read_line(&mut reader, &mut buf) {
            Ok(n) => {
                if n == 0 {
                    break;
                }
                let tx_ = tx.clone();
                let _ = block_on_task(async move { tx_.send(wrapper(buf)).await });
            }
            Err(e) => {
                let _ =
                    block_on_task(async move { tx.send(CommandEvent::Error(e.to_string())).await });
                break;
            }
        }
    }
}

fn spawn_pipe_reader<F: Fn(Vec<u8>) -> CommandEvent + Send + Copy + 'static>(
    tx: Sender<CommandEvent>,
    guard: Arc<RwLock<()>>,
    pipe_reader: PipeReader,
    wrapper: F,
    raw_out: bool,
) {
    spawn(move || {
        let _lock = guard.read().unwrap();
        let reader = BufReader::new(pipe_reader);

        if raw_out {
            read_raw_bytes(reader, tx, wrapper);
        } else {
            read_line(reader, tx, wrapper);
        }
    });
}

/// Waits for the child to exit, returning its final exit status.
///
/// process-wrap's child wrappers only expose `&mut self` wait methods, so a
/// blocking wait taken through the lock would hold it for the child's whole
/// lifetime and starve `kill()`. Instead we block on the raw process *outside*
/// the lock, leaving it unreaped, and only then take the lock to collect the
/// status.
///
/// By the time the blocking wait returns, the child is either waitable or was
/// already reaped by a concurrent `kill()` (which caches the exit status), so
/// the first `try_wait` succeeds immediately; the loop is a defensive fallback.
fn wait_on_child(
    inner: &Arc<Mutex<Box<dyn process_wrap::std::StdChildWrapper>>>,
    waiter: ExitWaiter,
) -> std::io::Result<std::process::ExitStatus> {
    waiter.wait()?;
    loop {
        if let Some(status) = inner.lock().unwrap().try_wait()? {
            return Ok(status);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Blocks until the child exits, leaving it in a waitable (unreaped) state so
/// that the owning wrapper can still collect the exit status.
///
/// On Windows the process handle is opened at construction time, while the
/// caller is guaranteed to still own the child, so waiting can never resolve
/// a recycled pid to an unrelated process.
struct ExitWaiter {
    #[cfg(unix)]
    pid: u32,
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
}

// SAFETY (windows): the handle is an owned kernel handle; sending it across
// threads is safe, and it is only closed once, in Drop.
#[cfg(windows)]
unsafe impl Send for ExitWaiter {}

#[cfg(windows)]
impl Drop for ExitWaiter {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        unsafe { CloseHandle(self.handle) }.ok();
    }
}

#[cfg(unix)]
impl ExitWaiter {
    fn new(pid: u32) -> std::io::Result<Self> {
        Ok(Self { pid })
    }

    fn wait(&self) -> std::io::Result<()> {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        loop {
            // SAFETY: `info` is a valid, writable `siginfo_t`. `WNOWAIT`
            // leaves the child reapable, so this never steals the status
            // from `try_wait`.
            let ret = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if ret == 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            // ECHILD: the child was already reaped by another wait; the
            // wrapper holds the cached exit status, so let `try_wait`
            // return it.
            if err.raw_os_error() == Some(libc::ECHILD) {
                return Ok(());
            }
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

#[cfg(windows)]
impl ExitWaiter {
    fn new(pid: u32) -> std::io::Result<Self> {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
        // SAFETY: `pid` refers to a process the caller still owns a handle
        // to, so it is live and cannot have been recycled.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Self { handle })
    }

    fn wait(&self) -> std::io::Result<()> {
        use windows::Win32::{
            Foundation::WAIT_OBJECT_0,
            System::Threading::{WaitForSingleObject, INFINITE},
        };
        let result = unsafe { WaitForSingleObject(self.handle, INFINITE) };
        if result == WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

fn spawn_wait_thread(
    wait_fn: impl FnOnce() -> std::io::Result<std::process::ExitStatus> + Send + 'static,
    tx: Sender<CommandEvent>,
    guard: Arc<RwLock<()>>,
) {
    spawn(move || {
        let _ = match wait_fn() {
            Ok(status) => {
                let _l = guard.write().unwrap();
                block_on_task(async move {
                    tx.send(CommandEvent::Terminated(TerminatedPayload {
                        code: status.code(),
                        #[cfg(windows)]
                        signal: None,
                        #[cfg(unix)]
                        signal: status.signal(),
                    }))
                    .await
                })
            }
            Err(e) => {
                let _l = guard.write().unwrap();
                block_on_task(async move { tx.send(CommandEvent::Error(e.to_string())).await })
            }
        };
    });
}

// tests for the commands functions.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_command_path_resolves() {
        let cwd_parent = platform::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent() // Go up once more to get out of the "deps" directory
            .unwrap()
            .to_owned();
        assert_eq!(
            relative_command_path(Path::new("Tauri.Example")).unwrap(),
            cwd_parent.join(if cfg!(windows) {
                "Tauri.Example.exe"
            } else {
                "Tauri.Example"
            })
        );
        assert_eq!(
            relative_command_path(Path::new("Tauri.Example.exe")).unwrap(),
            cwd_parent.join(if cfg!(windows) {
                "Tauri.Example.exe"
            } else {
                "Tauri.Example"
            })
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn test_cmd_spawn_output() {
        let cmd = Command::new("cat").args(["test/test.txt"]);
        let (mut rx, _) = cmd.spawn().unwrap();

        tauri::async_runtime::block_on(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    CommandEvent::Terminated(payload) => {
                        assert_eq!(payload.code, Some(0));
                    }
                    CommandEvent::Stdout(line) => {
                        assert_eq!(String::from_utf8(line).unwrap(), "This is a test doc!");
                    }
                    _ => {}
                }
            }
        });
    }

    #[cfg(not(windows))]
    #[test]
    fn test_cmd_spawn_raw_output() {
        let cmd = Command::new("cat").args(["test/test.txt"]);
        let (mut rx, _) = cmd.spawn().unwrap();

        tauri::async_runtime::block_on(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    CommandEvent::Terminated(payload) => {
                        assert_eq!(payload.code, Some(0));
                    }
                    CommandEvent::Stdout(line) => {
                        assert_eq!(String::from_utf8(line).unwrap(), "This is a test doc!");
                    }
                    _ => {}
                }
            }
        });
    }

    #[cfg(not(windows))]
    #[test]
    // test the failure case
    fn test_cmd_spawn_fail() {
        let cmd = Command::new("cat").args(["test/"]);
        let (mut rx, _) = cmd.spawn().unwrap();

        tauri::async_runtime::block_on(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    CommandEvent::Terminated(payload) => {
                        assert_eq!(payload.code, Some(1));
                    }
                    CommandEvent::Stderr(line) => {
                        assert_eq!(
                            String::from_utf8(line).unwrap(),
                            "cat: test/: Is a directory\n"
                        );
                    }
                    _ => {}
                }
            }
        });
    }

    #[cfg(not(windows))]
    #[test]
    // test the failure case (raw encoding)
    fn test_cmd_spawn_raw_fail() {
        let cmd = Command::new("cat").args(["test/"]);
        let (mut rx, _) = cmd.spawn().unwrap();

        tauri::async_runtime::block_on(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    CommandEvent::Terminated(payload) => {
                        assert_eq!(payload.code, Some(1));
                    }
                    CommandEvent::Stderr(line) => {
                        assert_eq!(
                            String::from_utf8(line).unwrap(),
                            "cat: test/: Is a directory\n"
                        );
                    }
                    _ => {}
                }
            }
        });
    }

    #[cfg(not(windows))]
    #[test]
    fn test_cmd_output_output() {
        let cmd = Command::new("cat").args(["test/test.txt"]);
        let output = tauri::async_runtime::block_on(cmd.output()).unwrap();

        assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "This is a test doc!\n"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn test_cmd_output_output_fail() {
        let cmd = Command::new("cat").args(["test/"]);
        let output = tauri::async_runtime::block_on(cmd.output()).unwrap();

        assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "cat: test/: Is a directory\n\n"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn test_cmd_spawn_process_group_output() {
        let cmd = Command::new("cat")
            .args(["test/test.txt"])
            .set_process_group(true);
        let (mut rx, _) = cmd.spawn().unwrap();

        tauri::async_runtime::block_on(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    CommandEvent::Terminated(payload) => {
                        assert_eq!(payload.code, Some(0));
                    }
                    CommandEvent::Stdout(line) => {
                        assert_eq!(String::from_utf8(line).unwrap(), "This is a test doc!");
                    }
                    _ => {}
                }
            }
        });
    }

    #[cfg(not(windows))]
    #[test]
    fn test_cmd_process_group_kill() {
        // Spawn a shell that runs a sleep command as a child process.
        // With process_group enabled, killing the parent should also kill the child.
        let cmd = Command::new("sh")
            .args(["-c", "sleep 60"])
            .set_process_group(true);
        let (mut rx, child) = cmd.spawn().unwrap();
        let pid = child.pid();

        // Verify the process is running
        let ret = unsafe { libc::kill(pid as i32, 0) };
        assert_eq!(ret, 0, "process should be running");

        // Kill the process group
        child.kill().unwrap();

        tauri::async_runtime::block_on(async move {
            while let Some(event) = rx.recv().await {
                if let CommandEvent::Terminated(payload) = event {
                    // Process was killed by signal, so code is None and signal is Some
                    assert!(payload.code.is_none() || payload.signal.is_some());
                    break;
                }
            }
        });

        // Verify the process group is gone
        let ret = unsafe { libc::killpg(pid as i32, 0) };
        assert_ne!(ret, 0, "process group should no longer exist");
    }

    #[cfg(not(windows))]
    #[test]
    fn test_cmd_process_group_output() {
        let cmd = Command::new("cat")
            .args(["test/test.txt"])
            .set_process_group(true);
        let output = tauri::async_runtime::block_on(cmd.output()).unwrap();

        assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "This is a test doc!\n"
        );
    }

    /// End-to-end test simulating the PyInstaller scenario from issue #1332.
    ///
    /// PyInstaller wraps the real application in a thin bootloader process.
    /// Without process groups, killing the bootloader orphans the real app.
    /// This test verifies that with `process_group` enabled, killing the
    /// wrapper also kills the grandchild process.
    #[cfg(not(windows))]
    #[test]
    fn test_pyinstaller_simulation_without_process_group() {
        // Without process_group: killing the wrapper does NOT kill the grandchild.
        let cmd = Command::new("sh").args(["test/pyinstaller_sim.sh"]);
        let (mut rx, child) = cmd.spawn().unwrap();

        // Collect the child PID from stdout
        let grandchild_pid = tauri::async_runtime::block_on(async {
            let mut pid = None;
            while let Some(event) = rx.recv().await {
                if let CommandEvent::Stdout(line) = &event {
                    let line_str = String::from_utf8_lossy(line);
                    if let Some(rest) = line_str.strip_prefix("CHILD_PID=") {
                        pid = rest.trim().parse::<i32>().ok();
                    }
                }
                if pid.is_some() {
                    break;
                }
            }
            pid.expect("should have received CHILD_PID from script")
        });

        // Verify the grandchild is running
        let ret = unsafe { libc::kill(grandchild_pid, 0) };
        assert_eq!(ret, 0, "grandchild should be running before kill");

        // Kill just the direct child (no process group)
        child.kill().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        // The grandchild is STILL alive — this is the bug
        let ret = unsafe { libc::kill(grandchild_pid, 0) };
        assert_eq!(
            ret, 0,
            "grandchild should survive when process_group is off"
        );

        // Clean up the orphaned grandchild
        unsafe { libc::kill(grandchild_pid, libc::SIGKILL) };
    }

    #[cfg(not(windows))]
    #[test]
    fn test_pyinstaller_simulation_with_process_group() {
        // With process_group: killing the wrapper ALSO kills the grandchild.
        let cmd = Command::new("sh")
            .args(["test/pyinstaller_sim.sh"])
            .set_process_group(true);
        let (mut rx, child) = cmd.spawn().unwrap();

        // Collect the grandchild PID from stdout
        let grandchild_pid = tauri::async_runtime::block_on(async {
            let mut pid = None;
            while let Some(event) = rx.recv().await {
                if let CommandEvent::Stdout(line) = &event {
                    let line_str = String::from_utf8_lossy(line);
                    if let Some(rest) = line_str.strip_prefix("CHILD_PID=") {
                        pid = rest.trim().parse::<i32>().ok();
                    }
                }
                if pid.is_some() {
                    break;
                }
            }
            pid.expect("should have received CHILD_PID from script")
        });

        // Verify the grandchild is running
        let ret = unsafe { libc::kill(grandchild_pid, 0) };
        assert_eq!(ret, 0, "grandchild should be running before kill");

        // Kill the process group
        child.kill().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        // The grandchild should now be DEAD
        let ret = unsafe { libc::kill(grandchild_pid, 0) };
        assert_ne!(
            ret, 0,
            "grandchild should be killed when process_group is on"
        );
    }

    /// Race the wait thread against kill() repeatedly: every cycle must end
    /// in a Terminated event — an Error event means one side lost the reap
    /// race unhandled. Slow-ish, so ignored by default; run explicitly with
    /// `cargo test -- --ignored stress_spawn_kill`.
    #[cfg(not(windows))]
    #[test]
    #[ignore]
    fn stress_spawn_kill() {
        for i in 0..300 {
            let cmd = Command::new("sleep")
                .args(["5"])
                .set_process_group(i % 2 == 0);
            let (mut rx, child) = cmd.spawn().unwrap();
            child.kill().unwrap();
            let got = tauri::async_runtime::block_on(async move {
                while let Some(ev) = rx.recv().await {
                    match ev {
                        CommandEvent::Terminated(_) => return true,
                        CommandEvent::Error(e) => panic!("cycle {i}: error event: {e}"),
                        _ => {}
                    }
                }
                false
            });
            assert!(got, "cycle {i}: channel closed without Terminated event");
        }
    }

    /// Windows kill regression tests.
    ///
    /// These drive the real `spawn()`/`kill()` (os_pipe plumbing, reader
    /// guard, bounded event channel, tauri async runtime). Each test prints
    /// stage markers and arms a watchdog so a hang identifies its stage in
    /// CI logs instead of timing out silently.
    #[cfg(windows)]
    mod win_diag {
        use super::super::*;
        use std::time::Duration;

        fn watchdog(name: &'static str) {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(60));
                println!("WATCHDOG: {name} HUNG");
                std::io::Write::flush(&mut std::io::stdout()).ok();
                std::process::exit(101);
            });
        }

        fn mark(name: &str, stage: &str) {
            println!("[{name}] {stage}");
            std::io::Write::flush(&mut std::io::stdout()).ok();
        }

        /// Drain events until Terminated or timeout; panics on timeout.
        fn expect_terminated(name: &'static str, mut rx: Receiver<CommandEvent>) {
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                tauri::async_runtime::block_on(async move {
                    while let Some(event) = rx.recv().await {
                        if let CommandEvent::Terminated(payload) = event {
                            let _ = done_tx.send(payload);
                            return;
                        }
                    }
                });
            });
            match done_rx.recv_timeout(Duration::from_secs(20)) {
                Ok(payload) => mark(name, &format!("terminated: {payload:?}")),
                Err(e) => panic!("[{name}] no Terminated event within 20s: {e}"),
            }
        }

        fn long_running() -> Command {
            Command::new("ping").args(["-n", "90", "127.0.0.1"])
        }

        fn tree_running() -> Command {
            Command::new("cmd").args([
                "/C",
                "start /B ping -n 90 127.0.0.1 > NUL & ping -n 90 127.0.0.1 > NUL",
            ])
        }

        fn kill_scenario(name: &'static str, cmd: Command, expect_event: bool) {
            watchdog(name);
            mark(name, "spawning");
            let (rx, child) = cmd.spawn().unwrap();
            mark(name, &format!("spawned pid {}", child.pid()));
            std::thread::sleep(Duration::from_millis(500));
            mark(name, "killing");
            child.kill().unwrap();
            mark(name, "killed");
            if expect_event {
                expect_terminated(name, rx);
            }
        }

        #[test]
        fn diag_kill_plain_live() {
            kill_scenario("plain-live", long_running(), true);
        }

        #[test]
        fn diag_kill_job_live() {
            kill_scenario("job-live", long_running().set_process_group(true), true);
        }

        #[test]
        fn diag_kill_job_tree() {
            kill_scenario("job-tree", tree_running().set_process_group(true), true);
        }

        /// Without a process group the `start /B` grandchild survives the
        /// kill and keeps the stdio pipes open, so Terminated is expected to
        /// lag — only assert that kill() itself returns.
        #[test]
        fn diag_kill_plain_tree_kill_returns() {
            kill_scenario("plain-tree", tree_running(), false);
        }

        #[test]
        fn diag_kill_job_after_exit() {
            watchdog("job-after-exit");
            let cmd = Command::new("cmd")
                .args(["/C", "echo done"])
                .set_process_group(true);
            mark("job-after-exit", "spawning");
            let (rx, child) = cmd.spawn().unwrap();
            mark("job-after-exit", &format!("spawned pid {}", child.pid()));
            std::thread::sleep(Duration::from_secs(2));
            mark("job-after-exit", "killing exited child");
            // Child exited long ago; kill() must not hang on the stale job.
            let _ = child.kill();
            mark("job-after-exit", "killed");
            expect_terminated("job-after-exit", rx);
        }

        #[test]
        fn diag_output_echo() {
            watchdog("output-echo");
            let output = tauri::async_runtime::block_on(
                Command::new("cmd").args(["/C", "echo hello"]).output(),
            )
            .unwrap();
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
        }
    }
}

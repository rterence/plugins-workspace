//! Diagnostic harness for the Windows `kill()` hang reported on
//! tauri-apps/plugins-workspace#3351.
//!
//! Replicates the shell plugin's exact concurrency pattern: the process-wrap
//! child lives behind an `Arc<Mutex<...>>`, a wait thread blocks on the raw
//! process (no reap) outside the lock, and `kill()` goes through the lock.
//!
//! Each scenario runs in its own child invocation of this binary (so a hang in
//! one scenario cannot block the rest), prints a stage marker before/after
//! every step, and a watchdog aborts with the last stage after 30s.

use std::{
    io::Write as _,
    process::{Command as StdCommand, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use process_wrap::std::{StdChildWrapper, StdCommandWrap};

const STAGES: &[&str] = &[
    "0:init",
    "1:spawned",
    "2:wait-thread-started",
    "3:kill-lock-acquiring",
    "4:kill-lock-acquired-calling-kill",
    "5:kill-returned",
    "6:terminated-event-received",
    "7:done",
];

static STAGE: AtomicUsize = AtomicUsize::new(0);

fn stage(n: usize) {
    STAGE.store(n, Ordering::SeqCst);
    println!("STAGE {}", STAGES[n]);
    std::io::stdout().flush().ok();
}

fn watchdog(scenario: &'static str) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(30));
        let n = STAGE.load(Ordering::SeqCst);
        println!("WATCHDOG: scenario {scenario} HUNG after stage {}", STAGES[n]);
        std::io::stdout().flush().ok();
        std::process::exit(42);
    });
}

/// A long-running command that stays alive ~90s.
fn long_running() -> StdCommand {
    let mut c = StdCommand::new("ping");
    c.args(["-n", "90", "127.0.0.1"]);
    c
}

/// cmd wrapper that backgrounds a grandchild ping (survives the wrapper) and
/// stays alive itself via a foreground ping.
fn tree_running() -> StdCommand {
    let mut c = StdCommand::new("cmd");
    c.args([
        "/C",
        "start /B ping -n 90 127.0.0.1 > NUL & ping -n 90 127.0.0.1 > NUL",
    ]);
    c
}

/// cmd wrapper that backgrounds a grandchild and exits immediately —
/// the PyInstaller orphaning shape.
fn wrapper_exits_fast() -> StdCommand {
    let mut c = StdCommand::new("cmd");
    c.args(["/C", "start /B ping -n 90 127.0.0.1 > NUL"]);
    c
}

fn wrap(mut cmd: StdCommand, job_object: bool) -> StdCommandWrap {
    cmd.stdout(Stdio::null()).stderr(Stdio::null()).stdin(Stdio::null());
    let mut w = StdCommandWrap::from(cmd);
    if job_object {
        #[cfg(windows)]
        {
            use windows::Win32::System::Threading::CREATE_NO_WINDOW;
            w.wrap(process_wrap::std::CreationFlags(CREATE_NO_WINDOW));
            w.wrap(process_wrap::std::JobObject);
        }
        #[cfg(unix)]
        w.wrap(process_wrap::std::ProcessGroup::leader());
    }
    w
}

/// Mirror of the plugin's `wait_for_exit_without_reaping` (windows).
#[cfg(windows)]
fn raw_wait_no_reap(pid: u32) -> std::io::Result<()> {
    use windows::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE},
    };
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let result = unsafe { WaitForSingleObject(handle, INFINITE) };
    unsafe { CloseHandle(handle) }.ok();
    if result == WAIT_OBJECT_0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn raw_wait_no_reap(_pid: u32) -> std::io::Result<()> {
    unimplemented!("diagnostic targets windows")
}

/// Mirror of the plugin's wait thread. `raw_first` selects the new no-poll
/// behavior (raw wait outside the lock, then try_wait) vs the old 10ms
/// try_wait polling loop.
fn spawn_wait_thread(
    inner: Arc<Mutex<Box<dyn StdChildWrapper>>>,
    pid: u32,
    raw_first: bool,
) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if raw_first {
            if let Err(e) = raw_wait_no_reap(pid) {
                let _ = tx.send(format!("raw-wait-error: {e}"));
                return;
            }
            println!("  [wait-thread] raw wait returned");
            std::io::stdout().flush().ok();
        }
        loop {
            match inner.lock().unwrap().try_wait() {
                Ok(Some(status)) => {
                    let _ = tx.send(format!("terminated: {status:?}"));
                    return;
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = tx.send(format!("wait-error: {e}"));
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    rx
}

/// Full plugin-shaped scenario: spawn, wait thread, optional settle delay,
/// then kill through the lock from the main thread.
fn run_scenario(
    name: &'static str,
    cmd: StdCommand,
    job_object: bool,
    raw_first: bool,
    delay_before_kill_ms: u64,
) {
    watchdog(name);
    println!("=== scenario {name} (job_object={job_object}, raw_first={raw_first}, delay={delay_before_kill_ms}ms) ===");
    stage(0);

    let mut wrapped = wrap(cmd, job_object);
    let child = wrapped.spawn().expect("spawn failed");
    let pid = child.id();
    println!("  pid={pid}");
    stage(1);

    let inner: Arc<Mutex<Box<dyn StdChildWrapper>>> = Arc::new(Mutex::new(child));
    let rx = spawn_wait_thread(inner.clone(), pid, raw_first);
    stage(2);

    if delay_before_kill_ms > 0 {
        std::thread::sleep(Duration::from_millis(delay_before_kill_ms));
    }

    stage(3);
    let mut guard = inner.lock().unwrap();
    stage(4);
    guard.kill().expect("kill failed");
    drop(guard);
    stage(5);

    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(msg) => println!("  [main] wait thread reported: {msg}"),
        Err(e) => {
            println!("  [main] NO terminated event within 10s: {e}");
            std::io::stdout().flush().ok();
            std::process::exit(43);
        }
    }
    stage(6);
    stage(7);
    println!("=== scenario {name} OK ===");
}

/// Minimal process-wrap-only sanity check: no wait thread at all.
fn run_bare_kill(name: &'static str, cmd: StdCommand, job_object: bool) {
    watchdog(name);
    println!("=== scenario {name} (bare kill, job_object={job_object}) ===");
    stage(0);
    let mut wrapped = wrap(cmd, job_object);
    let mut child = wrapped.spawn().expect("spawn failed");
    println!("  pid={}", child.id());
    stage(1);
    stage(4);
    child.kill().expect("kill failed");
    stage(5);
    stage(7);
    println!("=== scenario {name} OK ===");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scenario = args.get(1).map(String::as_str);

    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .init();
    }

    match scenario {
        // process-wrap alone, no plugin machinery
        Some("bare-plain") => run_bare_kill("bare-plain", long_running(), false),
        Some("bare-job") => run_bare_kill("bare-job", long_running(), true),
        Some("bare-job-tree") => run_bare_kill("bare-job-tree", tree_running(), true),

        // plugin-shaped, old polling wait thread
        Some("poll-plain") => run_scenario("poll-plain", long_running(), false, false, 500),
        Some("poll-job") => run_scenario("poll-job", long_running(), true, false, 500),
        Some("poll-job-tree") => run_scenario("poll-job-tree", tree_running(), true, false, 500),

        // plugin-shaped, new raw-wait-first wait thread (branch perf/shell-wait-no-poll)
        Some("raw-plain") => run_scenario("raw-plain", long_running(), false, true, 500),
        Some("raw-job") => run_scenario("raw-job", long_running(), true, true, 500),
        Some("raw-job-tree") => run_scenario("raw-job-tree", tree_running(), true, true, 500),
        // wrapper exits fast, grandchild lingers; kill after the wait thread
        // has already collected the wrapper's exit
        Some("raw-job-orphan") => {
            run_scenario("raw-job-orphan", wrapper_exits_fast(), true, true, 3000)
        }
        // kill immediately, racing spawn/wait-thread startup
        Some("raw-job-instant") => run_scenario("raw-job-instant", long_running(), true, true, 0),

        Some(other) => {
            eprintln!("unknown scenario {other}");
            std::process::exit(2);
        }
        None => {
            // Driver mode: run every scenario as a subprocess so one hang
            // doesn't stop the matrix.
            let exe = std::env::current_exe().unwrap();
            let all = [
                "bare-plain",
                "bare-job",
                "bare-job-tree",
                "poll-plain",
                "poll-job",
                "poll-job-tree",
                "raw-plain",
                "raw-job",
                "raw-job-tree",
                "raw-job-orphan",
                "raw-job-instant",
            ];
            let mut failures = Vec::new();
            for s in all {
                println!("\n########## {s} ##########");
                std::io::stdout().flush().ok();
                let status = StdCommand::new(&exe)
                    .arg(s)
                    .env("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_default())
                    .status()
                    .expect("failed to run scenario subprocess");
                if !status.success() {
                    println!("########## {s} FAILED: {status:?} ##########");
                    failures.push(s);
                }
                std::io::stdout().flush().ok();
            }
            println!("\n==================== SUMMARY ====================");
            if failures.is_empty() {
                println!("all scenarios passed");
            } else {
                println!("FAILED scenarios: {failures:?}");
                std::process::exit(1);
            }
        }
    }
}

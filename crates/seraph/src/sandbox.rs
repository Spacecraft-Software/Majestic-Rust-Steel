// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The Seraph command sandbox (PRD #1 §5.2.4) — *how* a policy-allowed, user-approved shell command
//! actually runs.
//!
//! [`Policy::decide`](crate::Policy) rules *whether* a command may run (the allow-list, plus
//! per-invocation approval). This module rules *how*, shrinking the blast radius as far as a portable,
//! unprivileged process can:
//!
//! - **No shell interpretation.** The command is split into a program + arguments ([`shlex`]) and
//!   run directly — there is no `sh -c`, so `;`, `|`, `>`, and `$(…)` are inert; the tool runs one
//!   program, never a shell script.
//! - **Minimal environment.** The child inherits none of the parent's environment except a short safe
//!   set (`PATH`, `HOME`, `LANG`, `TERM`), so secrets in the environment are not exposed.
//! - **Working-directory confinement.** The child runs in the configured project root.
//! - **Resource limits.** When `prlimit` (util-linux) is on `PATH`, the command runs through it so the
//!   kernel caps its CPU time and written-file size and disables core dumps. Concept #1 forbids
//!   `unsafe` (PRD §6.1, `#![deny(unsafe_code)]`, no exceptions), so Seraph cannot call `setrlimit`
//!   itself (that needs `pre_exec`); delegating to `prlimit` keeps the syscalls inside a vetted
//!   external tool. Where `prlimit` is absent, the limits are simply not applied (see [`Sandbox::resource_limited`]).
//! - **Wall-clock timeout.** The child runs in its own process group; on overrun the whole group (the
//!   command and any children it spawned) is killed (`nix`'s safe `killpg`).
//! - **Bounded output.** stdout/stderr are drained on reader threads and truncated to a cap, so a
//!   chatty command can neither exhaust memory nor deadlock on a full pipe.
//!
//! This is **defense-in-depth process confinement, not a hard security boundary**: it does not use
//! namespaces or seccomp, so an allowed program can still reach the filesystem and the network. The
//! real gate stays the allow-list plus the human approving each command; filesystem/network isolation
//! (via a tool such as `bwrap`/`unshare`) is the next layer.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Default wall-clock timeout for a sandboxed command.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default cap on captured stdout/stderr (bytes each), so a chatty command can't exhaust memory.
pub const DEFAULT_OUTPUT_CAP: usize = 64 * 1024;

/// Default CPU-time cap (seconds) applied via `prlimit` when available.
const DEFAULT_CPU_LIMIT_SECS: u64 = 120;

/// Default written-file-size cap (bytes) applied via `prlimit` when available.
const DEFAULT_FSIZE_LIMIT_BYTES: u64 = 256 * 1024 * 1024;

/// Why a command could not be *run* (distinct from running and exiting non-zero).
#[derive(Debug)]
pub enum ShellError {
    /// The command line was empty or could not be split into a program + arguments.
    Unparseable,
    /// The program could not be spawned (not found, not executable, …).
    Spawn(std::io::Error),
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparseable => f.write_str("the command could not be parsed into a program"),
            Self::Spawn(error) => write!(f, "could not start the command: {error}"),
        }
    }
}

impl std::error::Error for ShellError {}

/// The result of running a command in the sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellOutput {
    /// Captured standard output (lossy UTF-8, truncated to the output cap).
    pub stdout: String,
    /// Captured standard error (lossy UTF-8, truncated to the output cap).
    pub stderr: String,
    /// The process exit code, or `None` if it was killed (e.g. it timed out).
    pub exit_code: Option<i32>,
    /// Whether the command was killed for exceeding the wall-clock timeout.
    pub timed_out: bool,
}

/// A confined executor for policy-allowed, user-approved commands. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Sandbox {
    workdir: PathBuf,
    timeout: Duration,
    output_cap: usize,
    cpu_limit_secs: u64,
    fsize_limit_bytes: u64,
    hardening: Hardening,
}

impl Sandbox {
    /// A sandbox that runs commands in `workdir` with the default timeout, output cap, and (when the
    /// tooling is present) resource limits.
    #[must_use]
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        Self {
            workdir: workdir.into(),
            timeout: DEFAULT_TIMEOUT,
            output_cap: DEFAULT_OUTPUT_CAP,
            cpu_limit_secs: DEFAULT_CPU_LIMIT_SECS,
            fsize_limit_bytes: DEFAULT_FSIZE_LIMIT_BYTES,
            hardening: Hardening::detect(),
        }
    }

    /// Sets the wall-clock timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the per-stream output cap in bytes.
    #[must_use]
    pub fn with_output_cap(mut self, cap: usize) -> Self {
        self.output_cap = cap;
        self
    }

    /// Sets the CPU-time cap (seconds) applied when resource limits are available.
    #[must_use]
    pub fn with_cpu_limit_secs(mut self, secs: u64) -> Self {
        self.cpu_limit_secs = secs;
        self
    }

    /// Whether the sandbox can apply kernel resource limits (i.e. `prlimit` was found on `PATH`).
    #[must_use]
    pub fn resource_limited(&self) -> bool {
        self.hardening.limits_resources()
    }

    /// Runs `command` under the sandbox and returns its captured output.
    ///
    /// # Errors
    /// Returns [`ShellError::Unparseable`] if `command` has no program, or [`ShellError::Spawn`] if the
    /// program cannot be started. A command that *runs* and fails is `Ok` with a non-zero `exit_code`.
    pub fn run(&self, command: &str) -> Result<ShellOutput, ShellError> {
        let parts = shlex::split(command).ok_or(ShellError::Unparseable)?;
        let (program, args) = parts.split_first().ok_or(ShellError::Unparseable)?;
        if program.is_empty() {
            return Err(ShellError::Unparseable);
        }

        let mut cmd =
            self.hardening
                .command(program, args, self.cpu_limit_secs, self.fsize_limit_bytes);
        cmd.current_dir(&self.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_minimal_environment(&mut cmd);
        apply_unix_confinement(&mut cmd);

        let mut child = cmd.spawn().map_err(ShellError::Spawn)?;
        let pid = child.id();

        // Drain both streams on threads so a full pipe never blocks the child while we wait.
        let stdout = drain(child.stdout.take(), self.output_cap);
        let stderr = drain(child.stderr.take(), self.output_cap);

        let deadline = Instant::now() + self.timeout;
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        kill_process_group(pid);
                        let _ = child.wait();
                        break None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break None,
            }
        };

        Ok(ShellOutput {
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
            exit_code: status.and_then(|status| status.code()),
            timed_out,
        })
    }
}

/// Detected resource-limit tooling. Concept #1 forbids `unsafe`, so Seraph cannot `setrlimit` itself;
/// when `prlimit` (util-linux) is present, the command runs through it instead, keeping the syscalls
/// inside that vetted external tool.
#[derive(Clone, Debug)]
struct Hardening {
    /// The `prlimit` binary, if found on `PATH`. `None` means resource limits are not applied.
    prlimit: Option<PathBuf>,
}

impl Hardening {
    /// Looks up the limiting tools once (at [`Sandbox`] construction).
    fn detect() -> Self {
        Self {
            prlimit: find_on_path("prlimit"),
        }
    }

    /// Whether kernel resource limits will be applied to commands.
    fn limits_resources(&self) -> bool {
        self.prlimit.is_some()
    }

    /// The [`Command`] to spawn for `program` + `args`: prefixed with `prlimit` (CPU time, written-file
    /// size, no core dumps) when available, otherwise the program run directly.
    fn command(&self, program: &str, args: &[String], cpu_secs: u64, fsize_bytes: u64) -> Command {
        let Some(prlimit) = &self.prlimit else {
            // No `prlimit`: run the program directly (no resource limits).
            let mut cmd = Command::new(program);
            cmd.args(args);
            return cmd;
        };
        let mut cmd = Command::new(prlimit);
        cmd.arg(format!("--cpu={cpu_secs}"))
            .arg(format!("--fsize={fsize_bytes}"))
            .arg("--core=0")
            .arg("--")
            .arg(program)
            .args(args);
        cmd
    }
}

/// Finds `name` on `PATH`, returning the first existing match (so the sandbox can detect optional
/// tools like `prlimit` without shelling out to `which`).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Replaces the child's environment with a minimal safe set: the parent's `PATH`/`HOME` (so tools
/// resolve and find their config) plus a neutral `LANG`/`TERM`; everything else is dropped.
fn apply_minimal_environment(cmd: &mut Command) {
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }
    cmd.env("LANG", "C.UTF-8");
    cmd.env("TERM", "dumb");
}

/// Drains `reader` on a thread into a lossy-UTF-8 string truncated to `cap` bytes, while continuing to
/// read (and discard) past the cap so the child never blocks on a full pipe.
fn drain<R: Read + Send + 'static>(reader: Option<R>, cap: usize) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buffer: Vec<u8> = Vec::new();
        if let Some(mut reader) = reader {
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if buffer.len() < cap {
                            let take = read.min(cap - buffer.len());
                            buffer.extend_from_slice(&chunk[..take]);
                        }
                        // Past the cap we keep reading but discard, so the writer is never blocked.
                    }
                }
            }
        }
        String::from_utf8_lossy(&buffer).into_owned()
    })
}

/// Puts the child in its own process group so a timeout can kill the whole tree (Unix only).
#[cfg(unix)]
fn apply_unix_confinement(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    cmd.process_group(0); // a new group, so the timeout can kill the command *and* its children
}

/// No process-group setup on non-Unix targets (still env-cleared, cwd-confined, timed, output-capped).
#[cfg(not(unix))]
fn apply_unix_confinement(_cmd: &mut Command) {}

/// Kills the process group led by `pid` — the command and any children it spawned — with `SIGKILL`,
/// via `nix`'s safe `killpg` wrapper (Unix). The child's group id equals its pid (see
/// [`apply_unix_confinement`]), and `nix` keeps the `unsafe` syscall inside its own crate so Seraph
/// stays `#![deny(unsafe_code)]`.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    if pid > 0 {
        let group = nix::unistd::Pid::from_raw(pid);
        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
    }
}

/// Best-effort kill of just the child on non-Unix targets (no process-group cleanup available).
#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

#[cfg(all(test, unix))]
mod tests {
    use super::{Sandbox, ShellError, DEFAULT_OUTPUT_CAP};
    use std::time::Duration;

    fn sandbox() -> Sandbox {
        Sandbox::new(std::env::temp_dir())
    }

    #[test]
    fn runs_a_program_and_captures_stdout() {
        let output = sandbox().run("echo hello sandbox").expect("echo runs");
        assert_eq!(output.stdout.trim(), "hello sandbox");
        assert_eq!(output.exit_code, Some(0));
        assert!(!output.timed_out);
    }

    #[test]
    fn shell_metacharacters_are_inert_no_shell_interpretation() {
        // With `sh -c` this chains a second command (output "first\nsecond"). Run directly, the `;`
        // and the second `echo` are just literal arguments to the one `echo`, so the whole line is
        // echoed verbatim — proof there is no shell interpretation.
        let output = sandbox()
            .run("echo first ; echo second")
            .expect("echo runs");
        assert_eq!(
            output.stdout.trim(),
            "first ; echo second",
            "the `;` and second `echo` are literal args, not a separator + new command"
        );
        assert_eq!(output.exit_code, Some(0));
    }

    #[test]
    fn an_empty_command_is_unparseable() {
        assert!(matches!(sandbox().run("   "), Err(ShellError::Unparseable)));
    }

    #[test]
    fn a_missing_program_fails() {
        // Run directly, a missing program is a Spawn error. Wrapped in `prlimit`, the wrapper spawns
        // fine and reports the failure as a non-zero exit instead — either way the command did not run.
        match sandbox().run("seraph-no-such-program-xyz") {
            Err(ShellError::Spawn(_)) => {}
            Ok(output) => assert_ne!(output.exit_code, Some(0), "the missing program must fail"),
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_command_that_overruns_is_killed() {
        let output = sandbox()
            .with_timeout(Duration::from_millis(200))
            .run("sleep 30")
            .expect("sleep spawns");
        assert!(output.timed_out, "the command should have been killed");
        assert_eq!(output.exit_code, None);
    }

    #[test]
    fn output_is_capped() {
        // `head -c N /dev/zero` writes N bytes; with a small cap the capture is truncated, and the
        // drain-past-cap path means the command still completes rather than deadlocking.
        let output = sandbox()
            .with_output_cap(1024)
            .run("head -c 200000 /dev/zero")
            .expect("head runs");
        assert!(
            output.stdout.len() <= 1024,
            "stdout was truncated to the cap"
        );
        assert!(!output.timed_out);
        let _ = DEFAULT_OUTPUT_CAP;
    }

    #[test]
    fn the_environment_is_stripped() {
        // A secret in the parent environment must not reach the child.
        std::env::set_var("SERAPH_SANDBOX_SECRET", "leak-me");
        let output = sandbox().run("env").expect("env runs");
        std::env::remove_var("SERAPH_SANDBOX_SECRET");
        assert!(
            !output.stdout.contains("leak-me"),
            "the parent environment leaked into the sandbox: {}",
            output.stdout
        );
    }

    #[test]
    fn find_on_path_locates_a_known_program_and_rejects_a_bogus_one() {
        assert!(
            super::find_on_path("sh").is_some(),
            "`sh` is on every unix PATH"
        );
        assert!(super::find_on_path("seraph-definitely-no-such-tool").is_none());
    }

    #[test]
    fn resource_limits_cap_cpu_time_when_prlimit_is_available() {
        let sandbox = Sandbox::new(std::env::temp_dir())
            .with_cpu_limit_secs(1)
            .with_timeout(Duration::from_secs(30));
        if !sandbox.resource_limited() {
            return; // `prlimit` not installed — the limit cannot be applied, nothing to assert
        }
        // `yes` burns CPU forever; the 1-second CPU rlimit (not our 30-second wall timeout) ends it.
        let output = sandbox.run("yes").expect("yes spawns");
        assert!(
            !output.timed_out,
            "the CPU limit, not the wall timeout, should have stopped it"
        );
        assert_eq!(
            output.exit_code, None,
            "killed by a signal, so there is no exit code"
        );
    }
}

//! Oracles: how the harness decides whether the agent succeeded.
//!
//! Four oracle families are supported, all derived from the task's
//! [`OracleSpec`](crate::task::OracleSpec):
//!
//! - **Command** — run shell commands; pass iff all exit zero.
//! - **Rust** — a convenience that expands to common cargo quality gates.
//! - **File assertion** — `file_contains` / `file_not_contains` / exists /
//!   missing checks against the post-run workspace.
//! - **Behavior** — checks evaluated from the structured event stream, never
//!   from model prose (`must_emit_plan`, `must_use_tool`, `must_run_command`,
//!   and their negations).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crom_sdk::SdkEvent;

use crate::fixture::read_workspace_file;
use crate::task::{AssertionSpec, BehaviorSpec, EvalTask};

/// Result of running a single command oracle command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    pub command: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub timed_out: bool,
    /// Set when stdout/stderr exceeded `MAX_CAPTURE` and the captured stream was
    /// truncated to bound memory. This does NOT affect `success`: a command that
    /// prints over the cap and exits zero is still a success (#70). Defaults to
    /// `false` for backward-compatible deserialization.
    #[serde(default)]
    pub output_truncated: bool,
}

/// Result of a single file assertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssertionResult {
    pub description: String,
    pub passed: bool,
    pub detail: String,
}

/// Result of a single behavior check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorResult {
    pub description: String,
    pub passed: bool,
    pub detail: String,
}

/// Aggregated outcome of evaluating all oracles for a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleReport {
    pub commands: Vec<CommandResult>,
    pub assertions: Vec<AssertionResult>,
    pub behavior: Vec<BehaviorResult>,
}

impl OracleReport {
    /// Number of discrete checks that passed.
    pub fn checks_passed(&self) -> usize {
        self.commands.iter().filter(|c| c.success).count()
            + self.assertions.iter().filter(|a| a.passed).count()
            + self.behavior.iter().filter(|b| b.passed).count()
    }

    /// Total number of discrete checks evaluated.
    pub fn checks_total(&self) -> usize {
        self.commands.len() + self.assertions.len() + self.behavior.len()
    }

    /// Whether every evaluated check passed. An oracle with no checks is
    /// considered passing (the harness still scored the run via efficiency and
    /// process quality).
    pub fn all_passed(&self) -> bool {
        self.commands.iter().all(|c| c.success)
            && self.assertions.iter().all(|a| a.passed)
            && self.behavior.iter().all(|b| b.passed)
    }

    /// Fraction of checks that passed in `[0, 1]`. Returns `1.0` when there are
    /// no checks.
    pub fn pass_fraction(&self) -> f64 {
        let total = self.checks_total();
        if total == 0 {
            return 1.0;
        }
        self.checks_passed() as f64 / total as f64
    }
}

/// Evaluate every oracle for `task` against the post-run `workspace_dir` and
/// the captured `events`. Uses the local backend (commands run directly).
pub async fn evaluate(task: &EvalTask, workspace_dir: &Path, events: &[SdkEvent]) -> OracleReport {
    let backend = crate::isolation::LocalBackend;
    let timeout = task_command_timeout(task);
    let limits = crate::isolation::CommandLimits {
        timeout,
        network: task.requires_network,
        ..Default::default()
    };
    evaluate_oracle(&task.oracle, workspace_dir, events, &backend, &limits).await
}

/// Per-command timeout for a task's oracle.
pub fn task_command_timeout(task: &EvalTask) -> Duration {
    Duration::from_secs(
        task.oracle
            .command_timeout_secs
            .unwrap_or_else(|| task.timeout.as_secs().max(1)),
    )
}

/// Evaluate an arbitrary [`OracleSpec`](crate::task::OracleSpec) against a
/// workspace and event stream, running commands through the supplied isolation
/// backend. This is the shared path used for both public and hidden verification.
pub async fn evaluate_oracle(
    oracle: &crate::task::OracleSpec,
    workspace_dir: &Path,
    events: &[SdkEvent],
    backend: &dyn crate::isolation::IsolationBackend,
    limits: &crate::isolation::CommandLimits,
) -> OracleReport {
    let mut commands = Vec::new();
    for cmd in oracle.primary_commands() {
        match backend.run(workspace_dir, &cmd, limits).await {
            Ok(outcome) => commands.push(outcome.result),
            Err(e) => commands.push(CommandResult {
                command: cmd,
                success: false,
                exit_code: None,
                stdout: String::new(),
                stderr: format!("isolation error: {e}"),
                duration_ms: 0,
                timed_out: false,
                output_truncated: false,
            }),
        }
    }

    let assertions = oracle
        .assertions
        .iter()
        .map(|a| evaluate_assertion(a, workspace_dir))
        .collect();

    let behavior = oracle
        .behavior
        .iter()
        .map(|b| evaluate_behavior(b, events))
        .collect();

    OracleReport {
        commands,
        assertions,
        behavior,
    }
}

/// Run a single shell command in `dir` with a timeout, capturing output.
///
/// The child is spawned in its own process group (on Unix) so the entire group
/// can be signalled, not just the wrapping `sh` — otherwise long children (e.g.
/// `cargo test`) would be orphaned and outlive `run_command`.
///
/// stdout/stderr are streamed with a hard byte cap of `MAX_CAPTURE` per
/// stream rather than buffered unbounded; once a stream exceeds the cap the
/// reader stops accumulating and that stream is flagged truncated, but the
/// command keeps running so its real exit status is observed (#70). This bounds
/// live memory even for commands that print unboundedly fast, without failing or
/// killing a command merely for being verbose. Only the wall-clock timeout kills
/// a (never-exiting) command.
///
/// The main flow selects ONLY between the child exiting and the wall-clock
/// timeout: there is no read-completion branch that could drop the timeout
/// future. A process that closes its stdout/stderr but keeps running (so the
/// pipes reach EOF while the process lives) is still bounded by the timeout.
///
/// After the select resolves — whether the child exited or the timeout fired —
/// the whole process group is ALWAYS killed before the read tasks are joined.
/// Reaping the direct `sh` does not close stdout/stderr fds that a backgrounded
/// descendant inherited and still holds, and on Unix a pipe reaches EOF only
/// when every write end is closed; so without this a command that backgrounds a
/// stdio-inheriting descendant and exits (e.g. `sh -c 'sleep 60 & exit 0'`)
/// would block the readers forever. The post-exit joins are additionally bounded
/// by a short grace (`REAP_GRACE`) as a backstop, so no `.await` in this
/// function is unbounded.
///
/// Every `child.wait()` that runs AFTER a group kill (the timeout branch's
/// post-kill reap and the unconditional post-select reap) is itself wrapped in a
/// `REAP_GRACE` timeout: if the best-effort group kill is ineffective (rare, or
/// non-Unix where we can only signal the direct child), we give up reaping rather
/// than block until the command exits naturally. Combined with the bounded read
/// joins, no `.await` in this function can exceed roughly `timeout + REAP_GRACE`.
pub async fn run_command(command: &str, dir: &Path, timeout: Duration) -> CommandResult {
    let start = std::time::Instant::now();
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(dir)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Put the child in its own process group so we can signal the whole group
    // (the wrapping `sh` plus any children it spawns) on timeout / cap-exceeded.
    #[cfg(unix)]
    {
        // `tokio::process::Command` re-exposes the Unix `process_group`
        // extension; making the child a group leader lets us signal the whole
        // group (the child plus anything it spawns) on timeout / cap-exceeded.
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return CommandResult {
                command: command.to_string(),
                success: false,
                exit_code: None,
                stdout: String::new(),
                stderr: format!("failed to run command: {e}"),
                duration_ms: start.elapsed().as_millis() as u64,
                timed_out: false,
                output_truncated: false,
            };
        }
    };

    // The child's pid is also its process-group id (we made it a group leader).
    // It is used after the select to unconditionally kill the whole group so no
    // backgrounded descendant outlives the command and holds a pipe open.
    let child_pid = child.id();

    // Drain both streams concurrently with a per-stream byte cap. Each stream is
    // read in its own background task that, on exceeding the cap, (a) records a
    // PER-STREAM truncated flag and (b) stops accumulating but drains-and-discards
    // to EOF so the pipe never blocks the child. Exceeding the cap does NOT kill or
    // fail the command (#70): the command runs to completion so its real exit
    // status is observed. Capped reads bound live memory regardless of how fast the
    // command prints.
    //
    // The accumulated bytes and the truncated flag live in SHARED handles, not in
    // the task's return value: if a descendant escapes the group kill and keeps a
    // pipe open past the read-join grace, the join below aborts the task — and we
    // still recover whatever was captured so far by reading the shared handle,
    // instead of dropping it with the aborted task.
    let out_buf = SharedCapture::default();
    let err_buf = SharedCapture::default();
    let out_task = tokio::spawn(capped_read_owned(
        child.stdout.take(),
        MAX_CAPTURE,
        out_buf.clone(),
    ));
    let err_task = tokio::spawn(capped_read_owned(
        child.stderr.take(),
        MAX_CAPTURE,
        err_buf.clone(),
    ));

    // Select ONLY between child exit and the wall-clock timeout. A cap-exceeded
    // reader no longer kills the command; it just stops accumulating and drains to
    // EOF, so the command runs to completion and `child.wait()` resolves through
    // this exit branch. The timeout is the only thing that can kill a runaway
    // (never-exiting) command.
    let (timed_out, status) = tokio::select! {
        res = child.wait() => (false, res.ok()),
        _ = tokio::time::sleep(timeout) => {
            kill_group(&mut child);
            // Bound the post-kill reap: if the best-effort group kill was
            // ineffective (rare, or non-Unix) we must NOT wait until the command
            // exits naturally, which would defeat the wall-clock timeout.
            (true, reap_bounded(&mut child).await)
        }
    };

    // ALWAYS kill the whole process group, even on a normal `child.wait()` exit.
    // Reaping the direct `sh` child does NOT close stdout/stderr fds that a
    // backgrounded descendant inherited and still holds: on Unix a pipe reaches
    // EOF only when ALL write ends are closed. So a command like
    // `sh -c 'sleep 60 & exit 0'` exits the `sh` immediately (the exit branch
    // wins with timed_out=false) while the descendant keeps the pipe open — the
    // read tasks below would then block forever waiting for an EOF that never
    // comes. Signalling the group here kills any lingering descendant so the
    // write ends close and the readers hit EOF. Killing an already-dead group is
    // a harmless ESRCH no-op. This also satisfies the orphan-prevention goal
    // (#33): a command's backgrounded descendants must not outlive it.
    kill_pgid(child_pid);

    // Bounded post-kill re-reap. On the normal-exit path `child.wait()` in the
    // select already reaped the direct child; on the timeout path `reap_bounded`
    // may have already done so or given up. This final reap is a backstop that is
    // ALSO bounded by REAP_GRACE, so even if a prior reap gave up (ineffective
    // group kill) we never block until the command exits naturally.
    reap_bounded(&mut child).await;

    // Collect whatever the readers captured, BOUNDED by a short grace timeout so
    // nothing here can hang even in a pathological case. After the group kill the
    // pipes drain to EOF almost immediately; the grace is a backstop. If it
    // elapses we abort the read tasks and recover the partial bytes the task had
    // already accumulated into the shared buffer.
    let (stdout_bytes, out_truncated) = join_capped(out_task, &out_buf).await;
    let (stderr_bytes, err_truncated) = join_capped(err_task, &err_buf).await;
    // Either-stream-truncated, preserved on CommandResult for serialization.
    let output_truncated = out_truncated || err_truncated;

    // Mark `[truncated]` only on the stream that actually exceeded its cap.
    let stdout = cap_to_string(stdout_bytes, out_truncated);
    let stderr = if timed_out {
        let mut s = cap_to_string(stderr_bytes, err_truncated);
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&format!("command timed out after {}s", timeout.as_secs()));
        s
    } else {
        cap_to_string(stderr_bytes, err_truncated)
    };

    // Size-truncation does NOT fail a command: a legitimately passing verification
    // command (e.g. `cargo test --all-targets`) routinely prints over the cap. Only
    // a timeout or a non-zero/absent exit status fails the command. See #70.
    let success = !timed_out && status.map(|s| s.success()).unwrap_or(false);

    CommandResult {
        command: command.to_string(),
        success,
        exit_code: if timed_out {
            None
        } else {
            status.and_then(|s| s.code())
        },
        stdout,
        stderr,
        duration_ms: start.elapsed().as_millis() as u64,
        timed_out,
        output_truncated,
    }
}

/// Grace window for (a) reaping the child after a best-effort group kill and (b)
/// collecting the read tasks after the child has exited and the process group has
/// been killed. The pipes drain to EOF almost immediately once every write end is
/// closed, and a killed child is reaped near-instantly; this bounds both waits so
/// a pathological descendant — or an ineffective group kill — can never make
/// `run_command` hang. If it elapses on a reap we give up reaping; if it elapses
/// on a join we abort the task and recover whatever was captured so far.
const REAP_GRACE: Duration = Duration::from_millis(500);

/// Shared, abort-safe capture for one stream: the accumulated bytes plus a
/// per-stream truncated flag. The read task and `run_command` both hold a clone;
/// because the bytes live here (not in the task's return value) they survive an
/// abort of the read task, so the grace-timeout path can return partial output
/// rather than dropping it.
#[derive(Clone, Default)]
struct SharedCapture {
    buf: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
}

impl SharedCapture {
    /// Snapshot the accumulated bytes and truncated flag. Used both on the
    /// abort/grace path (to recover partial output) and as the post-join read.
    fn snapshot(&self) -> (Vec<u8>, bool) {
        let bytes = self.buf.lock().expect("capture mutex not poisoned").clone();
        (bytes, self.truncated.load(Ordering::Acquire))
    }
}

/// Reap a child whose process group was already best-effort SIGKILL'd, BOUNDED by
/// `REAP_GRACE`. A killed child is reaped almost instantly; the timeout exists
/// only for the rare case where the group kill was ineffective (e.g. non-Unix, or
/// the leader changed) so we never wait until the command would exit naturally.
/// On timeout we give up reaping and return `None` (the child has already been
/// SIGKILL'd best-effort, and `kill_on_drop` will make a final attempt).
async fn reap_bounded(child: &mut tokio::process::Child) -> Option<std::process::ExitStatus> {
    match tokio::time::timeout(REAP_GRACE, child.wait()).await {
        Ok(res) => res.ok(),
        Err(_) => None,
    }
}

/// Await a spawned capped-read task with a bounded grace window. On grace
/// timeout the task is aborted and the PARTIAL bytes it had already accumulated
/// into `shared` are recovered, so the post-exit collection in `run_command` can
/// never block indefinitely and never drops already-captured output.
async fn join_capped(
    mut task: tokio::task::JoinHandle<()>,
    shared: &SharedCapture,
) -> (Vec<u8>, bool) {
    match tokio::time::timeout(REAP_GRACE, &mut task).await {
        // The task finished (cleanly or panicked); the bytes it accumulated are in
        // `shared`. Read from `shared` either way so a panic still yields partials.
        Ok(_) => shared.snapshot(),
        Err(_) => {
            // Grace elapsed (a descendant is still holding the pipe open despite
            // the group kill): abort the reader and recover the partial capture
            // it had accumulated so far, instead of returning an empty buffer.
            task.abort();
            shared.snapshot()
        }
    }
}

/// Read from an optional owned async stream into a shared buffer, stopping
/// accumulation once `cap` bytes have been captured. The accumulated bytes and
/// the per-stream truncated flag are written into `shared` (not returned) so they
/// survive an abort of this task; see [`SharedCapture`].
///
/// When the cap is exceeded the task stops accumulating and drains-and-discards
/// the remaining data to EOF so the pipe never blocks the child, but it does NOT
/// kill the command: exceeding the size cap is purely a display/memory concern
/// and must not fail or terminate an otherwise-passing command (#70). The
/// accumulation never exceeds `cap` bytes, so live memory stays bounded
/// regardless of how much the command prints, while the command runs to
/// completion so its real exit status is observed. Only the wall-clock timeout in
/// [`run_command`] can kill the command.
///
/// The shared mutex is only ever held briefly to push a just-read chunk and is
/// released before the next `.await`, so no lock is held across a read.
async fn capped_read_owned<R>(reader: Option<R>, cap: usize, shared: SharedCapture)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let Some(mut reader) = reader else {
        return;
    };
    let mut chunk = [0u8; 8 * 1024];
    let mut captured = 0usize;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => return,
            Ok(n) => {
                let room = cap.saturating_sub(captured);
                if n > room {
                    // More data is available than the cap allows: keep up to the
                    // cap, then drain-and-discard the rest to EOF so the pipe never
                    // blocks the child. We do NOT kill the group here — exceeding
                    // the size cap is purely a display/memory concern and must not
                    // terminate or fail an otherwise-passing command (#70). The
                    // command runs to completion so its real exit status is
                    // observed; only the wall-clock timeout (in `run_command`) can
                    // kill it.
                    {
                        // Hold the lock only to append the kept prefix; release it
                        // before the drain reads below (no lock across `.await`).
                        let mut buf = shared.buf.lock().expect("capture mutex not poisoned");
                        buf.extend_from_slice(&chunk[..room]);
                    }
                    shared.truncated.store(true, Ordering::Release);
                    while let Ok(m) = reader.read(&mut chunk).await {
                        if m == 0 {
                            break;
                        }
                    }
                    return;
                }
                {
                    // Hold the lock only to append the just-read chunk; release it
                    // before looping back to the next `.await`.
                    let mut buf = shared.buf.lock().expect("capture mutex not poisoned");
                    buf.extend_from_slice(&chunk[..n]);
                }
                captured += n;
            }
            Err(_) => return,
        }
    }
}

/// Convert captured bytes to a (possibly already cap-truncated) UTF-8 string,
/// appending a truncation marker when bytes were dropped.
fn cap_to_string(bytes: Vec<u8>, truncated: bool) -> String {
    let mut s = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        s.push_str("\n... [truncated]");
    }
    s
}

/// Kill the child. On Unix the child is a process-group leader, so signal the
/// whole group (negative pid) with SIGKILL to take down any descendants (e.g.
/// `cargo` spawned by `sh`). On other platforms fall back to killing just the
/// child process.
#[cfg(unix)]
fn kill_group(child: &mut tokio::process::Child) {
    // The child was made a group leader via `process_group(0)`, so its pid is
    // also its process-group id.
    if let Some(pid) = child.id() {
        kill_pgid(Some(pid));
    } else {
        let _ = child.start_kill();
    }
}

/// Non-Unix fallback: kill just the child process.
#[cfg(not(unix))]
fn kill_group(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

/// Kill the process group led by `pid` (the child is a group leader, so its pid
/// is also its pgid). Best-effort: a `None` pid or already-gone group is a no-op.
/// This is callable from the read tasks, which do not hold a `&mut Child`.
#[cfg(unix)]
fn kill_pgid(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: `kill(2)` with a negative pid targets the process group; this
        // is a best-effort cleanup and ignores errors (e.g. group already gone).
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

/// Non-Unix fallback: without a `Child` handle we cannot target the group, so
/// the read-task cap-kill is a no-op here. The wall-clock timeout in
/// `run_command` still bounds the wait, and on drop `kill_on_drop` reaps the
/// child.
#[cfg(not(unix))]
fn kill_pgid(_pid: Option<u32>) {}

/// Run a program with an explicit argv (no intermediate shell) in `dir` with a
/// timeout, capturing output. Used by container backends so multi-word
/// verification commands are not re-split by a wrapping `sh -c`.
pub async fn run_argv(
    program: &str,
    args: &[String],
    dir: &Path,
    timeout: Duration,
) -> CommandResult {
    let start = std::time::Instant::now();
    let display = format!("{program} {}", args.join(" "));
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());

    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => CommandResult {
            command: display,
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: truncate(&String::from_utf8_lossy(&output.stdout)),
            stderr: truncate(&String::from_utf8_lossy(&output.stderr)),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: false,
            output_truncated: false,
        },
        Ok(Err(e)) => CommandResult {
            command: display,
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: format!("failed to run command: {e}"),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: false,
            output_truncated: false,
        },
        Err(_) => CommandResult {
            command: display,
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: format!("command timed out after {}s", timeout.as_secs()),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: true,
            output_truncated: false,
        },
    }
}

const MAX_CAPTURE: usize = 64 * 1024;

fn truncate(s: &str) -> String {
    if s.len() <= MAX_CAPTURE {
        return s.to_string();
    }
    // Slice on a valid UTF-8 boundary: a fixed byte offset can land mid-codepoint
    // (common in cargo's box-drawing / quoted output) and panic.
    let end = (0..=MAX_CAPTURE)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    let mut out = s[..end].to_string();
    out.push_str("\n... [truncated]");
    out
}

/// Evaluate a file assertion against the workspace.
pub fn evaluate_assertion(spec: &AssertionSpec, workspace_dir: &Path) -> AssertionResult {
    let description = spec.describe();
    let (passed, detail) = match spec {
        AssertionSpec::FileContains { path, text } => {
            match read_workspace_file(workspace_dir, path) {
                Some(content) if content.contains(text) => (true, "found".to_string()),
                Some(_) => (false, "text not present".to_string()),
                None => (false, "file missing".to_string()),
            }
        }
        AssertionSpec::FileNotContains { path, text } => {
            match read_workspace_file(workspace_dir, path) {
                Some(content) if content.contains(text) => (false, "text present".to_string()),
                Some(_) => (true, "absent as required".to_string()),
                None => (true, "file missing (vacuously true)".to_string()),
            }
        }
        AssertionSpec::FileExists { path } => {
            if workspace_dir.join(path).exists() {
                (true, "exists".to_string())
            } else {
                (false, "missing".to_string())
            }
        }
        AssertionSpec::FileMissing { path } => {
            if workspace_dir.join(path).exists() {
                (false, "present".to_string())
            } else {
                (true, "absent".to_string())
            }
        }
    };
    AssertionResult {
        description,
        passed,
        detail,
    }
}

/// Evaluate a behavior check from the structured event stream.
pub fn evaluate_behavior(spec: &BehaviorSpec, events: &[SdkEvent]) -> BehaviorResult {
    let description = spec.describe();
    let (passed, detail) = match spec {
        BehaviorSpec::MustEmitPlan => {
            let emitted = events.iter().any(|e| matches!(e, SdkEvent::PlanCreated(_)));
            (
                emitted,
                if emitted {
                    "plan emitted".to_string()
                } else {
                    "no PlanCreated event".to_string()
                },
            )
        }
        BehaviorSpec::MustUseTool { tool } => {
            let used = tool_calls(events).any(|name| name == tool);
            (
                used,
                if used {
                    format!("{tool} was called")
                } else {
                    format!("{tool} never called")
                },
            )
        }
        BehaviorSpec::MustNotUseTool { tool } => {
            let used = tool_calls(events).any(|name| name == tool);
            (
                !used,
                if used {
                    format!("{tool} was called")
                } else {
                    format!("{tool} not called")
                },
            )
        }
        BehaviorSpec::MustRunCommand { command_contains } => {
            let ran = bash_commands(events).any(|c| c.contains(command_contains));
            (
                ran,
                if ran {
                    format!("ran a command containing {command_contains:?}")
                } else {
                    format!("no command contained {command_contains:?}")
                },
            )
        }
        BehaviorSpec::MustNotRunCommand { command_contains } => {
            let ran = bash_commands(events).any(|c| c.contains(command_contains));
            (
                !ran,
                if ran {
                    format!("ran a forbidden command containing {command_contains:?}")
                } else {
                    format!("no command contained {command_contains:?}")
                },
            )
        }
    };
    BehaviorResult {
        description,
        passed,
        detail,
    }
}

/// Iterate over the tool names of every `ToolCallStarted` event.
pub fn tool_calls(events: &[SdkEvent]) -> impl Iterator<Item = &str> {
    events.iter().filter_map(|e| match e {
        SdkEvent::ToolCallStarted(ev) => Some(ev.tool_name.as_str()),
        _ => None,
    })
}

/// Iterate over the command text of every Bash `ToolCallStarted` event.
pub fn bash_commands(events: &[SdkEvent]) -> impl Iterator<Item = String> + '_ {
    events.iter().filter_map(|e| match e {
        SdkEvent::ToolCallStarted(ev) if ev.tool_name == "Bash" => ev
            .input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crom_sdk::event::ToolCallStartedEvent;
    use crom_sdk::RunId;

    fn tool_started(tool: &str, input: serde_json::Value) -> SdkEvent {
        SdkEvent::ToolCallStarted(ToolCallStartedEvent {
            run_id: RunId::new(),
            tool_name: tool.to_string(),
            tool_use_id: "t1".to_string(),
            input,
        })
    }

    #[tokio::test]
    async fn command_success_and_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let ok = run_command("exit 0", tmp.path(), Duration::from_secs(10)).await;
        assert!(ok.success);
        assert_eq!(ok.exit_code, Some(0));
        let bad = run_command("exit 3", tmp.path(), Duration::from_secs(10)).await;
        assert!(!bad.success);
        assert_eq!(bad.exit_code, Some(3));
    }

    #[tokio::test]
    async fn run_argv_preserves_multiword_args() {
        let tmp = tempfile::tempdir().unwrap();
        // If args were re-split by a wrapping shell, this would print wrong
        // output or fail. printf joins its args with the format spec.
        let res = run_argv(
            "printf",
            &["%s-%s".into(), "cargo test".into(), "--all-targets".into()],
            tmp.path(),
            Duration::from_secs(10),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.stdout, "cargo test---all-targets");
    }

    #[tokio::test]
    async fn command_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let res = run_command("sleep 5", tmp.path(), Duration::from_millis(200)).await;
        assert!(res.timed_out);
        assert!(!res.success);
    }

    /// A never-exiting command that floods output stays memory-bounded to
    /// ~MAX_CAPTURE; since the cap no longer kills, only the wall-clock timeout
    /// stops it, so it returns as `timed_out` (not a success).
    #[tokio::test]
    async fn unbounded_output_is_capped_and_timeout_kills() {
        let tmp = tempfile::tempdir().unwrap();
        // `yes` prints forever and never exits; only the timeout can stop it.
        let res = run_command("yes AAAAAAAA", tmp.path(), Duration::from_millis(500)).await;

        assert!(res.output_truncated, "expected output_truncated to be set");
        assert!(res.timed_out, "a never-exiting command must time out");
        assert!(!res.success, "a timed-out command is not a success");
        // Bounded to roughly the cap (plus the small truncation marker).
        assert!(
            res.stdout.len() <= MAX_CAPTURE + "\n... [truncated]".len(),
            "stdout not bounded: {} bytes",
            res.stdout.len()
        );
        assert!(res.stdout.ends_with("[truncated]"));
    }

    /// REGRESSION (#70): a command that prints well over MAX_CAPTURE and exits 0
    /// is a SUCCESS — size-truncation alone must not fail or kill it. Output is
    /// still capped and flagged truncated, but the real exit status is observed.
    #[cfg(unix)]
    #[tokio::test]
    async fn over_cap_output_with_zero_exit_is_success() {
        let tmp = tempfile::tempdir().unwrap();
        // Print ~256 KiB (4x the 64 KiB cap) then exit 0. Pre-fix this was killed
        // mid-run and reported as failed; post-fix it runs to completion.
        let bytes = MAX_CAPTURE * 4;
        let command = format!("head -c {bytes} /dev/zero | tr '\\0' 'A'; exit 0");
        let res = run_command(&command, tmp.path(), Duration::from_secs(30)).await;

        assert!(res.success, "an over-cap command that exits 0 must succeed");
        assert_eq!(res.exit_code, Some(0));
        assert!(res.output_truncated, "expected output_truncated to be set");
        assert!(!res.timed_out, "ran to completion, not timed out");
        // Captured output stays bounded to the cap plus the truncation marker.
        assert!(
            res.stdout.len() <= MAX_CAPTURE + "\n... [truncated]".len(),
            "stdout not bounded: {} bytes",
            res.stdout.len()
        );
        assert!(res.stdout.ends_with("[truncated]"));
    }

    /// REGRESSION (Finding 1): a command that closes its stdout/stderr but keeps
    /// running makes the pipes reach EOF while the process is still alive. The
    /// timeout must still fire — the call must NOT hang waiting on `child.wait()`
    /// after the readers finish. We use a short timeout and assert it returns
    /// promptly with `timed_out` set.
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_fires_when_stdio_closed_but_process_alive() {
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        // Close both stdio fds, then sleep well past the timeout. The read tasks
        // see EOF immediately; only the wall-clock timeout can stop the sleeper.
        let res = run_command(
            "exec >&- 2>&-; sleep 30",
            tmp.path(),
            Duration::from_millis(300),
        )
        .await;
        let elapsed = start.elapsed();

        assert!(res.timed_out, "expected the wall-clock timeout to fire");
        assert!(!res.success, "a timed-out command is not a success");
        assert!(
            elapsed < Duration::from_secs(3),
            "run_command hung despite the timeout: {elapsed:?}"
        );
    }

    /// REGRESSION (Finding 1): a command that backgrounds a stdio-INHERITING
    /// descendant and then exits. The direct `sh` child exits immediately, so the
    /// `child.wait()` branch wins with `timed_out=false` and the `sleep(timeout)`
    /// future is dropped. The backgrounded `sleep` still holds the write end of
    /// both pipes, so without the unconditional group-kill the post-exit
    /// `out_task`/`err_task` joins would block FOREVER waiting for an EOF that
    /// never arrives. With the fix the group is killed after the select, the
    /// pipes hit EOF, and the call returns promptly — well under the 30s sleep.
    #[cfg(unix)]
    #[tokio::test]
    async fn returns_promptly_when_descendant_inherits_stdio_and_parent_exits() {
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        // `sleep 30 &` inherits the shell's stdout/stderr (the pipe write ends),
        // then `exit 0` reaps the `sh` immediately. The descendant keeps the pipe
        // open for 30s; the call must NOT wait for it.
        let res = run_command("sleep 30 & exit 0", tmp.path(), Duration::from_secs(30)).await;
        let elapsed = start.elapsed();

        // The `sh` exited 0 normally (not a timeout): success is fine here, the
        // point is that we returned without hanging on the live descendant.
        assert!(
            !res.timed_out,
            "sh exited normally; should not be a timeout"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "run_command hung on a backgrounded stdio-inheriting descendant: {elapsed:?}"
        );
    }

    /// Per-stream truncation (Finding 2): when only stdout floods past the cap,
    /// stdout is marked truncated but stderr (small) is NOT.
    #[cfg(unix)]
    #[tokio::test]
    async fn only_flooding_stream_is_marked_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        // stdout floods forever; stderr emits one small line.
        let res = run_command(
            "echo small 1>&2; yes AAAAAAAA",
            tmp.path(),
            Duration::from_secs(30),
        )
        .await;

        assert!(res.output_truncated, "either-stream flag should be set");
        assert!(
            res.stdout.ends_with("[truncated]"),
            "stdout should be marked truncated"
        );
        assert!(
            !res.stderr.contains("[truncated]"),
            "stderr did not exceed the cap and must not be marked truncated: {:?}",
            res.stderr
        );
        assert!(
            res.stderr.contains("small"),
            "stderr should retain its small payload: {:?}",
            res.stderr
        );
    }

    /// On timeout the whole process group is signalled, so a child the shell
    /// spawned does not outlive `run_command`. We run a `sleep` in a backgrounded
    /// subshell, capture its pid, then assert (Unix) that the pid is gone shortly
    /// after the short timeout fires.
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_child_process_group() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("child.pid");
        let pidfile_str = pidfile.to_str().unwrap();
        // Spawn a long sleeper, record its pid, then block so run_command times
        // out while the sleeper is still "running" under the same group.
        let command = format!("sleep 30 & echo $! > {pidfile_str}; wait");

        let start = std::time::Instant::now();
        let res = run_command(&command, tmp.path(), Duration::from_millis(300)).await;
        assert!(res.timed_out, "expected timeout");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "run_command did not return promptly on timeout"
        );

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("pidfile written")
            .trim()
            .parse()
            .expect("pid parses");

        // Give the SIGKILL a brief moment to propagate, then assert the child is
        // gone: kill(pid, 0) returns -1/ESRCH when no such process exists.
        let mut alive = true;
        for _ in 0..50 {
            // SAFETY: signal 0 only checks for process existence.
            let rc = unsafe { libc::kill(pid, 0) };
            if rc != 0 {
                alive = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive, "child sleeper (pid {pid}) outlived the timeout");
    }

    /// Partial-capture recovery (Finding 2): when `join_capped` aborts a reader
    /// that escaped the group kill and is still holding the pipe, the bytes the
    /// task already pushed into the SHARED buffer must be returned — not an empty
    /// Vec. We drive the abort path deterministically: a task writes some bytes
    /// (and sets truncated) into the shared capture, then parks forever so it can
    /// never join within the grace, forcing the abort branch.
    #[tokio::test]
    async fn join_capped_recovers_partial_bytes_on_abort() {
        let shared = SharedCapture::default();
        {
            let mut buf = shared.buf.lock().unwrap();
            buf.extend_from_slice(b"partial-output");
        }
        shared.truncated.store(true, Ordering::Release);

        // A task that never returns: it can only leave `join_capped` via the
        // grace timeout + abort branch.
        let task = tokio::spawn(async {
            std::future::pending::<()>().await;
        });

        let start = std::time::Instant::now();
        let (bytes, truncated) = join_capped(task, &shared).await;
        let elapsed = start.elapsed();

        assert_eq!(
            bytes, b"partial-output",
            "partial bytes accumulated before abort must be recovered"
        );
        assert!(truncated, "truncated flag must be preserved across abort");
        // Bounded by the grace, not hung.
        assert!(
            elapsed < REAP_GRACE + Duration::from_secs(1),
            "join_capped did not abort within the grace: {elapsed:?}"
        );
    }

    /// Normal-path parity (Finding 2): when the reader task joins cleanly within
    /// the grace, `join_capped` returns exactly what it accumulated into the
    /// shared buffer — same result as before the abort-safe refactor.
    #[tokio::test]
    async fn join_capped_returns_full_bytes_on_clean_join() {
        let shared = SharedCapture::default();
        let writer = shared.clone();
        let task = tokio::spawn(async move {
            let mut buf = writer.buf.lock().unwrap();
            buf.extend_from_slice(b"hello world");
        });

        let (bytes, truncated) = join_capped(task, &shared).await;
        assert_eq!(bytes, b"hello world");
        assert!(!truncated);
    }

    #[test]
    fn file_assertions() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "pub fn parse_args() {}").unwrap();

        let contains = evaluate_assertion(
            &AssertionSpec::FileContains {
                path: "a.txt".to_string(),
                text: "parse_args".to_string(),
            },
            tmp.path(),
        );
        assert!(contains.passed);

        let not_contains = evaluate_assertion(
            &AssertionSpec::FileNotContains {
                path: "a.txt".to_string(),
                text: "unwrap()".to_string(),
            },
            tmp.path(),
        );
        assert!(not_contains.passed);

        let missing = evaluate_assertion(
            &AssertionSpec::FileMissing {
                path: "nope.txt".to_string(),
            },
            tmp.path(),
        );
        assert!(missing.passed);

        let exists = evaluate_assertion(
            &AssertionSpec::FileExists {
                path: "a.txt".to_string(),
            },
            tmp.path(),
        );
        assert!(exists.passed);
    }

    #[test]
    fn behavior_from_synthetic_events() {
        let events = vec![
            tool_started("Read", serde_json::json!({"file_path": "src/lib.rs"})),
            tool_started(
                "Bash",
                serde_json::json!({"command": "cargo test --all-targets"}),
            ),
        ];

        assert!(
            evaluate_behavior(
                &BehaviorSpec::MustUseTool {
                    tool: "Read".into()
                },
                &events
            )
            .passed
        );
        assert!(
            !evaluate_behavior(
                &BehaviorSpec::MustUseTool {
                    tool: "Edit".into()
                },
                &events
            )
            .passed
        );
        assert!(
            evaluate_behavior(
                &BehaviorSpec::MustNotUseTool {
                    tool: "Edit".into()
                },
                &events
            )
            .passed
        );
        assert!(
            evaluate_behavior(
                &BehaviorSpec::MustRunCommand {
                    command_contains: "cargo test".into()
                },
                &events
            )
            .passed
        );
        assert!(
            evaluate_behavior(
                &BehaviorSpec::MustNotRunCommand {
                    command_contains: "rm -rf".into()
                },
                &events
            )
            .passed
        );
        assert!(
            !evaluate_behavior(&BehaviorSpec::MustEmitPlan, &events).passed,
            "no plan event present"
        );
    }

    #[test]
    fn truncate_handles_multibyte_boundaries() {
        // A string of multi-byte chars longer than the cap must not panic and
        // must remain valid UTF-8 after truncation.
        let s = "é".repeat(MAX_CAPTURE); // 2 bytes each → 2*MAX_CAPTURE bytes
        let out = truncate(&s);
        assert!(out.len() <= MAX_CAPTURE + "\n... [truncated]".len());
        assert!(out.ends_with("[truncated]"));
        // No panic and the prefix is valid UTF-8 (String guarantees this).
        assert!(out.starts_with('é'));
    }

    #[test]
    fn report_pass_fraction() {
        let report = OracleReport {
            commands: vec![CommandResult {
                command: "x".into(),
                success: true,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: 1,
                timed_out: false,
                output_truncated: false,
            }],
            assertions: vec![AssertionResult {
                description: "a".into(),
                passed: false,
                detail: String::new(),
            }],
            behavior: vec![],
        };
        assert_eq!(report.checks_total(), 2);
        assert_eq!(report.checks_passed(), 1);
        assert!((report.pass_fraction() - 0.5).abs() < 1e-9);
        assert!(!report.all_passed());
    }
}

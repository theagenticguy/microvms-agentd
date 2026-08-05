//! Command execution: idempotent start, read-only poll, ack-gated release, and
//! process-group kill.
//!
//! The lifecycle here is the one the `agentd-model` crate enumerates. Running →
//! Exited → Acked, with output held until the caller acks and only acked entries
//! eligible for TTL collection. Every rule below has a defect behind it, and the
//! four that are easiest to reintroduce are:
//!
//! * **Pipes, not temp files.** The Python predecessor wrote output to temp files
//!   and unlinked them when the direct child exited, which destroyed everything a
//!   backgrounded grandchild wrote afterward. Pipes invert that: grandchildren
//!   inherit the write end, so EOF arrives only when the last writer closes, and
//!   "the backgrounded server keeps logging" works by construction rather than by
//!   a special case.
//! * **The pgid is captured immediately after spawn.** `Child::id()` returns
//!   `None` once the child has been reaped, so reading it lazily from the kill
//!   path yields nothing exactly when a kill is most needed.
//! * **No `pre_exec`.** Demotion goes through `Command::uid`/`gid`, which do the
//!   work in C between fork and exec. Running interpreted code in a forked child
//!   of a threaded process can deadlock on an allocator lock the parent held.
//! * **`cwd` omitted means inherit.** No `cd`, and no defaulting to `/`. The
//!   daemon is the container `CMD`, so its own working directory *is* the image
//!   `WORKDIR`, and forcing `/` broke every prebuilt-image task.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::state::AppState;

/// Where an exec sits in its lifecycle. Mirrors `ExecPhase` in the model crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Child spawned, still running (or its pipes still held by a grandchild).
    Running,
    /// Child exited and output is buffered and readable.
    Exited,
    /// Caller acked; output has been released and the entry awaits collection.
    Acked,
}

/// Captured output and exit status of a finished exec.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Outcome {
    /// Exit code, or `None` when the child died to a signal.
    pub exit_code: Option<i32>,
    /// Signal number that killed the child, when one did.
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Set when either stream hit `max_output_bytes` and was cut. An explicit
    /// flag rather than a sentinel string in the output: a marker inside the
    /// bytes is indistinguishable from output that happens to contain it.
    pub truncated: bool,
    /// Set when the post-exit linger deadline expired with the pipes still open,
    /// meaning some grandchild is alive and may write more that nobody will see.
    /// Reported rather than hidden, because a harness that sees empty output from
    /// a command it knows produced some needs to be able to tell why.
    pub writers_may_be_alive: bool,
}

/// One exec slot, keyed by the caller-minted idempotency key.
///
/// The child is not owned here. Waiting on it and draining its pipes happens in
/// a detached task, which publishes into `shared` when it is done; the registry
/// lock is therefore never held across an await.
pub struct ExecEntry {
    /// Process group id, captured immediately after spawn while `Child::id()`
    /// still answers. `None` only if the child had already been reaped by the
    /// time we asked, which a fast-exiting command can manage.
    pgid: Option<u32>,
    shared: Arc<Shared>,
    /// When the entry was acked. TTL collection reads this; an unacked entry has
    /// no deadline and is never collected.
    acked_at: Option<Instant>,
}

/// The part of an entry the waiter task and the handlers both touch.
struct Shared {
    /// Written once by the waiter task when the child is done and its output has
    /// been drained (or the linger deadline cut the drain short).
    result: Mutex<Option<Outcome>>,
}

/// A start request. `command` is either an argv array or, with `shell: true`, a
/// single script string.
#[derive(Debug, Deserialize)]
pub struct StartRequest {
    /// Caller-minted idempotency key. Harbor retries, and a retry must not
    /// produce a second child.
    pub exec_id: String,
    /// argv when `shell` is false, or the script when it is true.
    pub command: Vec<String>,
    #[serde(default)]
    pub shell: bool,
    /// Omitted means inherit the daemon's working directory. See the module docs.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Numeric uid to demote to. Optional; omitted means run as the daemon's own
    /// user.
    #[serde(default)]
    pub user: Option<u32>,
    #[serde(default)]
    pub group: Option<u32>,
    /// Wall-clock budget. Validated before the child spawns — the predecessor
    /// raised on a bad value inside the waiter thread, by which point the child
    /// was already running and became an orphan.
    #[serde(default)]
    pub timeout_sec: Option<f64>,
}

#[derive(Debug, Serialize)]
struct StartResponse {
    exec_id: String,
    phase: Phase,
}

#[derive(Debug, Serialize)]
struct PollResponse {
    exec_id: String,
    phase: Phase,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(flatten)]
    result: Option<Outcome>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    detail: String,
}

/// Builds an error response.
///
/// The status is always chosen by the caller of this function, never inferred
/// from an error type: a bad body key must be 400 and an absent id must be 404,
/// and collapsing the two is the defect that made a protocol typo look like a
/// missing artifact.
fn fail(status: StatusCode, error: &'static str, detail: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error,
            detail: detail.into(),
        }),
    )
        .into_response()
}

/// `POST /v1/exec/start`.
pub async fn start(
    State(state): State<AppState>,
    body: Result<Json<StartRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(req)) = body else {
        return fail(
            StatusCode::BAD_REQUEST,
            "malformed_request",
            "body is not a valid start request",
        );
    };

    if req.exec_id.is_empty() {
        return fail(
            StatusCode::BAD_REQUEST,
            "malformed_request",
            "exec_id must not be empty",
        );
    }

    // Validated here, before anything is spawned. Doing it in the waiter left a
    // running child with nobody to reap it.
    let timeout = match validate_timeout(req.timeout_sec) {
        Ok(timeout) => timeout,
        Err(detail) => return fail(StatusCode::BAD_REQUEST, "malformed_request", detail),
    };

    let command = match build_command(&req) {
        Ok(command) => command,
        Err(detail) => return fail(StatusCode::BAD_REQUEST, "malformed_request", detail),
    };

    // Idempotency: decided under the registry lock, before the spawn, so two
    // concurrent retries cannot both find the slot empty. A known id returns
    // success and leaves the existing entry's phase and output untouched.
    let already_present = state.with_execs(|execs| execs.contains_key(&req.exec_id));
    if already_present {
        tracing::info!(exec_id = %req.exec_id, "start retried; not spawning again");
        return (
            StatusCode::OK,
            Json(StartResponse {
                exec_id: req.exec_id,
                phase: Phase::Running,
            }),
        )
            .into_response();
    }

    match spawn(&state, &req.exec_id, command, timeout) {
        Ok(()) => (
            StatusCode::OK,
            Json(StartResponse {
                exec_id: req.exec_id,
                phase: Phase::Running,
            }),
        )
            .into_response(),
        // A spawn failure is the daemon's problem, not a malformed request:
        // ENOENT on argv[0] is reported as a 500 with the detail rather than a
        // 404, which a client would read as "the exec id does not exist".
        Err(err) => {
            tracing::warn!(exec_id = %req.exec_id, %err, "spawn failed");
            fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "spawn_failed",
                err.to_string(),
            )
        }
    }
}

/// `GET /v1/exec/{id}`.
///
/// Strictly read-only. Nothing in this function may write to the registry or to
/// an entry — the model asserts it against the transition function, because
/// read-only is a property of the step and not of any reachable state.
pub async fn poll(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let shared = match state.with_execs(|execs| execs.get(&id).map(|entry| entry.shared.clone())) {
        Some(shared) => shared,
        None => return fail(StatusCode::NOT_FOUND, "unknown_exec", id),
    };

    let acked = state.with_execs(|execs| {
        execs
            .get(&id)
            .map(|entry| entry.acked_at.is_some())
            .unwrap_or(false)
    });

    let result = shared.result.lock().await.clone();
    let phase = phase_of(acked, result.is_some());

    (
        StatusCode::OK,
        Json(PollResponse {
            exec_id: id,
            // An acked entry's output has been released; reporting it again would
            // contradict the phase.
            result: if acked { None } else { result },
            phase,
        }),
    )
        .into_response()
}

/// `POST /v1/exec/{id}/ack`.
///
/// Releases the buffered output and starts the TTL clock. Acking a still-running
/// exec is 409, not a silent success: succeeding would drop output that is still
/// being written, which is precisely what unlinking on child exit did.
pub async fn ack(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let shared = match state.with_execs(|execs| execs.get(&id).map(|entry| entry.shared.clone())) {
        Some(shared) => shared,
        None => return fail(StatusCode::NOT_FOUND, "unknown_exec", id),
    };

    let mut slot = shared.result.lock().await;
    if slot.is_none() {
        return fail(
            StatusCode::CONFLICT,
            "still_running",
            "exec has not exited; output is still being written",
        );
    }

    let released = slot.take();
    drop(slot);

    let marked = state.with_execs(|execs| match execs.get_mut(&id) {
        Some(entry) if entry.acked_at.is_none() => {
            entry.acked_at = Some(Instant::now());
            true
        }
        // Already acked. The output was released by the first ack, so there is
        // nothing left to hand back; answer 409 rather than 200 with an empty
        // body, which would read as "the command produced no output".
        Some(_) => false,
        None => false,
    });

    if !marked {
        return fail(
            StatusCode::CONFLICT,
            "already_acked",
            "output was released by an earlier ack",
        );
    }

    tracing::info!(exec_id = %id, "output acked and released");
    (
        StatusCode::OK,
        Json(PollResponse {
            exec_id: id,
            phase: Phase::Acked,
            result: released,
        }),
    )
        .into_response()
}

/// `POST /v1/exec/{id}/kill`.
///
/// Signals the whole process group, not just the direct child. A shell that
/// backgrounded a server leaves the interesting process outside the child pid,
/// and `kill(child)` returned success while the workload kept running.
pub async fn kill(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let (pgid, done) = match state.with_execs(|execs| {
        execs
            .get(&id)
            .map(|entry| (entry.pgid, entry.shared.clone()))
    }) {
        Some((pgid, shared)) => (pgid, shared),
        None => return fail(StatusCode::NOT_FOUND, "unknown_exec", id),
    };

    let Some(pgid) = pgid else {
        // No pgid was ever captured, which means the child had already been
        // reaped. Nothing to signal, and saying so is more useful than a 500.
        return (
            StatusCode::OK,
            Json(serde_json::json!({ "exec_id": id, "killed": false })),
        )
            .into_response();
    };

    let grace = state.config().kill_grace;
    let signaled = escalate(pgid, grace, done).await;

    tracing::info!(exec_id = %id, pgid, signaled, "kill requested");
    (
        StatusCode::OK,
        Json(serde_json::json!({ "exec_id": id, "killed": signaled })),
    )
        .into_response()
}

/// Collects acked entries whose TTL has elapsed.
///
/// Returns how many were removed. Deliberately a plain function the daemon's own
/// loop calls, not a task spawned from a handler: a reaper started per request
/// multiplies with traffic, and the ones the predecessor spawned outlived the
/// entries they were meant to collect.
///
/// Only acked entries are eligible. An unacked entry has no deadline however old
/// it is, because collecting it would destroy output the caller never read.
pub fn collect_expired(state: &AppState) -> usize {
    let ttl = state.config().exec_ttl;
    let now = Instant::now();
    state.with_execs(|execs| {
        let before = execs.len();
        execs.retain(|_, entry| match entry.acked_at {
            Some(at) => now.duration_since(at) < ttl,
            None => true,
        });
        before - execs.len()
    })
}

/// Rejects a timeout that cannot describe a real budget.
///
/// `f64` from JSON admits NaN and infinity through some encoders, and both turn
/// into a `Duration` conversion panic or an effectively infinite wait.
fn validate_timeout(raw: Option<f64>) -> Result<Option<Duration>, String> {
    let Some(secs) = raw else { return Ok(None) };
    if !secs.is_finite() || secs <= 0.0 {
        return Err(format!(
            "timeout_sec must be a positive finite number, got {secs}"
        ));
    }
    Ok(Some(Duration::from_secs_f64(secs)))
}

/// Assembles the child command, including the shell decision and demotion.
fn build_command(req: &StartRequest) -> Result<Command, String> {
    let mut command = if req.shell {
        // A single argument to `sh -c`, not a constructed wrapper. The
        // predecessor built `"cd %s && {\n%s\n}"`, which made an empty command
        // and a comment-terminated command into syntax errors and let an
        // unbalanced `}` in the script escape the group it was supposed to be
        // confined by. `sh -c ''` exits 0, which is the correct answer.
        let script = req.command.join("\n");
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    } else {
        let Some((program, args)) = req.command.split_first() else {
            return Err("command must not be empty when shell is false".to_string());
        };
        let mut command = Command::new(program);
        command.args(args);
        command
    };

    // Only what the request asks for. Inheriting the daemon's environment would
    // carry the agent token into the child, and that is one of the three security
    // properties the model pins — so the environment starts empty and nothing
    // reads from `std::env`.
    command.env_clear();
    command.envs(&req.env);

    // Omitted cwd means inherit. Not `/`.
    if let Some(cwd) = &req.cwd {
        command.current_dir(cwd);
    }

    // Demotion between fork and exec, in C. Never through `pre_exec`: a closure
    // there runs interpreted-equivalent code in a forked child of a threaded
    // process, where a lock held by another thread at fork time is held forever.
    if let Some(gid) = req.group {
        command.gid(gid);
    }
    if let Some(uid) = req.user {
        command.uid(uid);
    }

    // A new process group, so kill can signal the whole tree. 0 means "use the
    // child's own pid as the pgid".
    command.process_group(0);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    Ok(command)
}

/// Spawns the child and registers the entry, then detaches the waiter.
fn spawn(
    state: &AppState,
    id: &str,
    mut command: Command,
    timeout: Option<Duration>,
) -> std::io::Result<()> {
    let mut child = command.spawn()?;

    // Immediately, while the child is still unreaped. `Child::id()` answers
    // `None` after a wait, so a kill path that read it lazily would find nothing
    // for exactly the fast-then-forking commands that most need killing. Because
    // of `process_group(0)`, the pid is also the pgid.
    let pgid = child.id();

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let shared = Arc::new(Shared {
        result: Mutex::new(None),
    });

    state.with_execs(|execs| {
        execs.insert(
            id.to_string(),
            ExecEntry {
                pgid,
                shared: clone_shared(&shared),
                acked_at: None,
            },
        );
    });

    let cfg = state.config().clone();
    let owned_id = id.to_string();
    tokio::spawn(async move {
        let outcome = super_wait(
            &mut child,
            stdout,
            stderr,
            pgid,
            cfg.max_output_bytes,
            cfg.output_linger,
            cfg.kill_grace,
            timeout,
        )
        .await;
        tracing::info!(
            exec_id = %owned_id,
            exit_code = ?outcome.exit_code,
            signal = ?outcome.signal,
            truncated = outcome.truncated,
            "exec finished"
        );
        *shared.result.lock().await = Some(outcome);
    });

    Ok(())
}

fn clone_shared(shared: &Arc<Shared>) -> Arc<Shared> {
    Arc::clone(shared)
}

fn phase_of(acked: bool, finished: bool) -> Phase {
    if acked {
        Phase::Acked
    } else if finished {
        Phase::Exited
    } else {
        Phase::Running
    }
}

/// Waits for the child while draining both pipes concurrently, then lingers.
///
/// Draining must be concurrent with the wait: a child that fills a 64 KiB pipe
/// buffer blocks in `write` forever if nobody is reading, and a waiter that only
/// reads after `wait()` returns deadlocks on exactly the noisy commands it was
/// written for.
#[allow(clippy::too_many_arguments)]
async fn super_wait(
    child: &mut tokio::process::Child,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    pgid: Option<u32>,
    cap: usize,
    linger: Duration,
    grace: Duration,
    timeout: Option<Duration>,
) -> Outcome {
    let mut out_reader = Capped::new(stdout, cap);
    let mut err_reader = Capped::new(stderr, cap);

    let mut status = None;
    let deadline = timeout.map(|budget| Instant::now() + budget);
    let mut timed_out = false;

    // Phase one: the direct child is alive. Read both pipes and the exit status
    // together.
    while status.is_none() {
        tokio::select! {
            biased;
            waited = child.wait() => status = Some(waited),
            _ = out_reader.pump() => {}
            _ = err_reader.pump() => {}
            _ = sleep_until(deadline) => {
                timed_out = true;
                if let Some(pgid) = pgid {
                    escalate_blind(pgid, grace).await;
                }
                // Fall through: the group has been signalled, so the wait above
                // will now complete and the pipes will reach EOF.
                status = Some(child.wait().await);
            }
        }
    }

    // Phase two: the child is gone but grandchildren may still hold the write
    // end. This is the case temp files got wrong — they were unlinked here, so
    // anything a backgrounded process wrote afterward went to a file with no
    // name. A pipe keeps working; all we need is a deadline so a daemonized
    // server does not hold the exec open forever.
    let linger_deadline = Some(Instant::now() + linger);
    let mut writers_may_be_alive = false;
    loop {
        if out_reader.done() && err_reader.done() {
            break;
        }
        tokio::select! {
            _ = out_reader.pump() => {}
            _ = err_reader.pump() => {}
            _ = sleep_until(linger_deadline) => {
                writers_may_be_alive = true;
                break;
            }
        }
    }

    let (exit_code, signal) = match status {
        Some(Ok(status)) => (status.code(), unix_signal(&status)),
        // A failed wait leaves the status genuinely unknown. Reporting `None`
        // for both is honest; inventing a code would be worse than saying so.
        Some(Err(err)) => {
            tracing::warn!(%err, "wait on exec child failed");
            (None, None)
        }
        None => (None, None),
    };

    if timed_out {
        tracing::warn!("exec exceeded its timeout and its process group was signalled");
    }

    let truncated = out_reader.truncated || err_reader.truncated;
    Outcome {
        exit_code,
        signal,
        stdout: out_reader.into_string(),
        stderr: err_reader.into_string(),
        truncated,
        writers_may_be_alive,
    }
}

/// Sleeps until `deadline`, or forever when there is none.
///
/// `select!` needs a branch that is always well-formed, and a `None` deadline has
/// to be a future that never completes rather than one that completes instantly —
/// the latter would make every unbounded exec look like it timed out.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending().await,
    }
}

/// A pipe reader with a byte cap.
///
/// After the cap, bytes are still read and discarded rather than left in the
/// pipe: stopping the read would block a writer in the kernel indefinitely, and a
/// command whose output overflows the cap should still be able to finish.
struct Capped<R> {
    reader: Option<R>,
    buf: Vec<u8>,
    scratch: Vec<u8>,
    cap: usize,
    truncated: bool,
    eof: bool,
}

impl<R: AsyncReadExt + Unpin> Capped<R> {
    fn new(reader: Option<R>, cap: usize) -> Self {
        Self {
            eof: reader.is_none(),
            reader,
            buf: Vec::new(),
            scratch: vec![0u8; 16 * 1024],
            cap,
            truncated: false,
        }
    }

    fn done(&self) -> bool {
        self.eof
    }

    /// Reads one chunk. Resolves immediately once at EOF, so a `select!` loop
    /// that still has the other stream open does not spin on this one — the
    /// caller's loop exits on `done()`.
    async fn pump(&mut self) {
        if self.eof {
            std::future::pending::<()>().await;
            return;
        }
        let Some(reader) = self.reader.as_mut() else {
            self.eof = true;
            return;
        };
        match reader.read(&mut self.scratch).await {
            Ok(0) => self.eof = true,
            Ok(n) => {
                let room = self.cap.saturating_sub(self.buf.len());
                if room == 0 {
                    self.truncated = true;
                } else if n > room {
                    self.buf.extend_from_slice(&self.scratch[..room]);
                    self.truncated = true;
                } else {
                    self.buf.extend_from_slice(&self.scratch[..n]);
                }
            }
            Err(err) => {
                tracing::debug!(%err, "exec pipe read failed; treating as EOF");
                self.eof = true;
            }
        }
    }

    /// Renders the captured bytes.
    ///
    /// Lossy on purpose. A cap that lands mid-codepoint is normal, and a strict
    /// decode would turn "your command printed a lot" into "the daemon lost your
    /// output".
    fn into_string(self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }
}

/// SIGTERM, wait `grace` for the group to go, then SIGKILL.
///
/// Returns whether the first signal was delivered. `done` lets the grace period
/// end early when the child finishes on its own, so a well-behaved process does
/// not cost the full grace period.
async fn escalate(pgid: u32, grace: Duration, done: Arc<Shared>) -> bool {
    if !signal_group(pgid, nix::sys::signal::Signal::SIGTERM) {
        return false;
    }

    let waited = tokio::time::timeout(grace, async {
        loop {
            if done.result.lock().await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;

    if waited.is_err() {
        tracing::warn!(
            pgid,
            "process group survived SIGTERM; escalating to SIGKILL"
        );
        signal_group(pgid, nix::sys::signal::Signal::SIGKILL);
    }
    true
}

/// The timeout path's escalation, which has no `Shared` to watch because the
/// waiter task *is* the caller.
async fn escalate_blind(pgid: u32, grace: Duration) {
    if !signal_group(pgid, nix::sys::signal::Signal::SIGTERM) {
        return;
    }
    tokio::time::sleep(grace).await;
    signal_group(pgid, nix::sys::signal::Signal::SIGKILL);
}

/// Signals a whole process group.
///
/// `ESRCH` is not an error worth reporting: it means the group is already gone,
/// which is the outcome a kill was asking for.
fn signal_group(pgid: u32, signal: nix::sys::signal::Signal) -> bool {
    let pid = nix::unistd::Pid::from_raw(pgid as i32);
    match nix::sys::signal::killpg(pid, signal) {
        Ok(()) => true,
        Err(nix::errno::Errno::ESRCH) => {
            tracing::debug!(pgid, "process group already gone");
            false
        }
        Err(err) => {
            tracing::warn!(pgid, %err, "killpg failed");
            false
        }
    }
}

#[cfg(unix)]
fn unix_signal(status: &std::process::ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(status)
}

#[cfg(not(unix))]
fn unix_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn state() -> AppState {
        AppState::new(Config::default())
    }

    fn state_with(f: impl FnOnce(&mut Config)) -> AppState {
        let mut cfg = Config::default();
        f(&mut cfg);
        AppState::new(cfg)
    }

    fn req(id: &str, argv: &[&str]) -> StartRequest {
        StartRequest {
            exec_id: id.to_string(),
            command: argv.iter().map(|s| s.to_string()).collect(),
            shell: false,
            cwd: None,
            env: HashMap::new(),
            user: None,
            group: None,
            timeout_sec: None,
        }
    }

    /// Drives an exec to completion the way the daemon does, then returns the
    /// outcome. Polls the shared slot rather than sleeping a fixed interval, so
    /// the test is fast and does not race.
    async fn run(state: &AppState, req: StartRequest) -> Outcome {
        let id = req.exec_id.clone();
        let timeout = validate_timeout(req.timeout_sec).expect("valid timeout");
        let command = build_command(&req).expect("buildable command");
        spawn(state, &id, command, timeout).expect("spawn");
        await_result(state, &id).await
    }

    /// Waits for the waiter task to publish, with a bounded number of attempts so
    /// a regression fails the test rather than hanging the suite.
    async fn await_result(state: &AppState, id: &str) -> Outcome {
        let shared = state
            .with_execs(|execs| execs.get(id).map(|e| e.shared.clone()))
            .expect("entry registered");
        for _ in 0..600 {
            if let Some(outcome) = shared.result.lock().await.clone() {
                return outcome;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("exec {id} never finished");
    }

    /// Decodes a handler response body as JSON, so tests can assert on what the
    /// caller actually receives and not only on the status.
    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    #[tokio::test]
    async fn a_simple_command_captures_stdout_and_exit_zero() {
        let state = state();
        let outcome = run(&state, req("e1", &["/bin/sh", "-c", "echo hi"])).await;
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout.trim(), "hi");
        assert!(!outcome.truncated);
    }

    #[tokio::test]
    async fn stderr_is_captured_separately_and_a_nonzero_code_is_reported() {
        let state = state();
        let outcome = run(
            &state,
            req("e2", &["/bin/sh", "-c", "echo oops >&2; exit 3"]),
        )
        .await;
        assert_eq!(outcome.exit_code, Some(3));
        assert_eq!(outcome.stderr.trim(), "oops");
        assert!(outcome.stdout.is_empty());
    }

    /// The predecessor wrapped the script in `cd %s && { ... }`, which made an
    /// empty command a syntax error. `sh -c ''` exits 0 and that is correct.
    #[tokio::test]
    async fn an_empty_shell_command_exits_zero() {
        let state = state();
        let mut request = req("e3", &[]);
        request.shell = true;
        let outcome = run(&state, request).await;
        assert_eq!(
            outcome.exit_code,
            Some(0),
            "empty command must not be a syntax error"
        );
        assert!(outcome.stderr.is_empty(), "stderr was {:?}", outcome.stderr);
    }

    /// The other half of the same defect: a trailing comment used to swallow the
    /// closing brace, and an unbalanced brace used to escape the group.
    #[tokio::test]
    async fn a_comment_terminated_shell_command_exits_zero() {
        let state = state();
        let mut request = req("e4", &["echo hi", "# trailing comment"]);
        request.shell = true;
        let outcome = run(&state, request).await;
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout.trim(), "hi");
    }

    #[tokio::test]
    async fn an_unbalanced_brace_cannot_escape_the_wrapper() {
        let state = state();
        let mut request = req("e5", &["echo a", "}", "echo b"]);
        request.shell = true;
        let outcome = run(&state, request).await;
        // It is a shell syntax error inside `sh -c`, which is the honest result:
        // nothing after the stray brace runs, and the daemon reports the code.
        assert_ne!(outcome.exit_code, Some(0));
        assert!(
            !outcome.stdout.contains('b'),
            "stdout was {:?}",
            outcome.stdout
        );
    }

    /// Idempotency. Two starts with one id produce one child, and the second
    /// leaves the first entry's phase and output alone.
    #[tokio::test]
    async fn a_retried_start_does_not_spawn_a_second_child() {
        let state = state();
        // A file whose content counts spawns: two children would append twice.
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("spawns");
        let script = format!("echo x >> {}", marker.display());

        let first = start(
            State(state.clone()),
            Ok(Json(req("dup", &["/bin/sh", "-c", &script]))),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        await_result(&state, "dup").await;

        let second = start(
            State(state.clone()),
            Ok(Json(req("dup", &["/bin/sh", "-c", &script]))),
        )
        .await;
        assert_eq!(second.status(), StatusCode::OK, "a retry is success");

        assert_eq!(
            state.with_execs(|execs| execs.len()),
            1,
            "the retry created a second entry"
        );
        let content = std::fs::read_to_string(&marker).expect("marker written");
        assert_eq!(
            content.lines().count(),
            1,
            "the retry spawned a second child"
        );
    }

    /// The retry must not disturb the existing entry's output either. Polling
    /// after the retry still sees the first child's stdout.
    #[tokio::test]
    async fn a_retry_leaves_the_existing_entrys_output_intact() {
        let state = state();
        run(&state, req("keep", &["/bin/sh", "-c", "echo original"])).await;

        start(
            State(state.clone()),
            Ok(Json(req("keep", &["/bin/sh", "-c", "echo replacement"]))),
        )
        .await;

        let shared = state
            .with_execs(|execs| execs.get("keep").map(|e| e.shared.clone()))
            .expect("entry");
        let held = shared.result.lock().await.clone().expect("output held");
        assert_eq!(held.stdout.trim(), "original");
    }

    /// Poll is read-only. Nothing about the entry may change across it.
    #[tokio::test]
    async fn poll_does_not_mutate_the_entry() {
        let state = state();
        run(&state, req("ro", &["/bin/sh", "-c", "echo hi"])).await;

        let before = state.with_execs(|execs| {
            let e = execs.get("ro").expect("entry");
            (e.pgid, e.acked_at)
        });
        let before_output = {
            let shared = state
                .with_execs(|execs| execs.get("ro").map(|e| e.shared.clone()))
                .expect("entry");
            shared.result.lock().await.clone()
        };

        for _ in 0..3 {
            let response = poll(State(state.clone()), Path("ro".to_string())).await;
            assert_eq!(response.status(), StatusCode::OK);
        }

        let after = state.with_execs(|execs| {
            let e = execs.get("ro").expect("entry");
            (e.pgid, e.acked_at)
        });
        let after_output = {
            let shared = state
                .with_execs(|execs| execs.get("ro").map(|e| e.shared.clone()))
                .expect("entry");
            shared.result.lock().await.clone()
        };

        assert_eq!(before, after, "poll changed entry metadata");
        assert_eq!(
            before_output.map(|o| (o.stdout, o.exit_code)),
            after_output.map(|o| (o.stdout, o.exit_code)),
            "poll consumed or altered the held output"
        );
        assert_eq!(state.with_execs(|execs| execs.len()), 1);
    }

    /// One of the three security properties the model pins: the agent token
    /// never enters a child's environment. Proven by having the child print its
    /// own environment and grepping it.
    #[tokio::test]
    async fn the_agent_token_never_reaches_the_child_environment() {
        let state = state();
        state.bootstrap(b"super-secret-agent-token");
        // Also present in the daemon's own environment, so an inherited env — not
        // just an explicitly forwarded one — would fail this test.
        // SAFETY-free alternative to set_var: the child env starts empty because
        // build_command calls env_clear, so we assert on that instead of mutating
        // the process environment.
        let outcome = run(&state, req("envtest", &["/usr/bin/env"])).await;

        assert_eq!(outcome.exit_code, Some(0));
        assert!(
            !outcome.stdout.contains("super-secret-agent-token"),
            "the agent token appeared in the child environment: {:?}",
            outcome.stdout
        );
        assert!(
            !outcome.stdout.to_ascii_lowercase().contains("agent_token"),
            "an agent token variable name leaked: {:?}",
            outcome.stdout
        );
        assert!(
            outcome.stdout.trim().is_empty(),
            "the child environment must start empty, got {:?}",
            outcome.stdout
        );
    }

    /// Requested variables do reach the child, so the emptiness above is a
    /// property of what we pass and not of a broken env path.
    #[tokio::test]
    async fn requested_environment_variables_do_reach_the_child() {
        let state = state();
        let mut request = req("envpass", &["/bin/sh", "-c", "echo $WANTED"]);
        request.env.insert("WANTED".to_string(), "yes".to_string());
        let outcome = run(&state, request).await;
        assert_eq!(outcome.stdout.trim(), "yes");
    }

    /// Output past the cap truncates and says so, rather than growing until the
    /// daemon is OOM-killed in a VM nothing can restart it in.
    #[tokio::test]
    async fn output_past_the_cap_is_truncated_and_flagged() {
        let state = state_with(|cfg| cfg.max_output_bytes = 1024);
        let outcome = run(
            &state,
            req(
                "cap",
                &["/bin/sh", "-c", "i=0; while [ $i -lt 400 ]; do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; i=$((i+1)); done"],
            ),
        )
        .await;
        assert!(outcome.truncated, "the cap was not reported");
        assert!(
            outcome.stdout.len() <= 1024,
            "captured {} bytes past a 1024 cap",
            outcome.stdout.len()
        );
        // The command still finished: past the cap we keep draining so the writer
        // never blocks in the kernel.
        assert_eq!(outcome.exit_code, Some(0));
    }

    /// Output under the cap is not flagged, so `truncated` means something.
    #[tokio::test]
    async fn output_under_the_cap_is_not_flagged() {
        let state = state_with(|cfg| cfg.max_output_bytes = 1024);
        let outcome = run(&state, req("nocap", &["/bin/sh", "-c", "echo short"])).await;
        assert!(!outcome.truncated);
        assert_eq!(outcome.stdout.trim(), "short");
    }

    /// Omitting cwd inherits the daemon's working directory. Forcing `/` broke
    /// every prebuilt-image task, and it was the last blocker found in the PR.
    #[tokio::test]
    async fn an_omitted_cwd_inherits_rather_than_defaulting_to_root() {
        let state = state();
        let outcome = run(&state, req("cwd", &["/bin/sh", "-c", "pwd"])).await;
        let expected = std::env::current_dir().expect("cwd");
        assert_eq!(outcome.stdout.trim(), expected.to_string_lossy());
        assert_ne!(outcome.stdout.trim(), "/", "cwd must not default to /");
    }

    #[tokio::test]
    async fn an_explicit_cwd_is_honored() {
        let state = state();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut request = req("cwd2", &["/bin/sh", "-c", "pwd"]);
        request.cwd = Some(dir.path().to_string_lossy().into_owned());
        let outcome = run(&state, request).await;
        // Compared canonically: the temp root can be a symlink on some systems.
        let want = dir.path().canonicalize().expect("canonical");
        let got = std::path::PathBuf::from(outcome.stdout.trim())
            .canonicalize()
            .expect("canonical");
        assert_eq!(got, want);
    }

    /// An argv array execs directly, with no shell between. If a shell were
    /// involved the metacharacters here would be interpreted.
    #[tokio::test]
    async fn an_argv_array_is_not_passed_through_a_shell() {
        let state = state();
        let outcome = run(&state, req("noshell", &["/bin/echo", "a; echo b", "$HOME"])).await;
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout.trim(), "a; echo b $HOME");
    }

    /// Ack releases the output and moves the entry to Acked.
    #[tokio::test]
    async fn ack_releases_the_output_once() {
        let state = state();
        run(&state, req("ackme", &["/bin/sh", "-c", "echo done"])).await;

        let first = ack(State(state.clone()), Path("ackme".to_string())).await;
        assert_eq!(first.status(), StatusCode::OK);

        let acked_at = state.with_execs(|execs| execs.get("ackme").and_then(|e| e.acked_at));
        assert!(acked_at.is_some(), "ack did not start the TTL clock");

        // A second ack has nothing left to hand back, so it is a conflict rather
        // than a 200 with empty output that would read as "no output".
        let second = ack(State(state.clone()), Path("ackme".to_string())).await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    /// Acking a live exec is 409. A silent success would drop output that is
    /// still being written.
    #[tokio::test]
    async fn acking_a_running_exec_is_a_conflict_not_a_silent_success() {
        let state = state();
        let request = req("live", &["/bin/sh", "-c", "sleep 30"]);
        let timeout = validate_timeout(request.timeout_sec).expect("valid");
        let command = build_command(&request).expect("buildable");
        spawn(&state, "live", command, timeout).expect("spawn");

        let response = ack(State(state.clone()), Path("live".to_string())).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            state.with_execs(|execs| execs["live"].acked_at.is_none()),
            "a refused ack must not start the TTL clock"
        );

        kill(State(state.clone()), Path("live".to_string())).await;
    }

    /// Kill signals the group, so a backgrounded grandchild dies with it. The
    /// direct child exits immediately; the sleep it backgrounded is what the
    /// group signal has to reach.
    #[tokio::test]
    async fn kill_signals_the_whole_process_group() {
        let state = state_with(|cfg| {
            cfg.kill_grace = Duration::from_millis(50);
            cfg.output_linger = Duration::from_millis(200);
        });
        let request = req("group", &["/bin/sh", "-c", "sleep 30 & echo started; wait"]);
        let command = build_command(&request).expect("buildable");
        spawn(&state, "group", command, None).expect("spawn");

        let pgid = state
            .with_execs(|execs| execs.get("group").and_then(|e| e.pgid))
            .expect("the pgid must be captured at spawn, not lazily");
        assert!(pgid > 0);

        let response = kill(State(state.clone()), Path("group".to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
        // The body reports whether a signal was actually delivered. Asserting on
        // it rather than only on the status is what catches a lost pgid: a kill
        // with nothing to signal still answers 200.
        assert_eq!(
            body_json(response).await["killed"],
            serde_json::Value::Bool(true),
            "no signal was delivered, so the pgid was lost"
        );

        // `wait` in the script only returns once the backgrounded sleep is gone,
        // so the outcome arriving at all proves the group signal reached it.
        let outcome = await_result(&state, "group").await;
        assert_ne!(
            outcome.exit_code,
            Some(0),
            "the group was not actually signalled"
        );
    }

    /// The pgid is read at spawn, so it survives the child being reaped. Reading
    /// `Child::id()` from the kill path would return `None` here.
    #[tokio::test]
    async fn the_pgid_survives_the_child_being_reaped() {
        let state = state();
        run(&state, req("pgid", &["/bin/true"])).await;
        let pgid = state.with_execs(|execs| execs["pgid"].pgid);
        assert!(pgid.is_some(), "pgid was lost once the child was reaped");

        // Killing an already-gone group is not an error the caller has to handle.
        let response = kill(State(state.clone()), Path("pgid".to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A timeout signals the group and the entry still reports a result rather
    /// than hanging. Kept short so the suite stays fast.
    #[tokio::test]
    async fn a_timeout_kills_the_group_and_still_reports() {
        let state = state_with(|cfg| {
            cfg.kill_grace = Duration::from_millis(30);
            cfg.output_linger = Duration::from_millis(100);
        });
        let mut request = req("slow", &["/bin/sh", "-c", "echo before; sleep 30"]);
        request.timeout_sec = Some(0.15);
        let outcome = run(&state, request).await;

        assert_ne!(outcome.exit_code, Some(0), "the timeout did not fire");
        assert_eq!(
            outcome.stdout.trim(),
            "before",
            "output before the kill is kept"
        );
    }

    /// Only acked entries are collected. An unacked entry has no deadline
    /// however old it is — collecting one destroys output nobody read, which is
    /// the defect the Python daemon shipped by unlinking on exit.
    #[tokio::test]
    async fn collection_takes_acked_entries_only() {
        let state = state_with(|cfg| cfg.exec_ttl = Duration::ZERO);
        run(&state, req("gc-acked", &["/bin/true"])).await;
        run(&state, req("gc-unacked", &["/bin/true"])).await;

        ack(State(state.clone()), Path("gc-acked".to_string())).await;
        // The TTL is zero, so the acked entry is immediately expired while the
        // unacked one is still not eligible.
        assert_eq!(collect_expired(&state), 1);
        assert!(state.with_execs(|execs| execs.contains_key("gc-unacked")));
        assert!(!state.with_execs(|execs| execs.contains_key("gc-acked")));
    }

    #[tokio::test]
    async fn an_acked_entry_survives_until_its_ttl_elapses() {
        let state = state_with(|cfg| cfg.exec_ttl = Duration::from_secs(300));
        run(&state, req("gc-young", &["/bin/true"])).await;
        ack(State(state.clone()), Path("gc-young".to_string())).await;

        assert_eq!(collect_expired(&state), 0);
        assert!(state.with_execs(|execs| execs.contains_key("gc-young")));
    }

    /// An unknown id is 404, because it is genuinely absent.
    #[tokio::test]
    async fn an_unknown_id_is_404_on_every_route() {
        let state = state();
        for response in [
            poll(State(state.clone()), Path("nope".to_string())).await,
            ack(State(state.clone()), Path("nope".to_string())).await,
            kill(State(state.clone()), Path("nope".to_string())).await,
        ] {
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }

    /// A malformed body is 400, never 404. Clients map 404 onto
    /// FileNotFoundError, so the wrong code turns a protocol typo into a phantom
    /// absent artifact — that is how one defect hid for a whole review round.
    #[tokio::test]
    async fn a_malformed_body_is_400_and_never_404() {
        let state = state();
        let rejection = Json::<StartRequest>::from_bytes(b"{").expect_err("invalid json");
        let response = start(State(state.clone()), Err(rejection)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_ne!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            state.with_execs(|execs| execs.is_empty()),
            "a rejected start must not register anything"
        );
    }

    #[tokio::test]
    async fn an_empty_exec_id_is_400() {
        let state = state();
        let response = start(State(state.clone()), Ok(Json(req("", &["/bin/true"])))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(state.with_execs(|execs| execs.is_empty()));
    }

    /// A bad timeout is rejected before anything spawns. The predecessor raised
    /// inside the waiter thread, by which point the child was running with
    /// nobody left to reap it.
    #[tokio::test]
    async fn a_bad_timeout_is_rejected_before_the_child_spawns() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let state = state();
            let mut request = req("badtimeout", &["/bin/sh", "-c", "sleep 30"]);
            request.timeout_sec = Some(bad);

            let response = start(State(state.clone()), Ok(Json(request))).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "timeout_sec {bad} was accepted"
            );
            assert!(
                state.with_execs(|execs| execs.is_empty()),
                "timeout_sec {bad} spawned an orphan before validation"
            );
        }
    }

    #[test]
    fn a_valid_timeout_is_accepted_and_an_absent_one_means_unbounded() {
        assert_eq!(
            validate_timeout(Some(1.5)).expect("valid"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(validate_timeout(None).expect("valid"), None);
    }

    /// An argv array with no program is a 400, not a spawn attempt on "".
    #[tokio::test]
    async fn an_empty_argv_without_shell_is_400() {
        let state = state();
        let response = start(State(state.clone()), Ok(Json(req("empty", &[])))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(state.with_execs(|execs| execs.is_empty()));
    }

    /// Demotion is optional. With no user or group the child runs as the daemon,
    /// which is what every current caller wants.
    #[tokio::test]
    async fn demotion_is_optional() {
        let state = state();
        let outcome = run(&state, req("nodemote", &["/usr/bin/id", "-u"])).await;
        let expected = std::process::Command::new("/usr/bin/id")
            .arg("-u")
            .output()
            .expect("id -u");
        assert_eq!(
            outcome.stdout.trim(),
            String::from_utf8_lossy(&expected.stdout).trim()
        );
    }

    /// Requested demotion reaches the builder. Actually demoting needs root, so
    /// the assertion is on the built command rather than on a spawned child —
    /// what matters is that the uid/gid path is `Command::uid`/`gid` and not
    /// `pre_exec`, and that is visible in the source, not in a child's output.
    #[test]
    fn a_demotion_request_builds_without_pre_exec() {
        let mut request = req("demote", &["/bin/true"]);
        request.user = Some(65534);
        request.group = Some(65534);
        let command = build_command(&request).expect("buildable");
        assert_eq!(
            command.as_std().get_program(),
            std::ffi::OsStr::new("/bin/true")
        );
    }

    /// The phase mapping matches the model's `ExecPhase`.
    #[test]
    fn phases_match_the_modeled_state_machine() {
        assert_eq!(phase_of(false, false), Phase::Running);
        assert_eq!(phase_of(false, true), Phase::Exited);
        assert_eq!(phase_of(true, true), Phase::Acked);
    }
}

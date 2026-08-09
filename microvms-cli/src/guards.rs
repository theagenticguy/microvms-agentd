// SPDX-License-Identifier: Apache-2.0
//! The guards that need to reach inside the crate: the behavioral thinness check (CLI-2), the
//! interrupt teardown (CLI-6), and the classification half of the exit catalogue (CLI-3).
//!
//! # Why these are here and the others are in `tests/`
//!
//! This crate has no lib target — that absence is ARCH-5's witness — so an integration test can
//! only reach it by spawning the binary. That is the right shape for the checks whose subject is
//! the *process*: an exit code (which `ExitCode` deliberately hides in-process), and the
//! single-document property of stdout. Those live in `tests/`.
//!
//! It is the wrong shape for the three below. The behavioral guard has to *inject* a refusing
//! seam, the interrupt guard has to fire an interrupt at a known instant mid-launch, and both are
//! assertions about which code path ran rather than about what the process printed. A spawned
//! binary can do neither without an environment variable that switches in a fake — which would be
//! a test hook in a shipping artifact, and a worse thing than the tests it enables.
//!
//! Compiled only under `cfg(test)`, so none of it is in the binary.

#![cfg(test)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use microvms_core::control::transport::{Call, Reply, Transport};
use microvms_core::control::{Clock, ControlPlane};
use microvms_core::sandbox::Sandbox;
use microvms_core::session::Session;
use microvms_core::{Error, ErrorKind, Region};

use crate::cli::{
    BuildArgs, Cli, Command, CostArgs, DoctorArgs, ExecArgs, InfraFlags, LogsArgs, LsArgs,
    MemoryMib, RegionFlags, ResumeArgs, RunArgs, SuspendArgs, TerminateArgs,
};
use crate::commands::{Ctx, Rendered};
use crate::envelope::{Format, Output};
use crate::exit::{CliError, Exit};
use crate::seam::futures_util_shim::BoxFuture;
use crate::seam::{Attach, CoreSeam, Door, Infra};

// ── CLI-2's behavioral half: a seam that fails closed ────────────────────────

/// The sentinel every refusal carries, so a test can tell "it failed" from "it failed *here*".
const SENTINEL: &str = "seam-was-refused-a1b2c3";

/// A seam whose every door refuses, recording which one was entered.
///
/// The Rust counterpart of `test_cli.py:375`'s `refusing` factory, and it makes the same three
/// assertions possible. The third — *which* door — is the one the Python found it needed after
/// the second was defeated on purpose: a handler that constructed its own client would still
/// fail with the patched error while having bypassed the seam entirely.
struct RefusingSeam {
    entered: Mutex<Vec<Door>>,
}

impl RefusingSeam {
    fn new() -> Self {
        Self {
            entered: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, door: Door) -> Error {
        self.entered.lock().expect("not poisoned").push(door);
        Error::new(
            // `Platform` rather than a kind under test, so a handler that swallowed the failure
            // and reported its own would produce a different code and be caught.
            ErrorKind::Platform,
            format!("{SENTINEL}: the {} door refused", door.as_str()),
        )
    }

    fn doors(&self) -> Vec<Door> {
        self.entered.lock().expect("not poisoned").clone()
    }
}

impl CoreSeam for RefusingSeam {
    fn control_plane(&self, _region: Region) -> BoxFuture<'_, Result<ControlPlane, Error>> {
        let error = self.record(Door::ControlPlane);
        Box::pin(async move { Err(error) })
    }

    fn open_sandbox(
        &self,
        _region: Region,
        _port: Option<u16>,
    ) -> BoxFuture<'_, Result<Sandbox, Error>> {
        let error = self.record(Door::OpenSandbox);
        Box::pin(async move { Err(error) })
    }

    fn attach_session(
        &self,
        _region: Region,
        _attach: Attach,
    ) -> BoxFuture<'_, Result<Session, Error>> {
        let error = self.record(Door::AttachSession);
        Box::pin(async move { Err(error) })
    }

    fn put_artifact(&self, _uri: &str, _bytes: Vec<u8>) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            Err(Error::new(
                ErrorKind::Platform,
                format!("{SENTINEL}: the artifact upload refused"),
            ))
        })
    }
}

/// Infrastructure that satisfies every `require`, so a command reaches the seam rather than
/// failing on a precondition.
///
/// Without this the behavioral guard would pass vacuously: every AWS command would fail at
/// `Infra::require` and never touch a door, and "it failed" would be true for the wrong reason.
fn full_infra() -> Infra {
    Infra {
        bucket: Some("a-bucket".into()),
        build_role_arn: Some("arn:aws:iam::123456789012:role/build".into()),
        execution_role_arn: Some("arn:aws:iam::123456789012:role/execution".into()),
    }
}

fn region_flags() -> RegionFlags {
    RegionFlags {
        region: Some(crate::cli::RegionArg::UsEast1),
        unlisted_region: None,
    }
}

/// A temp file that looks like an aarch64 ELF, so the binary precondition passes.
struct FakeBinary(std::path::PathBuf);

impl FakeBinary {
    fn new(label: &str) -> Self {
        let mut header = vec![0u8; 20];
        header[..4].copy_from_slice(b"\x7fELF");
        header[5] = 1;
        header[18..20].copy_from_slice(&0xB7u16.to_le_bytes());
        let path = std::env::temp_dir().join(format!(
            "microvm-guard-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, header).expect("writes");
        Self(path)
    }
}

impl Drop for FakeBinary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Runs `command` against `seam` and returns whatever the handler produced.
async fn dispatch_with(
    seam: &dyn CoreSeam,
    command: &Command,
    infra: Infra,
) -> (Result<Rendered, CliError>, String) {
    let mut out = Output::new(Format::Json, false, Vec::new(), Vec::new());
    let env = |_: &str| None;
    let result = {
        let mut ctx = Ctx {
            seam,
            out: &mut out,
            infra,
            env: &env,
        };
        handle_for_test(&mut ctx, command).await
    };
    let stderr = String::from_utf8(out.into_streams().1).expect("utf8");
    (result, stderr)
}

/// The dispatcher, as the guard calls it.
///
/// Mirrors `main`'s `handle` and is deliberately a second copy of nothing: it forwards to the same
/// handler functions, and the interrupt for `run` is [`crate::commands::lifecycle::never`] so this
/// guard measures the seam rather than racing a signal.
async fn handle_for_test<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    command: &Command,
) -> Result<Rendered, CliError> {
    use crate::commands::{cost, doctor, lifecycle, local};
    match command {
        Command::Run(args) => lifecycle::run(ctx, args, lifecycle::never()).await,
        Command::Build(args) => lifecycle::build(ctx, args).await,
        Command::Exec(args) => lifecycle::exec(ctx, args).await,
        Command::Suspend(args) => lifecycle::suspend(ctx, args).await,
        Command::Resume(args) => lifecycle::resume(ctx, args).await,
        Command::Terminate(args) => lifecycle::terminate(ctx, args).await,
        Command::Ls(args) => local::ls(ctx, args),
        Command::Logs(args) => local::logs(ctx, args),
        Command::Cost(args) => cost::cost(ctx, args),
        Command::Doctor(args) => doctor::doctor(ctx, args).await,
        Command::Manifest(_) => local::manifest(ctx),
        Command::Constants(_) => local::constants(ctx),
    }
}

/// Every AWS-touching command, its arguments, and the door it must enter.
///
/// The door is named per command rather than left implicit, because "it failed" and "it went
/// through the seam" are different claims and only the second is what CLI-2 asks for.
fn aws_commands(binary: &std::path::Path) -> Vec<(&'static str, Command, Door)> {
    vec![
        (
            "run",
            Command::Run(RunArgs {
                binary: Some(binary.to_path_buf()),
                image: None,
                artifact_uri: Some("s3://bucket/img.zip".into()),
                exec: Some("true".into()),
                name: Some("img".into()),
                memory: MemoryMib::Mib2048,
                dockerfile: None,
                repair_identity: false,
                egress: false,
                keep: false,
                timeout: 30.0,
                max_idle_sec: 600,
                suspended_sec: 600,
                max_duration_sec: 3600,
                port: None,
                state_dir: Some(std::env::temp_dir().join("microvm-guard-ledgers")),
                region: region_flags(),
                infra: InfraFlags::default(),
            }),
            Door::OpenSandbox,
        ),
        (
            "build",
            Command::Build(BuildArgs {
                binary: binary.to_path_buf(),
                artifact_uri: Some("s3://bucket/img.zip".into()),
                name: Some("img".into()),
                memory: MemoryMib::Mib2048,
                dockerfile: None,
                repair_identity: false,
                port: None,
                region: region_flags(),
                infra: InfraFlags::default(),
            }),
            Door::OpenSandbox,
        ),
        (
            "exec",
            Command::Exec(ExecArgs {
                command: "true".into(),
                endpoint: "https://mvm-1.example".into(),
                agent_token: "t".into(),
                microvm_id: "mvm-1".into(),
                timeout: 30.0,
                cwd: None,
                port: None,
                region: region_flags(),
            }),
            Door::AttachSession,
        ),
        (
            "suspend",
            Command::Suspend(SuspendArgs {
                microvm_id: "mvm-1".into(),
                timeout: 30.0,
                region: region_flags(),
            }),
            Door::ControlPlane,
        ),
        (
            "resume",
            Command::Resume(ResumeArgs {
                microvm_id: "mvm-1".into(),
                timeout: 30.0,
                region: region_flags(),
            }),
            Door::ControlPlane,
        ),
        (
            "terminate",
            Command::Terminate(TerminateArgs {
                microvm_id: "mvm-1".into(),
                image_identifier: None,
                image_name: None,
                delete_image: false,
                wait: false,
                region: region_flags(),
            }),
            Door::ControlPlane,
        ),
        (
            "doctor",
            Command::Doctor(DoctorArgs {
                binary: None,
                infra_dir: Some(std::path::PathBuf::from("/definitely/not/a/stack")),
                region: region_flags(),
                infra: InfraFlags::default(),
            }),
            Door::ControlPlane,
        ),
    ]
}

/// The commands that reach no door, and why each is legitimately local.
///
/// Listed with a reason rather than skipped by a naming rule, so a *new* AWS-touching command is
/// covered by the guard by default and can only leave the net by someone writing its name here.
const LOCAL_ONLY: [(&str, &str); 5] = [
    (
        "ls",
        "reads the local ledger; the whole point is that AWS cannot attribute a dead run",
    ),
    (
        "logs",
        "names the build log group and says it cannot read it — no CloudWatch client exists in \
         either crate, and adding one to the CLI is what CLI-2 forbids",
    ),
    (
        "cost",
        "arithmetic over the rate table pinned in microvms-core; no account is involved",
    ),
    (
        "manifest",
        "introspects the clap tree and the exit table, both of which are compile-time constants",
    ),
    (
        "constants",
        "emits microvms_core::constants::as_json for the drift gate",
    ),
];

/// **CLI-2's behavioral guard.** Every AWS-touching command fails through the seam, with the
/// seam's own error, having entered the door it is supposed to.
///
/// Three assertions per command, and the third is the load-bearing one. A handler that reached
/// around the seam and built its own `ControlPlane` would still fail — there are no credentials
/// in a test environment — and it would fail with a *different* message, which is what the second
/// assertion catches. But a handler that failed for its own unrelated reason would pass both, and
/// only "which door was entered" separates that from a thin layer.
///
/// **Falsification** — replace `ctx.seam.control_plane(region)` in `commands::lifecycle::suspend`
/// with a direct `ControlPlane::new(region)` and the `suspend` row goes red on the door list
/// (`entered: nothing`) while still failing. Verified; see the packet's guard proofs.
#[tokio::test]
async fn every_aws_command_fails_through_the_seam_and_names_the_door_it_entered() {
    let binary = FakeBinary::new("behavioral");
    for (name, command, expected) in aws_commands(&binary.0) {
        let seam = RefusingSeam::new();
        let (result, _) = dispatch_with(&seam, &command, full_infra()).await;

        match result {
            Ok(rendered) => {
                // `doctor` is the one command that *reports* a failure rather than raising, so a
                // success envelope is correct — but it must still say the credential check
                // failed, and it must still have gone through the door.
                assert_eq!(
                    name, "doctor",
                    "{name} succeeded with every seam door refusing"
                );
                assert_eq!(rendered.data["ok"], false, "doctor must report the failure");
                assert!(
                    rendered.text.contains(SENTINEL),
                    "doctor's credential check must carry the seam's own error: {}",
                    rendered.text
                );
            }
            Err(failure) => {
                assert!(
                    failure.message.contains(SENTINEL),
                    "{name} failed, but not with the seam's error — it reached AWS another way, \
                     or failed for an unrelated reason: {}",
                    failure.message
                );
            }
        }
        assert!(
            seam.doors().contains(&expected),
            "{name} did not enter {}; it reached the control plane by constructing its own \
             client instead of going through the seam (entered: {:?})",
            expected.as_str(),
            seam.doors(),
        );
    }
}

/// The guard's command list covers every registered command, or names it local with a reason.
///
/// A list is exactly the thing that goes stale when a thirteenth command lands, so it is checked
/// against the clap tree rather than trusted.
#[test]
fn the_behavioral_guard_covers_every_registered_command() {
    use clap::CommandFactory;

    let binary = std::path::PathBuf::from("/tmp/unused");
    let guarded: std::collections::BTreeSet<&str> = aws_commands(&binary)
        .iter()
        .map(|(name, _, _)| *name)
        .collect();
    let local: std::collections::BTreeSet<&str> =
        LOCAL_ONLY.iter().map(|(name, _)| *name).collect();
    let registered: std::collections::BTreeSet<String> = Cli::command()
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    let covered: std::collections::BTreeSet<String> = guarded
        .union(&local)
        .map(|name| (*name).to_string())
        .collect();

    assert_eq!(
        registered,
        covered,
        "commands neither guarded nor declared local: {:?}; declared but not registered: {:?}",
        registered.difference(&covered).collect::<Vec<_>>(),
        covered.difference(&registered).collect::<Vec<_>>(),
    );
    // Every local exemption states its reason, so the list cannot grow silently.
    for (name, reason) in LOCAL_ONLY {
        assert!(reason.len() > 30, "{name}'s exemption needs a real reason");
    }
}

/// A local command reaches no door at all.
///
/// The other half of the guard: without it, a "the seam was entered" assertion would be satisfied
/// by a `cost` command that pointlessly opened a control plane, and the four local commands would
/// stop being local without anything noticing.
#[tokio::test]
async fn no_local_command_touches_a_seam_door() {
    let commands = [
        Command::Ls(LsArgs {
            state_dir: Some(std::path::PathBuf::from("/nonexistent-guard-ledgers")),
        }),
        Command::Logs(LogsArgs {
            image_name: "img".into(),
            region: region_flags(),
        }),
        Command::Cost(CostArgs {
            estimate: false,
            compare: false,
            memory: MemoryMib::Mib2048,
            running_sec: 1.0,
            suspended_sec: 0.0,
            build_sec: 0.0,
            image_gb: None,
            cycles: 1,
            hold_sec: 3600.0,
        }),
        Command::Manifest(crate::cli::ManifestArgs { emit_json: true }),
        Command::Constants(crate::cli::ConstantsArgs { emit_json: true }),
    ];
    for command in &commands {
        let seam = RefusingSeam::new();
        let (_, _) = dispatch_with(&seam, command, full_infra()).await;
        assert!(
            seam.doors().is_empty(),
            "a local command entered {:?}",
            seam.doors()
        );
    }
}

// ── CLI-6: the interrupt teardown ────────────────────────────────────────────

/// A transport that answers from a queue and can fire an interrupt when it sees an operation.
///
/// Hand-rolled rather than reusing core's own recorder, which is `#[cfg(test)]`-private to that
/// crate. That is a feature here rather than a cost: like core's fake, every body below is a
/// **literal** written from the service model, so a member this crate misreads cannot be
/// misread identically by the fake.
struct ScriptedTransport {
    calls: Mutex<Vec<String>>,
    /// Answers per operation, front to back; the last repeats.
    answers: Mutex<std::collections::HashMap<String, std::collections::VecDeque<(u16, String)>>>,
    /// Fired the first time this operation is seen. The interrupt's trigger.
    trigger: Mutex<Option<(String, tokio::sync::oneshot::Sender<()>)>>,
}

impl ScriptedTransport {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            answers: Mutex::new(std::collections::HashMap::new()),
            trigger: Mutex::new(None),
        }
    }

    fn answer(&self, operation: &str, status: u16, body: &str) -> &Self {
        self.answers
            .lock()
            .expect("not poisoned")
            .entry(operation.to_string())
            .or_default()
            .push_back((status, body.to_string()));
        self
    }

    /// Fires `sender` the first time `operation` is called.
    fn fire_on(&self, operation: &str, sender: tokio::sync::oneshot::Sender<()>) -> &Self {
        *self.trigger.lock().expect("not poisoned") = Some((operation.to_string(), sender));
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("not poisoned").clone()
    }

    fn called(&self, operation: &str) -> usize {
        self.calls()
            .iter()
            .filter(|call| *call == operation)
            .count()
    }
}

impl Transport for ScriptedTransport {
    fn send(&self, call: Call) -> BoxFuture<'_, Result<Reply, Error>> {
        let operation = call.operation.to_string();
        self.calls
            .lock()
            .expect("not poisoned")
            .push(operation.clone());

        // The interrupt fires *when the launch is accepted*, which is the instant CLI-6 is about:
        // a VM exists, its identifier is recorded, and the RUNNING wait has not finished.
        let fire = {
            let mut trigger = self.trigger.lock().expect("not poisoned");
            match trigger.take() {
                Some((wanted, sender)) if wanted == operation => Some(sender),
                other => {
                    *trigger = other;
                    None
                }
            }
        };
        if let Some(sender) = fire {
            let _ = sender.send(());
        }

        let answer = {
            let mut answers = self.answers.lock().expect("not poisoned");
            let queue = answers
                .get_mut(&operation)
                .unwrap_or_else(|| panic!("the fake has no answer for {operation}"));
            if queue.len() > 1 {
                queue.pop_front().expect("non-empty")
            } else {
                queue.front().cloned().expect("non-empty")
            }
        };
        Box::pin(async move {
            Ok(Reply {
                status: answer.0,
                body: answer.1.into_bytes(),
            })
        })
    }
}

/// A clock whose `sleep` advances instantly **and yields**.
///
/// The yield is what makes the interrupt guard deterministic rather than a race. A `sleep` that
/// only advanced would let the launch's poll loop run all sixty iterations inside one `poll` of
/// the select's body arm, so the select would never get to look at the interrupt and the run would
/// end in `ERR_TIMEOUT` instead. Yielding returns `Pending` once, which is the select's chance to
/// see that the other arm is ready.
#[derive(Debug, Default)]
struct YieldingClock {
    elapsed: Mutex<Duration>,
}

impl Clock for YieldingClock {
    fn elapsed(&self) -> Duration {
        *self.elapsed.lock().expect("not poisoned")
    }

    fn sleep(
        &self,
        duration: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        *self.elapsed.lock().expect("not poisoned") += duration;
        Box::pin(tokio::task::yield_now())
    }
}

/// A seam that hands out sandboxes over `transport`.
struct ScriptedSeam {
    transport: Arc<ScriptedTransport>,
    clock: Arc<YieldingClock>,
}

impl CoreSeam for ScriptedSeam {
    fn control_plane(&self, region: Region) -> BoxFuture<'_, Result<ControlPlane, Error>> {
        let plane = ControlPlane::with_transport(
            Arc::clone(&self.transport) as Arc<dyn Transport>,
            region,
            Arc::clone(&self.clock) as Arc<dyn Clock>,
        );
        Box::pin(async move { Ok(plane) })
    }

    fn open_sandbox(
        &self,
        region: Region,
        _port: Option<u16>,
    ) -> BoxFuture<'_, Result<Sandbox, Error>> {
        let plane = ControlPlane::with_transport(
            Arc::clone(&self.transport) as Arc<dyn Transport>,
            region,
            Arc::clone(&self.clock) as Arc<dyn Clock>,
        );
        Box::pin(async move { Ok(Sandbox::with_control_plane(plane)) })
    }

    fn attach_session(
        &self,
        _region: Region,
        _attach: Attach,
    ) -> BoxFuture<'_, Result<Session, Error>> {
        Box::pin(async move {
            Err(Error::new(
                ErrorKind::Platform,
                "this guard does not attach sessions",
            ))
        })
    }

    fn put_artifact(&self, _uri: &str, _bytes: Vec<u8>) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move { Ok(()) })
    }
}

/// `RunMicrovmResponse`/`GetMicrovmResponse`, in the model's own spelling.
fn microvm_body(state: &str) -> String {
    format!(
        r#"{{"microvmId": "mvm-abc123", "state": "{state}",
             "endpoint": "https://mvm-abc123.microvm.us-east-1.amazonaws.com",
             "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
             "imageVersion": "1", "maximumDurationInSeconds": 3600, "startedAt": 1754524800}}"#
    )
}

/// `run --image`, so the launch reaches the wire without a build or an upload.
fn interrupt_run_args(state_dir: std::path::PathBuf) -> RunArgs {
    RunArgs {
        binary: None,
        image: Some("arn:aws:lambda:us-east-1:123456789012:microvm-image/img".into()),
        artifact_uri: None,
        exec: None,
        name: Some("img".into()),
        memory: MemoryMib::Mib2048,
        dockerfile: None,
        repair_identity: false,
        egress: false,
        keep: false,
        timeout: 30.0,
        max_idle_sec: 600,
        suspended_sec: 600,
        max_duration_sec: 3600,
        port: None,
        state_dir: Some(state_dir),
        region: region_flags(),
        infra: InfraFlags::default(),
    }
}

/// A state directory that cleans itself up.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "microvm-guard-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// **CLI-6, the guard proof.** An interrupt after the launch is accepted tears the VM down and
/// names every identifier the teardown could not remove.
///
/// The interrupt fires when the fake sees `RunMicrovm`, which is precisely the window that
/// matters: the VM exists and is billing, its id is recorded (`sandbox.rs:574` assigns it before
/// the RUNNING wait), and nothing has confirmed it is ready. `TerminateMicrovm` is scripted to
/// fail, so the id survives into `undeleted` — which is what makes "names what it could not
/// delete" observable rather than trivially true.
///
/// Four assertions, one per way this can be wrong: the exit code says interrupted rather than
/// timed out, the terminate really went to the wire, the leaked id is in the failure envelope's
/// `data`, and the ledger on disk carries it too — because the envelope is lost the moment the
/// terminal scrolls and the file is the operator's actual remedy.
///
/// **Falsification** — replace the `tokio::select!` with a bare `.await` on the launch body and
/// the interrupt is never observed: the run ends in `ERR_TIMEOUT` after the fake clock burns the
/// ready deadline, no `TerminateMicrovm` goes out under the interrupt condition, and all four
/// assertions go red. Verified; see the packet's guard proofs.
#[tokio::test]
async fn an_interrupt_after_launch_tears_down_and_names_every_leaked_identifier() {
    let dir = TempDir::new("interrupt");
    let transport = Arc::new(ScriptedTransport::new());
    let (fire, fired) = tokio::sync::oneshot::channel();
    transport
        .answer("RunMicrovm", 200, &microvm_body("PENDING"))
        // Never reaches RUNNING, so the only way out of the wait is the interrupt.
        .answer("GetMicrovm", 200, &microvm_body("PENDING"))
        // The teardown's terminate fails, so the id has to be reported rather than assumed gone.
        //
        // 409 rather than 500, and the reason is a defect this test found in its own first
        // draft: core retries a 5xx through `send_with_retry`, so a 500 here produced six
        // `TerminateMicrovm` calls and the call-count assertion below read 6. A conflict is
        // both the realistic failure — the VM is in a state that forbids the call — and the one
        // core does not retry, so the count is the observable it is supposed to be.
        .answer(
            "TerminateMicrovm",
            409,
            r#"{"message": "ConflictException"}"#,
        )
        .fire_on("RunMicrovm", fire);

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let args = interrupt_run_args(dir.0.clone());
    let mut out = Output::new(Format::Json, false, Vec::new(), Vec::new());
    let env = |_: &str| None;
    let interrupt: crate::commands::lifecycle::Interrupt<'_> = Box::pin(async move {
        let _ = fired.await;
    });
    let result = {
        let mut ctx = Ctx {
            seam: &seam,
            out: &mut out,
            infra: full_infra(),
            env: &env,
        };
        crate::commands::lifecycle::run(&mut ctx, &args, interrupt).await
    };

    let failure = result.expect_err("an interrupt is a failure");
    assert_eq!(
        failure.exit,
        Exit::Interrupted,
        "an interrupt must not read as a timeout: {}",
        failure.message
    );
    assert_eq!(failure.code(), "ERR_INTERRUPTED");
    assert_eq!(failure.finding(), "The build log group survives Terraform");

    assert_eq!(
        transport.called("TerminateMicrovm"),
        1,
        "the teardown must have run: {:?}",
        transport.calls()
    );

    let envelope = crate::envelope::error(&failure);
    assert_eq!(
        envelope["data"]["leaked"],
        serde_json::json!(["mvm-abc123"]),
        "the identifier the teardown could not delete has to be in the payload: {envelope}"
    );
    assert_eq!(envelope["data"]["microvmId"], "mvm-abc123");
    assert_eq!(envelope["data"]["terminateAccepted"], false);

    // And on disk, because the envelope is gone the moment the terminal scrolls.
    let ledgers = crate::ledger::read_all(&dir.0);
    assert_eq!(ledgers.len(), 1, "{ledgers:?}");
    assert_eq!(
        ledgers[0]["leaked"],
        serde_json::json!(["mvm-abc123"]),
        "{ledgers:?}"
    );

    // The human output warns about it too, and a warning is never suppressed.
    let stderr = String::from_utf8(out.into_streams().1).expect("utf8");
    assert!(
        stderr.contains("warning: could not delete mvm-abc123"),
        "{stderr}"
    );
    assert!(stderr.contains("still billing"), "{stderr}");
}

/// The same interrupt, with a teardown that **succeeds**: no leak reported, still exit 11.
///
/// The negative case, and it is what keeps the test above from passing vacuously. A CLI that
/// listed every identifier it had ever seen as leaked would satisfy "the leak is named" while
/// sending an operator to delete a VM that is already gone — and an operator who is sent on one
/// wild goose chase stops reading the list.
#[tokio::test]
async fn an_interrupt_whose_teardown_succeeds_reports_no_leak_and_still_exits_interrupted() {
    let dir = TempDir::new("interrupt-clean");
    let transport = Arc::new(ScriptedTransport::new());
    let (fire, fired) = tokio::sync::oneshot::channel();
    transport
        .answer("RunMicrovm", 200, &microvm_body("PENDING"))
        .answer("GetMicrovm", 200, &microvm_body("PENDING"))
        .answer("TerminateMicrovm", 200, "{}")
        .fire_on("RunMicrovm", fire);

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let args = interrupt_run_args(dir.0.clone());
    let mut out = Output::new(Format::Json, false, Vec::new(), Vec::new());
    let env = |_: &str| None;
    let interrupt: crate::commands::lifecycle::Interrupt<'_> = Box::pin(async move {
        let _ = fired.await;
    });
    let result = {
        let mut ctx = Ctx {
            seam: &seam,
            out: &mut out,
            infra: full_infra(),
            env: &env,
        };
        crate::commands::lifecycle::run(&mut ctx, &args, interrupt).await
    };

    let failure = result.expect_err("an interrupt is still a failure");
    assert_eq!(failure.exit, Exit::Interrupted);
    let envelope = crate::envelope::error(&failure);
    assert_eq!(
        envelope["data"]["leaked"],
        serde_json::json!([]),
        "a VM that really was terminated must not be reported as leaked: {envelope}"
    );
    assert_eq!(envelope["data"]["terminateAccepted"], true);
    // A clean teardown clears its ledger, so `microvm ls` says nothing outstanding.
    assert!(
        crate::ledger::read_all(&dir.0).is_empty(),
        "a clean teardown leaves no ledger"
    );
}

// ── CLI-3's classification half ──────────────────────────────────────────────

/// **CLI-3, table-driven over every non-zero row that a core failure can produce.**
///
/// Each row induces the failure at the seam and asserts the integer, the `ERR_*` code, and the
/// `docs/PLATFORM.md` finding — the same three `test_cli.py:484` asserts, and for the same reason:
/// a CLI that mapped every failure to one code satisfies "it exited non-zero" and fails here.
///
/// This is the *classification* half. The half that asserts the process really exits with these
/// numbers is `tests/exit_codes.rs`, because `ExitCode` deliberately hides its value in-process.
///
/// **Falsification** — map `ErrorKind::BuildWedged` and `ErrorKind::LaunchDied` to one `Exit` in
/// `Exit::for_kind` and two rows go red on both the code and the finding. Verified; see the
/// packet's guard proofs.
#[tokio::test]
async fn each_induced_failure_class_earns_its_own_code_and_finding() {
    let rows: [(&str, Error, Exit, &str); 11] = [
        (
            "wedged build",
            Error::new(
                ErrorKind::BuildWedged,
                "build never scheduled after 240s: all builds still PENDING — the clientToken \
                 replay signature",
            ),
            Exit::BuildWedged,
            "`clientToken` is a permanent idempotency key",
        ),
        (
            "terminal state before RUNNING",
            Error::new(
                ErrorKind::LaunchDied,
                "microvm mvm-1 reached TERMINATED before RUNNING: run hook returned 500",
            ),
            Exit::LaunchDied,
            "`runHookPayload` arrives wrapped, not as the body",
        ),
        (
            "expired suspended window",
            Error::new(
                ErrorKind::WindowClosed,
                "suspended 301s, past the 300s suspendedDurationSeconds window",
            ),
            Exit::WindowClosed,
            "`idlePolicy`",
        ),
        (
            "mint failure",
            Error::wire(
                microvms_core::WireKind::AuthTokenMint,
                "could not mint a proxy auth token",
            ),
            Exit::Retryable,
            "Endpoint authentication",
        ),
        (
            "wrong agent token",
            Error::wire(
                microvms_core::WireKind::Unauthorized,
                "GET /v1/exec/x -> 401",
            ),
            Exit::Credentials,
            "",
        ),
        (
            "daemon refused the request",
            Error::wire(microvms_core::WireKind::Conflict, "409 wrong state"),
            Exit::Protocol,
            "",
        ),
        (
            "off-table size class",
            Error::invalid_arg("minimumMemoryInMiB=1500 is not a documented size class baseline"),
            Exit::InvalidArg,
            "",
        ),
        (
            "control-plane failure",
            Error::new(ErrorKind::Platform, "ValidationException"),
            Exit::Platform,
            "",
        ),
        (
            "client-side deadline",
            Error::new(
                ErrorKind::Timeout,
                "the image did not become usable within 2700s",
            ),
            Exit::Timeout,
            "",
        ),
        (
            "missing prerequisite",
            Error::new(ErrorKind::Precondition, "no image to launch"),
            Exit::Precondition,
            "",
        ),
        (
            "a bug in this client",
            Error::new(ErrorKind::Unexpected, "no handler claimed this"),
            Exit::Unexpected,
            "",
        ),
    ];

    for (label, error, expected, finding) in rows {
        let failure = crate::exit::classify(&error);
        assert_eq!(failure.exit, expected, "{label}");
        assert_eq!(
            failure.code(),
            expected.code().expect("a non-zero row"),
            "{label}"
        );
        assert_eq!(failure.finding(), finding, "{label}");

        // And the envelope carries all three, since that is what a consumer actually reads.
        let envelope = crate::envelope::error(&failure);
        assert_eq!(envelope["exitCode"], expected.as_u8(), "{label}");
        assert_eq!(envelope["code"], failure.code(), "{label}");
        assert_eq!(envelope["finding"], finding, "{label}");
    }
}

/// The two rows no core error can produce, produced the way the CLI produces them.
///
/// `ERR_EXEC_FAILED` and `ERR_INTERRUPTED` complete the catalogue's coverage: the first is
/// `AlreadyReported` beside a *success* envelope, and the second is the interrupt. Without this
/// the table above would cover eleven of thirteen and the two most CLI-specific rows would be
/// untested.
#[tokio::test]
async fn the_two_cli_only_rows_are_reachable_and_distinct() {
    // ERR_EXEC_FAILED: the sandbox worked and the command in it did not.
    let outcome = crate::render::RunOutcome {
        exec_exit_code: Some(7),
        ..crate::render::RunOutcome::default()
    };
    let rendered = Rendered::ok(
        "microvm.run",
        outcome.to_data(),
        String::new(),
        String::new(),
    )
    .reporting(Exit::ExecFailed);
    assert_eq!(rendered.already_reported, Some(Exit::ExecFailed));
    assert_eq!(Exit::ExecFailed.as_u8(), 13);
    assert_eq!(Exit::ExecFailed.code(), Some("ERR_EXEC_FAILED"));
    // The workload's own code is in the payload and is *not* the process's exit code: a workload
    // exiting 4 must not be indistinguishable from a credential failure.
    assert_eq!(rendered.data["execExitCode"], 7);

    // ERR_INTERRUPTED, from core's own kind.
    let interrupted = crate::exit::classify(&Error::new(ErrorKind::Interrupted, "interrupted"));
    assert_eq!(interrupted.exit, Exit::Interrupted);
    assert_eq!(interrupted.exit.as_u8(), 11);
    assert_ne!(Exit::Interrupted, Exit::ExecFailed);
}

/// A success envelope precedes a non-zero exit, and there is exactly one of them.
///
/// The `AlreadyReported` property: `run`'s workload failed, the output and cost the caller asked
/// for are in `data`, and the code is 13. A `CliError` there would print a second envelope and
/// break the one-document rule — which is why this is a field on the returned value rather than
/// an error the dispatcher raises.
#[test]
fn an_already_reported_exit_writes_one_success_envelope_and_no_failure_one() {
    let mut out = Output::new(Format::Json, false, Vec::new(), Vec::new());
    let rendered = Rendered::ok(
        "microvm.run",
        crate::render::RunOutcome {
            exec_exit_code: Some(7),
            ..crate::render::RunOutcome::default()
        }
        .to_data(),
        "exit code: 7".into(),
        String::new(),
    )
    .reporting(Exit::ExecFailed);

    out.emit(
        &crate::envelope::ok(rendered.kind, rendered.data.clone()),
        &rendered.text,
    );
    let exit = rendered.already_reported.expect("reports a code");
    assert_eq!(exit, Exit::ExecFailed);

    let stdout = String::from_utf8(out.into_streams().0).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("exactly one document");
    assert_eq!(
        parsed["status"], "ok",
        "the envelope is a success: {stdout}"
    );
    assert_eq!(parsed["data"]["execExitCode"], 7);
}

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
    AckArgs, AttachFlags, BuildArgs, Cli, Command, CostArgs, CpArgs, DoctorArgs, ExecArgs,
    Explicit, HealthArgs, InfraFlags, LogsArgs, LsArgs, MemoryMib, PortForwardArgs, RegionFlags,
    ResumeArgs, RunArgs, StdinArgs, SuspendArgs, TerminateArgs, TunnelArgs,
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
///
/// (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)
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

/// The identifier triple every attached command takes, filled with plausible values.
///
/// Plausible rather than empty, because the guard's question is whether the command reached the
/// seam — and a blank endpoint could plausibly be refused *before* the door by some future
/// validation, which would make the door assertion pass for the wrong reason.
fn attach_flags() -> AttachFlags {
    AttachFlags {
        endpoint: Some("https://mvm-1.example".into()),
        agent_token: Some("t".into()),
        microvm_id: Some("mvm-1".into()),
        name: None,
        port: None,
        state_dir: None,
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
///
/// The fetch seam is [`crate::provision::PanickingFetch`], so every guard routed through
/// here is *also* asserting its command never provisions — a run or build handed a binary
/// that reached for GitHub anyway would panic the test rather than pass it quietly.
async fn dispatch_with(
    seam: &dyn CoreSeam,
    command: &Command,
    infra: Infra,
) -> (Result<Rendered, CliError>, String) {
    dispatch_with_fetch(seam, command, infra, &crate::provision::PanickingFetch).await
}

/// [`dispatch_with`], with the provisioning seam scripted — for the guards whose subject
/// *is* the provisioning chain.
async fn dispatch_with_fetch(
    seam: &dyn CoreSeam,
    command: &Command,
    infra: Infra,
    fetch: &dyn crate::provision::Fetch,
) -> (Result<Rendered, CliError>, String) {
    let mut out = Output::new(Format::Json, false, Vec::new(), Vec::new());
    let env = |_: &str| None;
    let result = {
        let mut ctx = Ctx {
            seam,
            out: &mut out,
            infra,
            env: &env,
            fetch,
        };
        // The *shipped* dispatcher, with the one substitution the guard needs: the interrupt for
        // `run` is [`crate::commands::lifecycle::never`], so this measures the seam rather than
        // racing a signal.
        crate::handle(&mut ctx, command, crate::commands::lifecycle::never()).await
    };
    let stderr = String::from_utf8(out.into_streams().1).expect("utf8");
    (result, stderr)
}

/// Every AWS-touching command, its arguments, and the door it must enter.
///
/// The door is named per command rather than left implicit, because "it failed" and "it went
/// through the seam" are different claims and only the second is what CLI-2 asks for.
fn aws_commands(binary: &std::path::Path) -> Vec<(&'static str, Command, Door)> {
    vec![
        (
            // `quickstart` builds, so its door is `run`'s. Its state dir is a fresh temp
            // path because it carries no binary: the door test's scripted fetch provisions
            // one there, which is itself part of what the row proves — quickstart reaches
            // the sandbox door only through the provisioning chain.
            "quickstart",
            Command::Quickstart(crate::cli::QuickstartArgs {
                exec: "true".into(),
                state_dir: Some(std::env::temp_dir().join(format!(
                    "microvm-guard-quickstart-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ))),
                region: region_flags(),
                infra: InfraFlags::default(),
            }),
            Door::OpenSandbox,
        ),
        (
            "run",
            Command::Run(RunArgs {
                binary: Some(binary.to_path_buf()),
                image: None,
                image_version: None,
                artifact_uri: Some("s3://bucket/img.zip".into()),
                exec: Some("true".into()),
                name: Some("img".into()),
                memory: MemoryMib::Mib2048,
                dockerfile: None,
                repair_identity: false,
                log_group: None,
                log_stream: None,
                egress: false,
                launch_env: Vec::new(),
                user: None,
                group: None,
                keep: false,
                identity: false,
                vm_name: None,
                timeout: 30.0,
                max_idle_sec: 600,
                suspended_sec: 600,
                max_duration_sec: 3600,
                port: None,
                state_dir: Some(std::env::temp_dir().join("microvm-guard-ledgers")),
                // No config file: the guard exercises the seam, not the merge, and an
                // ambient microvm.toml in the test runner's cwd must not leak in.
                config: no_config(),
                explicit: Explicit::default(),
                region: region_flags(),
                infra: InfraFlags::default(),
            }),
            Door::OpenSandbox,
        ),
        (
            "build",
            Command::Build(BuildArgs {
                binary: Some(binary.to_path_buf()),
                state_dir: None,
                base_image_version: None,
                artifact_uri: Some("s3://bucket/img.zip".into()),
                name: Some("img".into()),
                memory: MemoryMib::Mib2048,
                dockerfile: None,
                repair_identity: false,
                log_group: None,
                log_stream: None,
                reuse: false,
                port: None,
                region: region_flags(),
                infra: InfraFlags::default(),
            }),
            Door::OpenSandbox,
        ),
        (
            "exec",
            Command::Exec(ExecArgs {
                command: Some("true".into()),
                timeout: 30.0,
                cwd: None,
                env: Vec::new(),
                user: None,
                group: None,
                exec_id: None,
                poll: None,
                detach: false,
                stream: false,
                from_offset: None,
                stdin: false,
                attach: AttachFlags {
                    state_dir: Some(std::env::temp_dir().join("microvm-guard-history")),
                    ..attach_flags()
                },
                region: region_flags(),
            }),
            Door::AttachSession,
        ),
        (
            "health",
            Command::Health(HealthArgs {
                attach: attach_flags(),
                region: region_flags(),
            }),
            Door::AttachSession,
        ),
        (
            "tunnel",
            // `--max-connections 0` for the reason port-forward's entry gives: the guard measures
            // the door, and an entry that could serve a connection would wait for one.
            Command::Tunnel(TunnelArgs {
                ports: "5432".into(),
                bind: "127.0.0.1".into(),
                max_connections: Some(0),
                verify_identity: false,
                identity_host_seed: None,
                identity_vm_public_key: None,
                attach: attach_flags(),
                region: region_flags(),
            }),
            Door::AttachSession,
        ),
        (
            "port-forward",
            // `--max-connections 0` so the guard measures the door and returns: the seam fails
            // before a listener is ever bound, and a guard entry that could serve a connection
            // would be a guard that waits for one.
            Command::PortForward(PortForwardArgs {
                ports: "8080".into(),
                bind: "127.0.0.1".into(),
                max_connections: Some(0),
                attach: attach_flags(),
                region: region_flags(),
            }),
            Door::AttachSession,
        ),
        (
            "ack",
            Command::Ack(AckArgs {
                exec_id: "x-1".into(),
                attach: attach_flags(),
                region: region_flags(),
            }),
            Door::AttachSession,
        ),
        (
            "stdin",
            Command::Stdin(StdinArgs {
                exec_id: "x-1".into(),
                // A literal rather than `-`: `--data -` reads this process's stdin, and a test
                // that blocked on the runner's stdin would hang rather than fail.
                data: Some("hello".into()),
                eof: true,
                attach: attach_flags(),
                region: region_flags(),
            }),
            Door::AttachSession,
        ),
        (
            "cp",
            Command::Cp(CpArgs {
                // A path that does not exist, deliberately: `cp` attaches *before* it reads the
                // local file, so the door is entered either way — and a nonexistent path proves
                // the ordering rather than assuming it. Getting it backwards would make this row
                // fail on a precondition with `entered: nothing`, which is the failure the door
                // assertion is for.
                src: "/definitely/not/here/payload".into(),
                dst: "vm:/tmp/payload".into(),
                tar: false,
                mode: None,
                attach: attach_flags(),
                region: region_flags(),
            }),
            Door::AttachSession,
        ),
        (
            "suspend",
            Command::Suspend(SuspendArgs {
                microvm_id: "mvm-1".into(),
                timeout: 30.0,
                state_dir: Some(std::env::temp_dir().join("microvm-guard-history")),
                region: region_flags(),
            }),
            Door::ControlPlane,
        ),
        (
            "resume",
            Command::Resume(ResumeArgs {
                microvm_id: "mvm-1".into(),
                timeout: 30.0,
                state_dir: Some(std::env::temp_dir().join("microvm-guard-history")),
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
                state_dir: Some(std::env::temp_dir().join("microvm-guard-history")),
                region: region_flags(),
            }),
            Door::ControlPlane,
        ),
        (
            "doctor",
            Command::Doctor(DoctorArgs {
                binary: None,
                infra_dir: Some(std::path::PathBuf::from("/definitely/not/a/stack")),
                config: no_config(),
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
const LOCAL_ONLY: [(&str, &str); 7] = [
    (
        "ls",
        "reads the local ledger; the whole point is that AWS cannot attribute a dead run",
    ),
    (
        "history",
        "reads the local per-VM history; the record's value is that it survives the VM, and \
         no GetMicrovm can answer about an id the platform has already forgotten",
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
    (
        "dockerfile",
        "renders microvms_core::control::default_dockerfile to stdout; the stanza is a string \
         built from compile-time constants and no account is involved",
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
        // A scripted fetch rather than the panicking one, because `quickstart` carries no
        // binary and must provision before it can reach its door. The count assertion
        // below keeps the old property for every other row: only quickstart fetches.
        let fetch = CountingFetch(std::sync::atomic::AtomicUsize::new(0));
        let (result, _) = dispatch_with_fetch(&seam, &command, full_infra(), &fetch).await;
        assert_eq!(
            fetch.0.load(std::sync::atomic::Ordering::SeqCst),
            usize::from(name == "quickstart"),
            "{name} consulted the provisioning chain when it carries its own binary"
        );

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
        Command::History(crate::cli::HistoryArgs {
            microvm_id: "mvm-1".into(),
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
        Command::Manifest,
        Command::Constants(crate::cli::ConstantsArgs { emit_json: true }),
        Command::Dockerfile(crate::cli::DockerfileArgs {
            from: None,
            port: 9000,
            workdir: Some("/workspace".into()),
        }),
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
    calls: Mutex<Vec<Call>>,
    /// Answers per operation, front to back; the last repeats.
    answers: Mutex<std::collections::HashMap<String, std::collections::VecDeque<(u16, String)>>>,
    /// Fired the first time this operation is seen. The interrupt's trigger.
    trigger: Mutex<Option<(String, tokio::sync::oneshot::Sender<()>)>>,
    /// The URIs `put_artifact` was asked to fill. On the transport rather than the seam so
    /// the ordering guard can assert "zero uploads" through the handle it already holds —
    /// an upload is not a control-plane call, so it must not pollute `calls`.
    uploads: Mutex<Vec<String>>,
}

impl ScriptedTransport {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            answers: Mutex::new(std::collections::HashMap::new()),
            trigger: Mutex::new(None),
            uploads: Mutex::new(Vec::new()),
        }
    }

    fn uploads(&self) -> Vec<String> {
        self.uploads.lock().expect("not poisoned").clone()
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
        self.calls
            .lock()
            .expect("not poisoned")
            .iter()
            .map(|call| call.operation.to_string())
            .collect()
    }

    fn called(&self, operation: &str) -> usize {
        self.calls()
            .iter()
            .filter(|call| *call == operation)
            .count()
    }

    /// The paths requested for `operation`, in order — where the resolution guards read
    /// the `nameFilter` and `nextToken` query members.
    fn paths_of(&self, operation: &str) -> Vec<String> {
        self.calls
            .lock()
            .expect("not poisoned")
            .iter()
            .filter(|call| call.operation == operation)
            .map(|call| call.path.clone())
            .collect()
    }

    /// The first body sent to `operation`, as generic JSON — the recorder shape core's own
    /// fake uses, so an assertion reads the wire member rather than a struct's opinion of it.
    fn first_body(&self, operation: &str) -> serde_json::Value {
        let calls = self.calls.lock().expect("not poisoned");
        let call = calls
            .iter()
            .find(|call| call.operation == operation)
            .unwrap_or_else(|| panic!("no call to {operation}"));
        let body = call
            .body
            .as_deref()
            .unwrap_or_else(|| panic!("{operation} sent no body"));
        serde_json::from_slice(body).expect("a JSON body")
    }
}

impl Transport for ScriptedTransport {
    fn send(&self, call: Call) -> BoxFuture<'_, Result<Reply, Error>> {
        let operation = call.operation.to_string();
        self.calls.lock().expect("not poisoned").push(call);

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

    fn put_artifact(&self, uri: &str, _bytes: Vec<u8>) -> BoxFuture<'_, Result<(), Error>> {
        self.transport
            .uploads
            .lock()
            .expect("not poisoned")
            .push(uri.to_string());
        Box::pin(async move { Ok(()) })
    }
}

/// `RunMicrovmResponse`/`GetMicrovmResponse`, in the model's own spelling.
fn microvm_body(state: &str) -> String {
    format!(
        r#"{{"microvmId": "mvm-abc123", "state": "{state}",
             "endpoint": "https://mvm-abc123.microvm.us-east-1.amazonaws.com",
             "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
             "imageVersion": "1", "maximumDurationInSeconds": 3600, "startedAt": 1754524800}}"#
    )
}

/// `run --image`, so the launch reaches the wire without a build or an upload.
fn interrupt_run_args(state_dir: std::path::PathBuf) -> RunArgs {
    run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
        state_dir,
    )
}

/// `run --image <identifier>` with everything else defaulted, for the resolution guards.
fn run_args_for_image(identifier: &str, state_dir: std::path::PathBuf) -> RunArgs {
    RunArgs {
        binary: None,
        image: Some(identifier.into()),
        image_version: None,
        artifact_uri: None,
        exec: None,
        name: Some("img".into()),
        memory: MemoryMib::Mib2048,
        dockerfile: None,
        repair_identity: false,
        log_group: None,
        log_stream: None,
        egress: false,
        launch_env: Vec::new(),
        user: None,
        group: None,
        keep: false,
        identity: false,
        vm_name: None,
        timeout: 30.0,
        max_idle_sec: 600,
        suspended_sec: 600,
        max_duration_sec: 3600,
        port: None,
        state_dir: Some(state_dir),
        // See `aws_commands`: an ambient microvm.toml must not leak into a guard.
        config: no_config(),
        explicit: Explicit::default(),
        region: region_flags(),
        infra: InfraFlags::default(),
    }
}

/// `--no-config`, so a microvm.toml in the test runner's own cwd cannot reach a guard.
fn no_config() -> crate::cli::ConfigFlags {
    crate::cli::ConfigFlags {
        config: None,
        no_config: true,
    }
}

/// A state directory that cleans itself up.
struct TempDir(std::path::PathBuf, #[allow(dead_code)] tempfile::TempDir);

impl TempDir {
    fn new(label: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("microvm-guard-{label}-"))
            .tempdir()
            .expect("a temp dir");
        Self(dir.path().to_path_buf(), dir)
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
            fetch: &crate::provision::PanickingFetch,
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
            fetch: &crate::provision::PanickingFetch,
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
    // The history is the opposite property, asserted side by side on purpose: the ledger is
    // gone because nothing leaked, and the record of what happened survives anyway — with
    // the values the platform reported (`RunMicrovm`'s own id and endpoint, the teardown's
    // acceptance), which is what `microvm history` exists to answer after the VM is gone.
    let events = crate::history::read_events(&dir.0, "mvm-abc123");
    assert_eq!(events.len(), 2, "launched, then terminated: {events:?}");
    assert_eq!(events[0]["event"], "launched");
    assert_eq!(
        events[0]["endpoint"],
        "https://mvm-abc123.microvm.us-east-1.amazonaws.com"
    );
    assert_eq!(events[0]["region"], "us-east-1");
    assert_eq!(events[1]["event"], "terminated");
    assert_eq!(events[1]["terminateAccepted"], true);
}

// ── image name resolution and `build --reuse`, against the scripted transport ─

/// `ListMicrovmImagesResponse`, in the model's own spelling, with an optional `nextToken`.
///
/// A literal for the reason every body in this file is one: a response produced by the
/// same serializer the client deserializes with cannot catch a misspelled member.
fn list_images_body(names: &[&str], next_token: Option<&str>) -> String {
    let items: Vec<String> = names
        .iter()
        .map(|name| {
            format!(
                r#"{{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:{name}",
                     "name": "{name}", "state": "ACTIVE", "createdAt": 1754524800}}"#
            )
        })
        .collect();
    let token = match next_token {
        Some(token) => format!(r#", "nextToken": "{token}""#),
        None => String::new(),
    };
    format!(r#"{{"items": [{}]{token}}}"#, items.join(", "))
}

/// **`run --image <bare-name>` resolves the name to its ARN before the launch.**
///
/// The measured defect this closes: the identifier used to pass verbatim into
/// `RunMicrovm.imageIdentifier`, and a bare name was answered with HTTP 400 "Malformed
/// ARN" — a message that says nothing about names. The assertions are on the wire: the
/// listing was asked with the model's `nameFilter`, and the launch body's
/// `imageIdentifier` is the resolved ARN rather than the name.
///
/// `RunMicrovm` is scripted to fail with a 400 so the test ends at the launch rather than
/// entering the RUNNING wait — resolution has already happened by then, which is what is
/// under test.
///
/// **Guard proof.** Revert the resolution (pass `identifier.clone()` through as before)
/// and the `imageIdentifier` assertion reads the bare name. Run 2026-08-14 against the
/// pre-change handler shape; failed exactly there.
#[tokio::test]
async fn a_bare_image_name_is_resolved_to_its_arn_before_the_launch() {
    let dir = TempDir::new("resolve-name");
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .answer(
            "ListMicrovmImages",
            200,
            &list_images_body(&["coding-agents"], None),
        )
        .answer("RunMicrovm", 400, r#"{"message": "scripted stop"}"#);

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Run(run_args_for_image("coding-agents", dir.0.clone()));
    let (result, stderr) = dispatch_with(&seam, &command, full_infra()).await;
    result.expect_err("the scripted RunMicrovm failure ends the run after resolution");

    assert_eq!(transport.called("ListMicrovmImages"), 1);
    let listing = transport.paths_of("ListMicrovmImages");
    assert!(
        listing[0].contains("nameFilter=coding-agents"),
        "the listing narrows by the model's nameFilter member: {}",
        listing[0]
    );

    let body = transport.first_body("RunMicrovm");
    assert_eq!(
        body["imageIdentifier"],
        "arn:aws:lambda:us-east-1:123456789012:microvm-image:coding-agents",
        "the launch must carry the resolved ARN, never the bare name — a name here is the \
         Malformed-ARN 400 this exists to close: {body}"
    );

    // The progress line names the resolved ARN, so an operator reading a stalled launch
    // knows which image the name landed on.
    assert!(
        stderr.contains("resolved image name coding-agents to arn:aws:lambda"),
        "{stderr}"
    );
}

/// **An identifier already shaped like an ARN passes through with zero listing calls.**
///
/// The caller who holds the ARN — every existing script — pays nothing for the
/// resolution existing. Asserted on the call count, which is the observable that
/// distinguishes "resolved to itself" from "never looked".
#[tokio::test]
async fn an_arn_image_identifier_launches_with_no_listing_call() {
    let dir = TempDir::new("resolve-arn-passthrough");
    let transport = Arc::new(ScriptedTransport::new());
    transport.answer("RunMicrovm", 400, r#"{"message": "scripted stop"}"#);

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let arn = "arn:aws:lambda:us-east-1:123456789012:microvm-image:img";
    let command = Command::Run(run_args_for_image(arn, dir.0.clone()));
    let (result, stderr) = dispatch_with(&seam, &command, full_infra()).await;
    result.expect_err("the scripted RunMicrovm failure ends the run");

    assert_eq!(
        transport.called("ListMicrovmImages"),
        0,
        "an ARN must cost zero extra calls: {:?}",
        transport.calls()
    );
    assert_eq!(transport.first_body("RunMicrovm")["imageIdentifier"], arn);
    assert!(
        !stderr.contains("resolved image name"),
        "nothing was resolved, so nothing says so: {stderr}"
    );
}

/// **`run --launch-env` reaches the `runHookPayload` the daemon parses.**
///
/// Asserted on the wire body rather than on `RunArgs`, because the flag existing and the
/// value arriving are two different facts — and the second one is what a workload depends
/// on. `RunMicrovm` is scripted to fail so the test ends at the launch, which is after the
/// payload is built.
///
/// **Guard proof.** Drop the `with_launch_env` loop from `commands/lifecycle.rs` and the
/// `env` assertions read `null`.
#[tokio::test]
async fn a_launch_env_flag_reaches_the_run_hook_payload() {
    let dir = TempDir::new("launch-env");
    let transport = Arc::new(ScriptedTransport::new());
    transport.answer("RunMicrovm", 400, r#"{"message": "scripted stop"}"#);

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let mut args = run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
        dir.0.clone(),
    );
    args.launch_env = vec![
        (
            "ANTHROPIC_BASE_URL".to_string(),
            "https://gateway.example".to_string(),
        ),
        ("EMPTY".to_string(), String::new()),
    ];
    let (result, _) = dispatch_with(&seam, &Command::Run(args), full_infra()).await;
    result.expect_err("the scripted RunMicrovm failure ends the run after the payload is built");

    let body = transport.first_body("RunMicrovm");
    let payload = body["runHookPayload"]
        .as_str()
        .expect("runHookPayload is a string");
    // One parse deeper, which is where the daemon reads it from as well.
    let inner: serde_json::Value =
        serde_json::from_str(payload).expect("the payload is itself JSON");
    assert_eq!(
        inner["env"]["ANTHROPIC_BASE_URL"],
        "https://gateway.example"
    );
    assert_eq!(
        inner["env"]["EMPTY"], "",
        "an empty VALUE is a variable set to the empty string, not an omitted one"
    );
    assert!(
        inner["agent_token"].as_str().is_some_and(|t| !t.is_empty()),
        "the token still rides alongside the env: {payload}"
    );
}

/// **A run with no `--launch-env` emits no `env` key at all.**
///
/// The compatibility floor, and it is worth a guard because the cheap implementation —
/// always serialize the map — would put `"env":{}` on the wire for every existing caller
/// and spend their payload budget on nothing.
#[tokio::test]
async fn a_run_without_a_launch_env_emits_no_env_key() {
    let dir = TempDir::new("launch-env-absent");
    let transport = Arc::new(ScriptedTransport::new());
    transport.answer("RunMicrovm", 400, r#"{"message": "scripted stop"}"#);

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Run(run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
        dir.0.clone(),
    ));
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    result.expect_err("the scripted RunMicrovm failure ends the run");

    let payload = transport.first_body("RunMicrovm")["runHookPayload"]
        .as_str()
        .expect("a string")
        .to_string();
    assert!(
        !payload.contains("env"),
        "an unset launch env must not appear on the wire: {payload}"
    );
}

// ── microvm.toml: the config merge on the wire (issue #73) ───────────────────

/// A `microvm.toml` in a temp directory, removed on drop.
struct ConfigFile(std::path::PathBuf);

impl ConfigFile {
    fn new(label: &str, text: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "microvm-guard-config-{label}-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, text).expect("writes");
        Self(path)
    }
}

impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// **Config-file knobs reach the wire, and a typed flag beats the file on the same
/// field.**
///
/// Asserted on the `RunMicrovm` body rather than on the merge's own report, because the
/// file existing and its values arriving are two different facts — the launch request is
/// what the VM's policy windows are actually set from. Both halves in one scripted run:
/// `suspendedDurationSeconds` comes from the file (no flag typed), and `--max-idle-sec`
/// beats the file's value on `maxIdleTimeoutSeconds` because `explicit` says the caller
/// typed it. The env merge is per key: the file's `RUST_LOG` survives beside the flag's
/// winning `CI`.
///
/// **Falsification** — invert the `explicit` branch in `config::pick` (make a typed flag
/// lose to the file) and the `maxIdleTimeoutSeconds` assertion reads 120; drop the
/// config layer from `merge_config` and the `suspendedDurationSeconds` assertion reads
/// the built-in 600. Both were done on 2026-08-28 and both failed as stated, then were
/// restored.
#[tokio::test]
async fn config_knobs_reach_the_wire_and_a_typed_flag_beats_the_file() {
    let dir = TempDir::new("config-wire");
    let file = ConfigFile::new(
        "wire",
        r#"
memory = 4096
max-idle-sec = 120
suspended-sec = 300
egress = true

[env]
RUST_LOG = "debug"
CI = "0"
"#,
    );
    let transport = Arc::new(ScriptedTransport::new());
    transport.answer("RunMicrovm", 400, r#"{"message": "scripted stop"}"#);

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let mut args = run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
        dir.0.clone(),
    );
    args.config = crate::cli::ConfigFlags {
        config: Some(file.0.clone()),
        no_config: false,
    };
    // The caller typed `--max-idle-sec 90` and nothing else: `explicit` is the parse's
    // answer, set here the way `main.rs` sets it from `value_source`. 90 rather than
    // something smaller because core refuses idle windows under the model's minimum of
    // 60 before the wire.
    args.max_idle_sec = 90;
    args.explicit.max_idle_sec = true;
    args.launch_env = vec![("CI".to_string(), "1".to_string())];

    let (result, _) = dispatch_with(&seam, &Command::Run(args), full_infra()).await;
    result.expect_err("the scripted RunMicrovm failure ends the run after the request is built");

    let body = transport.first_body("RunMicrovm");
    // `memory` is a build-time knob (it sizes the image, not the launch), so a
    // `run --image` body carries no memory field — its merge is pinned through
    // `merge_config`'s report in the resolved-config guard below instead.
    assert_eq!(
        body["idlePolicy"]["suspendedDurationSeconds"], 300,
        "the file's suspended window reaches the launch: {body}"
    );
    assert_eq!(
        body["idlePolicy"]["maxIdleDurationSeconds"], 90,
        "the typed flag beats the file on the same field: {body}"
    );
    assert!(
        body["egressNetworkConnectors"]
            .as_array()
            .is_some_and(|connectors| !connectors.is_empty()),
        "egress = true in the file opts into the connector: {body}"
    );
    let payload: serde_json::Value =
        serde_json::from_str(body["runHookPayload"].as_str().expect("a payload string"))
            .expect("the payload is itself JSON");
    assert_eq!(
        payload["env"]["RUST_LOG"], "debug",
        "the file's env key survives the per-key merge: {payload}"
    );
    assert_eq!(
        payload["env"]["CI"], "1",
        "the flag pair wins its own key: {payload}"
    );
}

/// **A broken config file is `ERR_CONFIG` with zero doors entered.**
///
/// The refusal is local and its cost is the acceptance criterion: a file typo must not
/// spend a credential resolution, let alone a launch. Asserted on the seam's door list,
/// the same observable the named-VM collision guard pins.
///
/// **Falsification** — move the `merge_config` call below `open_sandbox` in
/// `commands/lifecycle.rs` and the door list reads `[OpenSandbox]`. Done on 2026-08-28;
/// failed as stated; restored.
#[tokio::test]
async fn a_broken_config_file_is_refused_with_its_own_row_and_zero_doors() {
    let dir = TempDir::new("config-broken");
    let file = ConfigFile::new("broken", "memroy = 4096\n");
    let seam = RefusingSeam::new();
    let mut args = run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
        dir.0.clone(),
    );
    args.config = crate::cli::ConfigFlags {
        config: Some(file.0.clone()),
        no_config: false,
    };

    let (result, _) = dispatch_with(&seam, &Command::Run(args), full_infra()).await;
    let failure = result.expect_err("a broken file refuses the run");
    assert_eq!(failure.exit, Exit::Config, "{failure:?}");
    assert_eq!(failure.code(), "ERR_CONFIG");
    assert_eq!(failure.exit.as_u8(), 15);
    assert!(
        failure.message.contains("memroy"),
        "the refusal names the unknown key: {}",
        failure.message
    );
    assert!(
        failure
            .suggestions
            .iter()
            .any(|hint| hint.contains("--no-config")),
        "{failure:?}"
    );
    assert_eq!(
        seam.doors(),
        Vec::<Door>::new(),
        "a config refusal must cost zero billable calls"
    );
}

/// **The envelope reports what each knob resolved to and which source won.**
///
/// `resolvedConfig` is the file's whole point made legible: a caller who stopped passing
/// flags reads what the run actually used instead of re-deriving the precedence. Scripted
/// to fail at the launch — the *failure* envelope does not carry it, so this asserts on
/// the merge output through a successful parse instead: the merged args and report are
/// checked directly, which is the same seam `run` reads.
///
/// **Falsification** — make `config::pick`'s config arm report `Source::Default` and the
/// `memory` source assertion reads `"default"`. Done on 2026-08-28; failed as stated;
/// restored.
#[tokio::test]
async fn the_resolved_config_report_names_each_knobs_source() {
    let file = ConfigFile::new(
        "resolved",
        "memory = 8192\nexec = \"pytest -q\"\nartifacts = [\"dist/**\"]\n",
    );
    let mut args = run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
        std::env::temp_dir(),
    );
    args.config = crate::cli::ConfigFlags {
        config: Some(file.0.clone()),
        no_config: false,
    };
    args.explicit.max_idle_sec = true;
    args.max_idle_sec = 90;

    // A pinned environment: the region report's env layer must be deterministic here.
    let merged = crate::commands::lifecycle::merge_config(&args, &|_| None).expect("merges");
    assert_eq!(merged.config_path.as_deref(), Some(file.0.as_path()));
    assert_eq!(merged.artifacts, ["dist/**"]);

    let knob = |name: &str| merged.resolved[name].clone();
    assert_eq!(knob("memory")["value"], 8192);
    assert_eq!(knob("memory")["source"], "config");
    assert_eq!(knob("exec")["value"], "pytest -q");
    assert_eq!(knob("exec")["source"], "config");
    assert_eq!(knob("maxIdleSec")["value"], 90);
    assert_eq!(knob("maxIdleSec")["source"], "flag");
    assert_eq!(knob("suspendedSec")["value"], 600);
    assert_eq!(knob("suspendedSec")["source"], "default");
    assert_eq!(knob("artifacts")["source"], "config");
    // The image was a flag (run_args_for_image sets it), so the report says so.
    assert_eq!(knob("image")["source"], "flag");
}

/// **The logging pair merges flag-over-file per knob, and a merged stream with no merged
/// group is refused** — the combination neither layer can see alone.
///
/// The stream comes from the flag and the group from the file in the first case, which is
/// the cross-layer pair the per-knob `pick` has to compose; the second case drops the
/// file and the same flag stream becomes a refusal, because a stream inside a group the
/// service names randomly is a location that does not exist.
///
/// **Falsification** — move the stream-needs-a-group check before the merge (test the
/// flags alone) and the first case fails: the flag stream plus the file group is legal
/// and would be refused.
#[tokio::test]
async fn the_log_knobs_merge_flag_over_file_and_a_cross_layer_stream_needs_its_group() {
    let file = ConfigFile::new(
        "log-knobs",
        "log-group = \"/aws/lambda-microvms/from-file\"\nlog-stream = \"file-stream\"\n",
    );
    let mut args = run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
        std::env::temp_dir(),
    );
    args.config = crate::cli::ConfigFlags {
        config: Some(file.0.clone()),
        no_config: false,
    };
    args.log_stream = Some("flag-stream".into());

    let merged = crate::commands::lifecycle::merge_config(&args, &|_| None).expect("merges");
    let knob = |name: &str| merged.resolved[name].clone();
    assert_eq!(knob("logGroup")["value"], "/aws/lambda-microvms/from-file");
    assert_eq!(knob("logGroup")["source"], "config");
    assert_eq!(knob("logStream")["value"], "flag-stream");
    assert_eq!(
        knob("logStream")["source"],
        "flag",
        "the typed flag beats the file's stream"
    );
    assert_eq!(
        merged.args.log_group.as_deref(),
        Some("/aws/lambda-microvms/from-file")
    );
    assert_eq!(merged.args.log_stream.as_deref(), Some("flag-stream"));

    // The same flag stream with no file is a refusal: no layer supplied a group.
    let mut orphan = run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
        std::env::temp_dir(),
    );
    orphan.log_stream = Some("flag-stream".into());
    // A `match` rather than `expect_err`, because `MergedRunArgs` carries no `Debug` —
    // deliberately, since `RunArgs` holds the agent-token-adjacent launch env.
    let Err(error) = crate::commands::lifecycle::merge_config(&orphan, &|_| None) else {
        panic!("a stream with no group from either layer must be refused");
    };
    assert_eq!(error.exit, Exit::InvalidArg, "{}", error.message);
    assert!(error.message.contains("log group"), "{}", error.message);
}

/// **A typed `BINARY` positional suppresses the file's `image`, because the pair is one
/// decision: `run` builds exactly when the merged image is absent.**
///
/// The failure this closes: a developer in a project whose file pins `image` types
/// `microvm run ./fresh-agentd` expecting a build-and-launch of that binary; a file that
/// silently won would run their tests against the stale pinned image.
///
/// **Falsification** — drop the `args.binary.is_some() && args.image.is_none()`
/// suppression from `merge_config` and the image assertion reads `"ci-image"` while
/// building stays false. Done on 2026-08-28; failed as stated; restored.
#[tokio::test]
async fn a_typed_binary_positional_beats_the_files_image() {
    let file = ConfigFile::new("binary-beats-image", "image = \"ci-image\"\n");
    let mut args = run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
        std::env::temp_dir(),
    );
    args.image = None;
    args.binary = Some("./fresh-agentd".into());
    args.config = crate::cli::ConfigFlags {
        config: Some(file.0.clone()),
        no_config: false,
    };

    let merged = crate::commands::lifecycle::merge_config(&args, &|_| None).expect("merges");
    assert_eq!(
        merged.args.image, None,
        "the typed positional suppresses the file's image: {:?}",
        merged.resolved
    );
    assert_eq!(
        merged.args.binary.as_deref(),
        Some(std::path::Path::new("./fresh-agentd"))
    );
    // With nothing typed for the pair, the file's image wins as usual.
    args.binary = None;
    let merged = crate::commands::lifecycle::merge_config(&args, &|_| None).expect("merges");
    assert_eq!(merged.args.image.as_deref(), Some("ci-image"));
    assert_eq!(merged.resolved["image"]["source"], "config");
}

/// **The region report walks the run's whole chain: past the file sit the environment
/// variables, then the built-in — never `null` from `default` while the launch goes
/// where `$AWS_REGION` points.**
///
/// **Falsification** — report the pre-`resolve` flag value instead of continuing the
/// chain and the env case reads `null`/`"default"`. Done on 2026-08-28; failed as
/// stated; restored.
#[tokio::test]
async fn the_region_report_names_the_environments_region_when_the_environment_decides() {
    let mut args = run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
        std::env::temp_dir(),
    );
    args.config = no_config();
    // `run_args_for_image` pins a region flag; the chain under test starts below it.
    args.region = crate::cli::RegionFlags::default();

    // No flag, no file: the environment decides, and the report says so.
    let env = |name: &str| (name == "AWS_REGION").then(|| "eu-west-1".to_string());
    let merged = crate::commands::lifecycle::merge_config(&args, &env).expect("merges");
    assert_eq!(merged.resolved["region"]["value"], "eu-west-1");
    assert_eq!(merged.resolved["region"]["source"], "env");

    // No flag, no file, no environment: the built-in, named rather than null.
    let merged = crate::commands::lifecycle::merge_config(&args, &|_| None).expect("merges");
    assert_eq!(merged.resolved["region"]["value"], "us-east-1");
    assert_eq!(merged.resolved["region"]["source"], "default");
}

/// **A name no image carries is a local `ERR_PRECONDITION` naming the name and the
/// remedy — and no launch goes out.**
///
/// The alternative was the service's 400 "Malformed ARN", which sends the reader to
/// check their ARN syntax rather than to build the image.
#[tokio::test]
async fn an_unknown_image_name_fails_precondition_before_any_launch() {
    let dir = TempDir::new("resolve-miss");
    let transport = Arc::new(ScriptedTransport::new());
    transport.answer("ListMicrovmImages", 200, &list_images_body(&[], None));

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Run(run_args_for_image("no-such-image", dir.0.clone()));
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;

    let failure = result.expect_err("nothing to launch from");
    assert_eq!(failure.exit, Exit::Precondition);
    assert_eq!(failure.code(), "ERR_PRECONDITION");
    assert!(
        failure.message.contains("no-such-image"),
        "{}",
        failure.message
    );
    assert!(
        failure.message.contains("microvm build"),
        "the remedy is a build, and the message has to say so: {}",
        failure.message
    );
    assert_eq!(
        transport.called("RunMicrovm"),
        0,
        "no launch may go out for a name that resolved to nothing"
    );
}

/// **Resolution follows `nextToken`**, at this level too: an image on page two of the
/// account's listing is found and launched from.
///
/// Core has the same test against its own fake; this one exists because the CLI is the
/// consumer the packet names, and a delegation that dropped the token would pass core's
/// test while every CLI resolution stopped at page one.
#[tokio::test]
async fn resolution_reads_past_the_first_page_of_the_listing() {
    let dir = TempDir::new("resolve-paged");
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .answer(
            "ListMicrovmImages",
            200,
            &list_images_body(&["unrelated"], Some("page-2")),
        )
        .answer(
            "ListMicrovmImages",
            200,
            &list_images_body(&["coding-agents"], None),
        )
        .answer("RunMicrovm", 400, r#"{"message": "scripted stop"}"#);

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Run(run_args_for_image("coding-agents", dir.0.clone()));
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    result.expect_err("the scripted RunMicrovm failure ends the run after resolution");

    assert_eq!(transport.called("ListMicrovmImages"), 2, "both pages read");
    let listing = transport.paths_of("ListMicrovmImages");
    assert!(
        listing[1].contains("nextToken=page-2"),
        "the second request carries the first page's token: {}",
        listing[1]
    );
    assert_eq!(
        transport.first_body("RunMicrovm")["imageIdentifier"],
        "arn:aws:lambda:us-east-1:123456789012:microvm-image:coding-agents"
    );
}

/// The name `build --reuse` derives for `binary`, computed the way the handler computes
/// it — through core's public hash over the same inputs — so the test knows the name
/// without copying the derivation logic.
fn expected_reuse_name(prefix: &str, binary: &std::path::Path) -> String {
    let bytes = std::fs::read(binary).expect("the fake binary is readable");
    let dockerfile = microvms_core::control::default_dockerfile(
        9000,
        None,
        &microvms_core::control::BaseImage::al2023(),
    );
    let hash = microvms_core::control::artifact_content_hash(&bytes, &dockerfile);
    format!("{prefix}-{}", &hash[..12])
}

/// **`build --reuse`, the hit: an image whose content-hash name already exists means no
/// build at all.**
///
/// The load-bearing assertion is the `CreateMicrovmImage` count: a reuse that "worked"
/// while still creating an image would bill a build and — worse — replay the
/// stale-snapshot hazard the flag exists to close. The envelope carries `reused: true`
/// and the existing image's identifier, which is what a script keys on.
///
/// **Guard proof.** Make the hit path fall through to the build (delete the early
/// `return` on `find_image_by_name`'s `Some`) and the count assertion goes red with a
/// `CreateMicrovmImage` the fake then also fails for lack of an answer.
#[tokio::test]
async fn a_reuse_build_whose_hash_name_exists_skips_the_build_entirely() {
    let binary = FakeBinary::new("reuse-hit");
    let expected = expected_reuse_name("coding-agents", &binary.0);

    let transport = Arc::new(ScriptedTransport::new());
    transport.answer(
        "ListMicrovmImages",
        200,
        &list_images_body(&[&expected], None),
    );

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Build(BuildArgs {
        binary: Some(binary.0.clone()),
        state_dir: None,
        base_image_version: None,
        artifact_uri: None,
        name: Some("coding-agents".into()),
        memory: MemoryMib::Mib2048,
        dockerfile: None,
        repair_identity: false,
        log_group: None,
        log_stream: None,
        reuse: true,
        port: None,
        region: region_flags(),
        infra: InfraFlags::default(),
    });
    let (result, stderr) = dispatch_with(&seam, &command, full_infra()).await;
    let rendered = result.expect("a hit is a success");

    assert_eq!(
        transport.called("CreateMicrovmImage"),
        0,
        "a reuse hit must build nothing: {:?}",
        transport.calls()
    );
    assert_eq!(transport.called("ListMicrovmImages"), 1);
    let listing = transport.paths_of("ListMicrovmImages");
    assert!(
        listing[0].contains(&format!("nameFilter={expected}")),
        "the listing is asked for the derived name: {}",
        listing[0]
    );

    assert_eq!(rendered.data["reused"], true);
    assert_eq!(rendered.data["imageName"], expected.as_str());
    assert_eq!(
        rendered.data["imageIdentifier"],
        format!("arn:aws:lambda:us-east-1:123456789012:microvm-image:{expected}"),
        "the existing image's identifier is the envelope's answer"
    );
    assert!(stderr.contains("reusing"), "{stderr}");
}

/// **`build --reuse`, the miss: the build runs, under the derived name.**
///
/// Two claims. The build happened — `CreateMicrovmImage` went out — and the name it went
/// out under carries the content hash, which is what makes the *next* invocation with
/// the same inputs a hit. A miss that built under the bare prefix would create an image
/// reuse can never find, and the flag would rebuild forever while reporting success.
///
/// **Guard proof.** Keep the seed as the request name on the miss path (drop the
/// `request.name = name.clone()` assignment) and the `body["name"]` assertion reads
/// `coding-agents` with no hash suffix.
#[tokio::test]
async fn a_reuse_build_whose_hash_name_is_absent_builds_under_the_derived_name() {
    let binary = FakeBinary::new("reuse-miss");
    let expected = expected_reuse_name("coding-agents", &binary.0);

    let transport = Arc::new(ScriptedTransport::new());
    transport
        .answer("ListMicrovmImages", 200, &list_images_body(&[], None))
        .answer(
            "CreateMicrovmImage",
            201,
            &format!(
                r#"{{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:{expected}",
                     "name": "{expected}", "state": "CREATING", "createdAt": 1754524800,
                     "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
                     "buildRoleArn": "arn:aws:iam::123456789012:role/build",
                     "codeArtifact": {{"uri": "s3://a-bucket/{expected}.zip"}},
                     "imageVersion": "1"}}"#
            ),
        )
        .answer(
            "GetMicrovmImage",
            200,
            &format!(
                r#"{{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:{expected}",
                     "name": "{expected}", "state": "CREATED", "createdAt": 1754524800}}"#
            ),
        );

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Build(BuildArgs {
        binary: Some(binary.0.clone()),
        state_dir: None,
        base_image_version: None,
        artifact_uri: None,
        name: Some("coding-agents".into()),
        memory: MemoryMib::Mib2048,
        dockerfile: None,
        repair_identity: false,
        log_group: None,
        log_stream: None,
        reuse: true,
        port: None,
        region: region_flags(),
        infra: InfraFlags::default(),
    });
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    let rendered = result.expect("a miss builds and succeeds");

    assert_eq!(transport.called("CreateMicrovmImage"), 1, "the miss builds");
    let body = transport.first_body("CreateMicrovmImage");
    assert_eq!(
        body["name"],
        expected.as_str(),
        "the build goes out under the derived name, hash included — the bare prefix would \
         create an image reuse can never find: {body}"
    );
    assert_eq!(
        body["codeArtifact"]["uri"],
        format!("s3://a-bucket/{expected}.zip"),
        "the derived artifact key follows the derived name"
    );
    assert_eq!(rendered.data["reused"], false);
    assert_eq!(rendered.data["imageName"], expected.as_str());
}

/// A [`crate::provision::Fetch`] that writes an aarch64 ELF header and counts calls, for
/// the provisioning guards.
struct CountingFetch(std::sync::atomic::AtomicUsize);

impl crate::provision::Fetch for CountingFetch {
    fn fetch(
        &self,
        _: &str,
        dest: &std::path::Path,
        _: &mut dyn FnMut(&str),
    ) -> Result<crate::provision::Verification, String> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut header = vec![0u8; 20];
        header[..4].copy_from_slice(b"\x7fELF");
        header[5] = 1;
        header[18..20].copy_from_slice(&0xB7u16.to_le_bytes());
        std::fs::write(dest, header).map_err(|error| error.to_string())?;
        Ok(crate::provision::Verification::Attestation)
    }
}

/// `build` arguments with **no binary at all** — the headline case provisioning exists for.
fn build_args_without_binary(state_dir: std::path::PathBuf) -> BuildArgs {
    BuildArgs {
        binary: None,
        state_dir: Some(state_dir),
        base_image_version: None,
        artifact_uri: None,
        name: Some("prov".into()),
        memory: MemoryMib::Mib2048,
        dockerfile: None,
        repair_identity: false,
        log_group: None,
        log_stream: None,
        reuse: false,
        port: None,
        region: region_flags(),
        infra: InfraFlags::default(),
    }
}

/// The scripted control-plane answers a provisioning build needs: one create, one poll.
fn script_prov_build(transport: &ScriptedTransport) {
    transport
        .answer(
            "CreateMicrovmImage",
            201,
            r#"{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:prov",
                 "name": "prov", "state": "CREATING", "createdAt": 1754524800,
                 "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
                 "buildRoleArn": "arn:aws:iam::123456789012:role/build",
                 "codeArtifact": {"uri": "s3://a-bucket/prov.zip"},
                 "imageVersion": "1"}"#,
        )
        .answer(
            "GetMicrovmImage",
            200,
            r#"{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:prov",
                 "name": "prov", "state": "CREATED", "createdAt": 1754524800}"#,
        );
}

/// **A `build` with no binary provisions one, builds from it, and says so on the
/// envelope; the next invocation reads the cache instead of fetching again.** The whole
/// self-provisioning promise in one guard: `microvm build`/`run` on a fresh machine needs
/// no path to this product's own component, and one download serves every later call.
///
/// **Guard proof.** Reorder the resolution chain so the fetch outranks the cache
/// (`provision.rs`) and the second dispatch fetches again — the count assertion below
/// reads 2 and goes red. Watched fail exactly that way before this landed.
#[tokio::test]
async fn a_build_with_no_binary_provisions_once_and_the_next_build_reads_the_cache() {
    let dir = TempDir::new("prov-cache");
    let fetch = CountingFetch(std::sync::atomic::AtomicUsize::new(0));

    let transport = Arc::new(ScriptedTransport::new());
    script_prov_build(&transport);
    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Build(build_args_without_binary(dir.0.clone()));
    let (result, stderr) = dispatch_with_fetch(&seam, &command, full_infra(), &fetch).await;
    let rendered = result.expect("a provisioned build succeeds");

    assert_eq!(
        transport.called("CreateMicrovmImage"),
        1,
        "the build went out"
    );
    assert_eq!(fetch.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        rendered.data["agentd"]["source"], "fetched",
        "{:?}",
        rendered.data
    );
    assert_eq!(rendered.data["agentd"]["verified"], "attestation");
    assert!(
        stderr.contains("fetching the release asset"),
        "the fetch must be visible on stderr, not silent: {stderr}"
    );

    // The second invocation: same state dir, fresh transport script, and the count must
    // not move — a chain that re-fetched would make every build cost a download.
    let transport = Arc::new(ScriptedTransport::new());
    script_prov_build(&transport);
    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Build(build_args_without_binary(dir.0.clone()));
    let (result, _) = dispatch_with_fetch(&seam, &command, full_infra(), &fetch).await;
    let rendered = result.expect("a cached build succeeds");
    assert_eq!(
        fetch.0.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no second fetch"
    );
    assert_eq!(
        rendered.data["agentd"]["source"], "cache",
        "{:?}",
        rendered.data
    );
    assert_eq!(rendered.data["agentd"]["verified"], serde_json::Value::Null);
}

/// **A caller-supplied binary suppresses provisioning entirely** — the envelope's
/// `agentd` is null and the fetch seam is never consulted. Proven by routing through
/// [`dispatch_with`], whose fetcher panics on contact.
#[tokio::test]
async fn a_supplied_binary_never_consults_the_provisioning_chain() {
    let binary = FakeBinary::new("no-prov");
    let transport = Arc::new(ScriptedTransport::new());
    script_prov_build(&transport);
    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let mut args = build_args_without_binary(std::env::temp_dir());
    args.binary = Some(binary.0.clone());
    let (result, _) = dispatch_with(&seam, &Command::Build(args), full_infra()).await;
    let rendered = result.expect("a supplied binary builds");
    assert_eq!(
        rendered.data["agentd"],
        serde_json::Value::Null,
        "{:?}",
        rendered.data
    );
}

/// **`quickstart` is `run` — same preconditions, same refusals, same order.** With no
/// infrastructure configured it fails `run`'s own role check, locally, before the fetch
/// (the panicking fetcher proves the ordering) and before any AWS call (the refusing seam
/// proves that). A quickstart that fetched or called AWS before the cheap refusal would
/// spend a first-time user's seconds discovering what one env read already knew.
#[tokio::test]
async fn quickstart_refuses_missing_infrastructure_before_fetching_or_calling_aws() {
    let command = Command::Quickstart(crate::cli::QuickstartArgs {
        exec: "echo hello".into(),
        state_dir: None,
        region: region_flags(),
        infra: InfraFlags::default(),
    });
    let (result, _) = dispatch_with(&RefusingSeam::new(), &command, Infra::default()).await;
    let failure = result.expect_err("no roles configured");
    assert_eq!(failure.exit, Exit::Precondition);
    assert!(
        failure.message.contains("build_role_arn") || failure.message.contains("BUILD_ROLE"),
        "the refusal names the missing value: {}",
        failure.message
    );
}

/// A plain `build` (no `--reuse`) never touches the listing, and its envelope still
/// carries `reused: false` — the key is always present, so no consumer guards for it.
#[tokio::test]
async fn a_plain_build_never_lists_and_reports_reused_false() {
    let binary = FakeBinary::new("plain-build");
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .answer(
            "CreateMicrovmImage",
            201,
            r#"{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                 "name": "img", "state": "CREATING", "createdAt": 1754524800,
                 "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
                 "buildRoleArn": "arn:aws:iam::123456789012:role/build",
                 "codeArtifact": {"uri": "s3://a-bucket/img.zip"},
                 "imageVersion": "1"}"#,
        )
        .answer(
            "GetMicrovmImage",
            200,
            r#"{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                 "name": "img", "state": "CREATED", "createdAt": 1754524800}"#,
        );

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Build(BuildArgs {
        binary: Some(binary.0.clone()),
        state_dir: None,
        base_image_version: None,
        artifact_uri: None,
        name: Some("img".into()),
        memory: MemoryMib::Mib2048,
        dockerfile: None,
        repair_identity: false,
        log_group: None,
        log_stream: None,
        reuse: false,
        port: None,
        region: region_flags(),
        infra: InfraFlags::default(),
    });
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    let rendered = result.expect("builds");
    assert_eq!(
        transport.called("ListMicrovmImages"),
        0,
        "no --reuse, no listing"
    );
    assert_eq!(rendered.data["reused"], false);
    assert_eq!(rendered.data["imageName"], "img");
}

/// **Issue #47: a request core itself refuses costs zero transport calls — including the
/// S3 upload.** Both uploading paths, `build` and `run`, against a Dockerfile core's own
/// guards reject (no `CMD`, so the daemon would never start).
///
/// The guards always ran; the defect was ordering. `upload_artifact` came before
/// `build_image`, so a caller iterating on a refused Dockerfile paid one S3 PUT per
/// attempt for a rejection that was knowable locally. The contract is the one
/// `create_image`'s docs state: nothing billable before everything checkable is checked.
///
/// **Falsification** — run 2026-08-17. Swap `sandbox.preflight(&request)?` back below
/// `upload_artifact` in either path and that path's `uploads` assertion goes red with the
/// PUT recorded; the guard still refuses, so only this ordering test catches it.
#[tokio::test]
async fn a_locally_refused_dockerfile_costs_no_upload_and_no_call() {
    let dockerfile_path = std::env::temp_dir().join(format!(
        "microvm-guard-no-cmd-{}-{:?}.Dockerfile",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(
        &dockerfile_path,
        "FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal\nCOPY agentd /agentd\n",
    )
    .expect("writes");

    // The build path.
    let binary = FakeBinary::new("refused-build");
    let transport = Arc::new(ScriptedTransport::new());
    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Build(BuildArgs {
        binary: Some(binary.0.clone()),
        state_dir: None,
        base_image_version: None,
        artifact_uri: None,
        name: Some("refused".into()),
        memory: MemoryMib::Mib2048,
        dockerfile: Some(dockerfile_path.clone()),
        repair_identity: false,
        log_group: None,
        log_stream: None,
        reuse: false,
        port: None,
        region: region_flags(),
        infra: InfraFlags::default(),
    });
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    let error = result.expect_err("core refuses a Dockerfile with no CMD");
    assert_eq!(error.exit, Exit::InvalidArg, "{}", error.message);
    assert_eq!(
        transport.uploads(),
        Vec::<String>::new(),
        "build: the refused request must not cost the S3 PUT"
    );
    assert_eq!(transport.calls(), Vec::<String>::new(), "build: zero calls");

    // The run path's build arm.
    let binary = FakeBinary::new("refused-run");
    // A distinct label from the FakeBinary above: both helpers derive the same
    // `microvm-guard-<label>-<pid>-<tid>` path, and a shared label is a file/dir collision.
    let ledgers = TempDir::new("refused-run-ledger");
    let transport = Arc::new(ScriptedTransport::new());
    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let mut args = run_args_for_image("unused", ledgers.0.clone());
    args.image = None;
    args.binary = Some(binary.0.clone());
    args.dockerfile = Some(dockerfile_path.clone());
    let (result, _) = dispatch_with(&seam, &Command::Run(args), full_infra()).await;
    let error = result.expect_err("the run path refuses the same Dockerfile");
    assert_eq!(error.exit, Exit::InvalidArg, "{}", error.message);
    assert_eq!(
        transport.uploads(),
        Vec::<String>::new(),
        "run: the refused request must not cost the S3 PUT"
    );
    assert_eq!(transport.calls(), Vec::<String>::new(), "run: zero calls");

    let _ = std::fs::remove_file(&dockerfile_path);
}

/// **`build --base-image-version` reaches the `CreateMicrovmImage` body**, and its absence
/// emits nothing.
///
/// Read off the emitted body rather than off `BuildArgs`, which is the whole point: a field on
/// the args struct proves nothing about what got sent, and the wiring from flag to wire member
/// runs through three hops — `BuildArgs`, `BuildSpec`, `CreateImageRequest` — any of which
/// could drop it while every other test still passed.
///
/// **Guard proof.** Run 2026-08-16. Set `base_image_version: None` in `build`'s `BuildSpec`
/// (the flag parsed, the spec ignores it) and the pinned assertion goes red with the member
/// absent from the body; every other CLI test stays green, which is why this test exists.
#[tokio::test]
async fn a_pinned_base_image_version_reaches_the_create_body_from_the_build_flag() {
    let binary = FakeBinary::new("pinned-base");
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .answer(
            "CreateMicrovmImage",
            201,
            r#"{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                 "name": "img", "state": "CREATING", "createdAt": 1754524800,
                 "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
                 "buildRoleArn": "arn:aws:iam::123456789012:role/build",
                 "codeArtifact": {"uri": "s3://a-bucket/img.zip"},
                 "imageVersion": "1"}"#,
        )
        .answer(
            "GetMicrovmImage",
            200,
            r#"{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                 "name": "img", "state": "CREATED", "createdAt": 1754524800}"#,
        );

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Build(BuildArgs {
        binary: Some(binary.0.clone()),
        state_dir: None,
        // The managed base's versions are bare integers, measured 2026-08-16.
        base_image_version: Some("1".into()),
        artifact_uri: None,
        name: Some("img".into()),
        memory: MemoryMib::Mib2048,
        dockerfile: None,
        repair_identity: false,
        log_group: None,
        log_stream: None,
        reuse: false,
        port: None,
        region: region_flags(),
        infra: InfraFlags::default(),
    });
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    result.expect("builds");

    let body = transport.first_body("CreateMicrovmImage");
    assert_eq!(
        body["baseImageVersion"], "1",
        "the flag has to reach the wire, or a build still floats on the service default: {body}"
    );
    // Both are sent: pinning a version does not replace the base ARN.
    assert_eq!(
        body["baseImageArn"],
        "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1"
    );
}

/// **`build --log-group`/`--log-stream` reach the `CreateMicrovmImage` body with the
/// per-build discriminator applied, and the envelope reports the resolved exact stream.**
///
/// Read off the emitted body for the base-image-version test's reason: the wiring runs
/// through three hops — `BuildArgs`, `BuildSpec`, `CreateImageRequest` — any of which
/// could drop it while every other test stayed green. The discriminator claim is the
/// load-bearing one: the wire stream must be `<user value>/<16 hex>`, never verbatim,
/// because the member is an exact stream name and one build is three VMs writing three
/// streams (issue #98). And the envelope's `logStream` must equal the wire's byte for
/// byte — the nonce is minted inside core's create call, so the envelope is the only
/// place a caller can learn the name.
///
/// **Guard proof.** Run 2026-08-30. Set `log_stream: None` in `build`'s `BuildSpec` (the
/// flag parsed, the spec ignores it) and the body assertion goes red with no `logging`
/// member; every other CLI test stays green. Restored.
#[tokio::test]
async fn a_build_log_stream_reaches_the_wire_suffixed_and_the_envelope_reports_it() {
    let binary = FakeBinary::new("log-stream");
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .answer(
            "CreateMicrovmImage",
            201,
            r#"{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                 "name": "img", "state": "CREATING", "createdAt": 1754524800,
                 "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
                 "buildRoleArn": "arn:aws:iam::123456789012:role/build",
                 "codeArtifact": {"uri": "s3://a-bucket/img.zip"},
                 "imageVersion": "1"}"#,
        )
        .answer(
            "GetMicrovmImage",
            200,
            r#"{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                 "name": "img", "state": "CREATED", "createdAt": 1754524800}"#,
        );

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Build(BuildArgs {
        binary: Some(binary.0.clone()),
        state_dir: None,
        base_image_version: None,
        artifact_uri: None,
        name: Some("img".into()),
        memory: MemoryMib::Mib2048,
        dockerfile: None,
        repair_identity: false,
        log_group: Some("/aws/lambda-microvms/conformance-builds".into()),
        log_stream: Some("img-ci".into()),
        reuse: false,
        port: None,
        region: region_flags(),
        infra: InfraFlags::default(),
    });
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    let rendered = result.expect("builds");

    let body = transport.first_body("CreateMicrovmImage");
    assert_eq!(
        body["logging"]["cloudWatch"]["logGroup"], "/aws/lambda-microvms/conformance-builds",
        "the flag has to reach the wire: {body}"
    );
    let wire_stream = body["logging"]["cloudWatch"]["logStream"]
        .as_str()
        .expect("a stream was sent");
    assert_ne!(
        wire_stream, "img-ci",
        "the flag's value must never reach the wire verbatim — an exact stream name \
         collapses every build's three streams into one"
    );
    assert!(wire_stream.starts_with("img-ci/"), "{wire_stream}");
    let suffix = &wire_stream["img-ci/".len()..];
    assert_eq!(suffix.len(), 16, "{wire_stream}");
    assert!(
        suffix.bytes().all(|b| b.is_ascii_hexdigit()),
        "{wire_stream}"
    );

    // The envelope reports the resolved name, byte-identical to the wire's, plus the
    // configured group as buildLogGroup — not the derived default.
    assert_eq!(rendered.data["logStream"], wire_stream);
    assert_eq!(
        rendered.data["buildLogGroup"],
        "/aws/lambda-microvms/conformance-builds"
    );
}

/// A build with no logging flags emits **no** `logging` member and a null `logStream`
/// key: absent on the wire (byte-for-byte the request this CLI always sent), present as
/// null in the envelope (so a consumer never guards for the key).
#[tokio::test]
async fn a_build_without_logging_flags_emits_no_logging_member_and_a_null_stream() {
    let binary = FakeBinary::new("no-logging");
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .answer(
            "CreateMicrovmImage",
            201,
            r#"{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                 "name": "img", "state": "CREATING", "createdAt": 1754524800,
                 "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
                 "buildRoleArn": "arn:aws:iam::123456789012:role/build",
                 "codeArtifact": {"uri": "s3://a-bucket/img.zip"},
                 "imageVersion": "1"}"#,
        )
        .answer(
            "GetMicrovmImage",
            200,
            r#"{"imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                 "name": "img", "state": "CREATED", "createdAt": 1754524800}"#,
        );

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Build(BuildArgs {
        binary: Some(binary.0.clone()),
        state_dir: None,
        base_image_version: None,
        artifact_uri: None,
        name: Some("img".into()),
        memory: MemoryMib::Mib2048,
        dockerfile: None,
        repair_identity: false,
        log_group: None,
        log_stream: None,
        reuse: false,
        port: None,
        region: region_flags(),
        infra: InfraFlags::default(),
    });
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    let rendered = result.expect("builds");

    let body = transport.first_body("CreateMicrovmImage");
    assert!(
        body.get("logging").is_none(),
        "an unconfigured build must emit byte-for-byte what this CLI always sent: {body}"
    );
    assert_eq!(
        rendered.data["logStream"],
        serde_json::Value::Null,
        "the key is always present so a consumer never guards for it"
    );
    assert_eq!(
        rendered.data["buildLogGroup"], "/aws/lambda-microvms/img",
        "no configured group means the derived default"
    );
}

/// **`run --image-version` reaches the `RunMicrovm` body**, and its absence emits nothing.
///
/// The absence half matters for compatibility: an unpinned `run` has to emit byte-for-byte the
/// request this CLI always sent, so a `"imageVersion": null` on every launch would be a new
/// member on a request that has worked for months.
///
/// **Guard proof.** Run 2026-08-16. Drop `request.image_version = args.image_version.clone()`
/// from `launch_and_exec` and the pinned assertion goes red with the member absent; nothing
/// else in the suite notices, which is the gap this test closes.
#[tokio::test]
async fn a_pinned_image_version_reaches_the_run_body_from_the_run_flag() {
    let dir = TempDir::new("pinned-launch");
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .answer("RunMicrovm", 200, &microvm_body("PENDING"))
        .answer("GetMicrovm", 200, &microvm_body("TERMINATED"))
        .answer("TerminateMicrovm", 200, "{}")
        .answer(
            "CreateMicrovmAuthToken",
            200,
            r#"{"authToken": {"X-aws-proxy-auth": "opaque"}}"#,
        )
        .answer(
            "ListMicrovmImageVersions",
            200,
            r#"{"items": [{
                 "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
                 "buildRoleArn": "arn:aws:iam::123456789012:role/build",
                 "codeArtifact": {"uri": "s3://bucket/img.zip"},
                 "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                 "imageVersion": "1", "state": "SUCCESSFUL", "status": "ACTIVE",
                 "createdAt": 1754524800}]}"#,
        )
        .answer(
            "DeleteMicrovmImage",
            200,
            r#"{"imageIdentifier": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                 "state": "DELETING"}"#,
        );

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let mut args = run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
        dir.0.clone(),
    );
    args.image_version = Some("2.0".into());
    let command = Command::Run(args);
    // The launch **fails**, and that is deliberate rather than incidental. `GetMicrovm` answers
    // TERMINATED, so `wait_for_running` fails fast on TRAP-8 — which happens *after* `RunMicrovm`
    // emitted the body this test reads and *before* a session is built. Answering RUNNING instead
    // would send the CLI on to `wait_until_ready` against a daemon that does not exist, and that
    // retries: measured 2026-08-16, the same test took **240 seconds**. The body is emitted either
    // way, so the fast path is the honest one.
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    assert!(
        result.is_err(),
        "a VM that reports TERMINATED never reaches RUNNING, which is what makes this fast"
    );

    let body = transport.first_body("RunMicrovm");
    assert_eq!(
        body["imageVersion"], "2.0",
        "a canary has to launch against the version it means to test: {body}"
    );
    assert_eq!(
        body["imageIdentifier"], "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
        "pinning a version does not replace the identifier"
    );

    // And an unpinned run emits nothing for the member.
    let dir = TempDir::new("unpinned-launch");
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .answer("RunMicrovm", 200, &microvm_body("PENDING"))
        .answer("GetMicrovm", 200, &microvm_body("TERMINATED"))
        .answer("TerminateMicrovm", 200, "{}")
        .answer(
            "CreateMicrovmAuthToken",
            200,
            r#"{"authToken": {"X-aws-proxy-auth": "opaque"}}"#,
        );
    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Run(run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
        dir.0.clone(),
    ));
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    assert!(result.is_err(), "TERMINATED before RUNNING, as above");
    assert!(
        transport
            .first_body("RunMicrovm")
            .get("imageVersion")
            .is_none(),
        "an unpinned run must send what this CLI always sent: {}",
        transport.first_body("RunMicrovm")
    );
}

// ── the attached surfaces, against a scripted daemon ─────────────────────────
//
// `RefusingSeam` above answers the CLI-2 question — did the command go through the door — and
// answers nothing about what it *did* once through. The five attached commands need the second
// question, because each of them has a specific claim: `cp` sends bytes it did not inspect,
// `--stream` writes the envelope last, `stdin` surfaces a 409 as `Conflict`, `ack` maps a second
// 409 to the same code with a different detail, and `--exec-id` forwards the caller's key verbatim.
//
// So this section scripts the *daemon* rather than refusing at the seam:
// `Session::builder(..).with_backend(..)` is public, so a queue of canned HTTP replies is a real
// session over a fake wire. Every reply body below is a **literal** written from the protocol
// crate's own field names, for the reason `ScriptedTransport` gives above and the reason lesson #5
// in `.erpaval/solutions/test-failures/guards-that-passed-against-broken-code.md` gives: a fake
// built by calling the same serializer the code under test calls cannot disagree with it, and
// therefore cannot catch a shape error. These can.

/// A queue of canned HTTP replies, keeping every request that was sent.
///
/// A recorder rather than an assertion sink, matching core's own testing shape: the assertions live
/// at the call site where a reader can see them, not inside the fake where they are invisible.
struct DaemonScript {
    seen: Mutex<Vec<microvms_core::session::HttpRequest>>,
    replies: Mutex<std::collections::VecDeque<(u16, Vec<u8>)>>,
    /// Chunk sequences for `open_stream`, front to back.
    streams: Mutex<std::collections::VecDeque<(u16, Vec<Vec<u8>>)>>,
}

impl DaemonScript {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            replies: Mutex::new(std::collections::VecDeque::new()),
            streams: Mutex::new(std::collections::VecDeque::new()),
        })
    }

    /// Queues one non-streaming reply.
    fn reply(self: &Arc<Self>, status: u16, body: &str) -> Arc<Self> {
        self.replies
            .lock()
            .expect("not poisoned")
            .push_back((status, body.as_bytes().to_vec()));
        Arc::clone(self)
    }

    /// Queues one streaming reply: the head status, then these chunks in order.
    fn stream(self: &Arc<Self>, status: u16, chunks: Vec<Vec<u8>>) -> Arc<Self> {
        self.streams
            .lock()
            .expect("not poisoned")
            .push_back((status, chunks));
        Arc::clone(self)
    }

    fn requests(&self) -> Vec<microvms_core::session::HttpRequest> {
        self.seen.lock().expect("not poisoned").clone()
    }

    /// The paths that were requested, in order — the observable most assertions want.
    fn paths(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect()
    }
}

/// A chunk source over a queue, for the streaming replies.
struct Chunks(std::collections::VecDeque<Vec<u8>>);

impl microvms_core::session::ChunkSource for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, Error>> {
        Box::pin(async move { Ok(self.0.pop_front()) })
    }
}

impl microvms_core::session::HttpBackend for DaemonScript {
    fn send(
        &self,
        request: microvms_core::session::HttpRequest,
    ) -> BoxFuture<'_, Result<microvms_core::session::HttpResponse, Error>> {
        let described = format!("{} {}", request.method, request.path);
        self.seen.lock().expect("not poisoned").push(request);
        let reply = self
            .replies
            .lock()
            .expect("not poisoned")
            .pop_front()
            .unwrap_or_else(|| panic!("the script ran out of replies at {described}"));
        Box::pin(async move {
            Ok(microvms_core::session::HttpResponse {
                status: reply.0,
                headers: std::collections::HashMap::new(),
                body: reply.1,
            })
        })
    }

    fn open_stream(
        &self,
        request: microvms_core::session::HttpRequest,
        _idle_timeout: Duration,
    ) -> BoxFuture<'_, Result<microvms_core::session::OpenStream, Error>> {
        let described = format!("{} {}", request.method, request.path);
        self.seen.lock().expect("not poisoned").push(request);
        let (status, chunks) = self
            .streams
            .lock()
            .expect("not poisoned")
            .pop_front()
            .unwrap_or_else(|| panic!("the script ran out of stream replies at {described}"));
        Box::pin(async move {
            let head = microvms_core::session::HttpResponse {
                status,
                headers: std::collections::HashMap::new(),
                body: if (200..300).contains(&status) {
                    Vec::new()
                } else {
                    chunks.concat()
                },
            };
            let source: Box<dyn microvms_core::session::ChunkSource> =
                if (200..300).contains(&status) {
                    Box::new(Chunks(chunks.into_iter().collect()))
                } else {
                    Box::new(Chunks(std::collections::VecDeque::new()))
                };
            Ok((head, source))
        })
    }
}

/// A seam whose `attach_session` hands out a session over `script`.
///
/// The other three doors refuse: a test of an attached command that reached `open_sandbox` would be
/// a test of the wrong path, and a refusal says so loudly rather than succeeding quietly.
struct ScriptedSessionSeam {
    script: Arc<DaemonScript>,
}

impl CoreSeam for ScriptedSessionSeam {
    fn control_plane(&self, _region: Region) -> BoxFuture<'_, Result<ControlPlane, Error>> {
        Box::pin(async move {
            Err(Error::new(
                ErrorKind::Platform,
                "this guard attaches sessions only",
            ))
        })
    }

    fn open_sandbox(
        &self,
        _region: Region,
        _port: Option<u16>,
    ) -> BoxFuture<'_, Result<Sandbox, Error>> {
        Box::pin(async move {
            Err(Error::new(
                ErrorKind::Platform,
                "this guard attaches sessions only",
            ))
        })
    }

    fn attach_session(
        &self,
        _region: Region,
        _attach: Attach,
    ) -> BoxFuture<'_, Result<Session, Error>> {
        // No minter, so no proxy headers — which is the shape core documents for a daemon reached
        // directly and is exactly right here: TRAP-9's mint is core's own tested property, and
        // adding a fake minter would put a second thing in the way of what this guard is asking.
        let backend = Arc::clone(&self.script) as Arc<dyn microvms_core::session::HttpBackend>;
        let built = Session::builder("https://mvm-1.example", "agent-token")
            .with_backend(backend)
            .build();
        Box::pin(async move { built })
    }

    fn put_artifact(&self, _uri: &str, _bytes: Vec<u8>) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move { Ok(()) })
    }
}

/// Runs one attached command against `script`, returning the result and both streams.
async fn against_daemon(
    script: &Arc<DaemonScript>,
    command: &Command,
) -> (Result<Rendered, CliError>, String, String) {
    let seam = ScriptedSessionSeam {
        script: Arc::clone(script),
    };
    let mut out = Output::new(Format::Json, false, Vec::new(), Vec::new());
    let env = |_: &str| None;
    let result = {
        let mut ctx = Ctx {
            seam: &seam,
            out: &mut out,
            infra: full_infra(),
            env: &env,
            fetch: &crate::provision::PanickingFetch,
        };
        crate::handle(&mut ctx, command, crate::commands::lifecycle::never()).await
    };
    let (stdout, stderr) = out.into_streams();
    (
        result,
        String::from_utf8_lossy(&stdout).to_string(),
        String::from_utf8_lossy(&stderr).to_string(),
    )
}

/// An exec whose `--exec-id` and flags the caller chooses; everything else defaulted.
fn exec_command(shape: impl FnOnce(&mut ExecArgs)) -> Command {
    let mut args = ExecArgs {
        command: Some("true".into()),
        timeout: 30.0,
        cwd: None,
        env: Vec::new(),
        user: None,
        group: None,
        exec_id: None,
        poll: None,
        detach: false,
        stream: false,
        from_offset: None,
        stdin: false,
        attach: AttachFlags {
            state_dir: Some(std::env::temp_dir().join(format!(
                "microvm-guard-exec-history-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ))),
            ..attach_flags()
        },
        region: region_flags(),
    };
    shape(&mut args);
    Command::Exec(args)
}

/// `PollResponse`, in the protocol's own snake_case spelling, with the outcome **flattened**.
///
/// Written out rather than serialized from `microvms_core::protocol::exec::PollResponse`, which is the whole point:
/// a body produced by the same serializer the client deserializes with agrees with a renamed field
/// by construction. This one does not — and it earned its keep immediately. The first draft nested
/// the outcome under a `"result"` key, because that is what the Rust field is called. It is
/// `#[serde(flatten)]` (`protocol/src/exec.rs:195`), so on the wire those fields sit **beside**
/// `exec_id` and `phase` with no wrapper at all. Every exec assertion in this file was reading an
/// absent outcome, and the ack test is the one that noticed: it asserted on released output and got
/// `""`. That is lesson #5 in the guards solution note reproducing itself in one edit — a fake more
/// forgiving than the real parser hides exactly the bug it was written to find.
fn poll_body(phase: &str, exit_code: &str, stdout: &str, truncated: bool) -> String {
    format!(
        r#"{{"exec_id": "x-1", "phase": "{phase}", "exit_code": {exit_code},
             "signal": null, "stdout": "{stdout}", "stderr": "", "truncated": {truncated},
             "writers_may_be_alive": false}}"#
    )
}

/// A running exec's poll: no outcome fields at all, which is what a flattened `None` looks like.
///
/// Not `"result": null` — there is no `result` key on the wire. The distinction matters here more
/// than anywhere: `--poll`'s whole contract is rendering this shape as a success with a null exit
/// code, and a body with a `result` wrapper would deserialize to the same `None` by accident and
/// prove nothing about the real one.
const RUNNING_BODY: &str = r#"{"exec_id": "x-1", "phase": "running"}"#;

/// `StartResponse`.
const STARTED_BODY: &str = r#"{"exec_id": "x-1", "phase": "running"}"#;

/// One SSE `output` frame, base64 as the daemon writes it.
///
/// The base64 is **precomputed literal text** rather than encoded here, for the reason above: an
/// encoder call in the fake would produce whatever the decoder accepts. `Y2h1bmstMQo=` is
/// `chunk-1\n` and `Y2h1bmstMgo=` is `chunk-2\n`, checked by hand against the round trip below.
fn sse_output(offset: u64, encoded: &str) -> Vec<u8> {
    format!(
        "event: output\ndata: {{\"offset\":{offset},\"stream\":\"stdout\",\
         \"output\":\"{encoded}\"}}\n\n"
    )
    .into_bytes()
}

/// The terminal SSE frame.
fn sse_exit(code: i32, total: u64) -> Vec<u8> {
    format!(
        "event: exit\ndata: {{\"exit_code\":{code},\"signal\":null,\"truncated\":false,\
         \"writers_may_be_alive\":false,\"offset\":{total}}}\n\n"
    )
    .into_bytes()
}

/// **`exec --exec-id` sends the caller's key verbatim, and a retry sends the identical one.**
///
/// The property an idempotency key *is*. The daemon returns success for a known id without
/// spawning a second child (`agentd/src/exec.rs:366`, decided under the registry lock), so a retry
/// is safe only if the key on the wire is byte-identical — a CLI that prefixed, suffixed, or
/// namespaced it would address a different exec on the retry and spawn exactly the duplicate the
/// key exists to prevent. Asserted on the recorded request body rather than on the return value,
/// because the return value is the same either way.
///
/// **Guard proof.** Change `spec.exec_id.unwrap_or_else(..)` in `lifecycle::start_request` to
/// `mint_exec_id()` — dropping the caller's key — and both bodies below carry
/// generated ids: the first assertion goes red on the key, and the second on the two being equal.
#[tokio::test]
async fn a_supplied_exec_id_reaches_the_wire_unchanged_on_every_retry() {
    let mut sent: Vec<String> = Vec::new();
    for _ in 0..2 {
        let script = DaemonScript::new();
        script
            .reply(200, STARTED_BODY)
            .reply(200, &poll_body("exited", "0", "", false))
            // `wait_and_ack`: the poll reported `exited`, so an ack follows and carries the output.
            .reply(200, &poll_body("acked", "0", "", false));

        let command = exec_command(|args| args.exec_id = Some("conformance-retry-1".into()));
        let (result, _, _) = against_daemon(&script, &command).await;
        result.expect("the exec succeeds");

        let start = script
            .requests()
            .into_iter()
            .find(|request| request.path == "/v1/exec/start")
            .expect("a start went out");
        let body: serde_json::Value =
            serde_json::from_slice(&start.body).expect("the start body is JSON");
        assert_eq!(
            body["exec_id"], "conformance-retry-1",
            "the caller's idempotency key must reach the wire undecorated, or the retry addresses \
             a different exec and spawns a second child: {body}"
        );
        sent.push(body["exec_id"].as_str().expect("a string").to_string());
    }
    assert_eq!(
        sent[0], sent[1],
        "two invocations with the same --exec-id must send the same key; that identity is the \
         whole of what an idempotency key buys"
    );
}

/// **A generated exec id differs per invocation**, which is the other half of the same decision.
///
/// Without this the test above would pass against a CLI that ignored `--exec-id` and happened to
/// generate a constant — and a constant generated id is far worse than a wrong one: every exec in
/// a process would be answered from the first one's record.
#[tokio::test]
async fn two_invocations_without_an_exec_id_send_different_keys() {
    let mut sent: Vec<String> = Vec::new();
    for _ in 0..2 {
        let script = DaemonScript::new();
        script
            .reply(200, STARTED_BODY)
            .reply(200, &poll_body("exited", "0", "", false))
            .reply(200, &poll_body("acked", "0", "", false));
        let (result, _, _) = against_daemon(&script, &exec_command(|_| {})).await;
        result.expect("the exec succeeds");
        let start = script
            .requests()
            .into_iter()
            .find(|request| request.path == "/v1/exec/start")
            .expect("a start went out");
        let body: serde_json::Value = serde_json::from_slice(&start.body).expect("JSON");
        sent.push(body["exec_id"].as_str().expect("a string").to_string());
    }
    assert_ne!(
        sent[0], sent[1],
        "a constant generated id makes every exec after the first read the first one's output"
    );
}

/// **`exec --env`, `--user`, and `--group` reach the start body verbatim, and their absence is
/// an absence.**
///
/// Asserted on the recorded request body, because that is the only place the claim lives: the
/// daemon `env_clear()`s and applies exactly this map (`agentd/src/exec.rs:1003`), so a key
/// mangled between the flag and the wire is a variable the child silently does not have — the
/// PATH failure the coding-agents example documents, reintroduced through the fix. The second
/// invocation asserts the defaults stay defaults: `env` empty and `user`/`group` **null**, since
/// `Some(0)` where `None` belonged would ask the daemon to demote every exec to root.
///
/// **Guard proof.** Swap the tuple in `attached::exec`'s collection — `args.env.iter().map(|(k,
/// v)| (v.clone(), k.clone()))` — and the body carries `{"/usr/bin:/bin": "PATH"}`: the `env`
/// assertion goes red naming the missing key. Change `user: args.user` to `None` and the uid
/// assertion goes red. Both breaks were made on 2026-08-14, both failed exactly there, and both
/// were restored.
#[tokio::test]
async fn env_user_and_group_reach_the_wire_verbatim_and_default_to_absent() {
    let script = DaemonScript::new();
    script
        .reply(200, STARTED_BODY)
        .reply(200, &poll_body("exited", "0", "", false))
        .reply(200, &poll_body("acked", "0", "", false));

    let command = exec_command(|args| {
        args.env = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("EMPTY".into(), String::new()),
        ];
        args.user = Some(1000);
        args.group = Some(2000);
    });
    let (result, _, _) = against_daemon(&script, &command).await;
    result.expect("the exec succeeds");

    let start = script
        .requests()
        .into_iter()
        .find(|request| request.path == "/v1/exec/start")
        .expect("a start went out");
    let body: serde_json::Value =
        serde_json::from_slice(&start.body).expect("the start body is JSON");
    assert_eq!(
        body["env"]["PATH"], "/usr/bin:/bin",
        "the key must stay the key and the value the value; a swap is a variable the child \
         silently lacks: {body}"
    );
    assert_eq!(
        body["env"]["EMPTY"], "",
        "an empty value is set-to-empty, not unset: {body}"
    );
    assert_eq!(body["user"], 1000, "{body}");
    assert_eq!(body["group"], 2000, "{body}");

    // And without the flags, the wire says nothing: an empty map and nulls. `Some(0)` here
    // would demote every exec to root, which is the opposite of a default.
    let script = DaemonScript::new();
    script
        .reply(200, STARTED_BODY)
        .reply(200, &poll_body("exited", "0", "", false))
        .reply(200, &poll_body("acked", "0", "", false));
    let (result, _, _) = against_daemon(&script, &exec_command(|_| {})).await;
    result.expect("the exec succeeds");
    let start = script
        .requests()
        .into_iter()
        .find(|request| request.path == "/v1/exec/start")
        .expect("a start went out");
    let body: serde_json::Value = serde_json::from_slice(&start.body).expect("JSON");
    assert_eq!(
        body["env"],
        serde_json::json!({}),
        "no --env means an empty environment on the wire: {body}"
    );
    assert_eq!(body["user"], serde_json::Value::Null, "{body}");
    assert_eq!(body["group"], serde_json::Value::Null, "{body}");
}

/// **`exec --detach` starts and stops: one POST, no wait, and above all no ack.**
///
/// The flag exists because every other `exec` shape ends in `wait_and_ack`, and that ack is the
/// irreversible step — it releases the output, a second one is a 409, and a poll afterwards reports
/// `acked` with nothing. A caller who wants to own an exec's lifecycle needs a start that stops
/// after starting, and the live round proved it: the conformance driver could not decompose
/// start/poll/ack without one, so `ack accepted` got a 409 from the exec `exec` had already acked.
///
/// The assertion is the **request list**, because that is the only place the difference shows. A
/// `--detach` that quietly waited would return the same envelope shape on a fast command.
///
/// **Guard proof.** Delete the `if args.detach` block from `attached::exec` so it falls through to
/// `wait_and_ack`, and this goes red on the request list: `GET /v1/exec/x-1` and
/// `POST /v1/exec/x-1/ack` appear where only the start belongs.
#[tokio::test]
async fn a_detached_exec_starts_without_waiting_and_without_acking() {
    let script = DaemonScript::new();
    script
        .reply(200, STARTED_BODY)
        // Two replies a correct `--detach` never asks for, queued on purpose. Without them a
        // `--detach` that fell through to `wait_and_ack` would die on "the script ran out of
        // replies" — a red that names the *fake* rather than the defect. With them it gets as far
        // as acking, and the break lands on the request-list assertion below, which says exactly
        // what went wrong: an ack happened that the caller did not ask for and cannot undo.
        .reply(200, &poll_body("exited", "0", "", false))
        .reply(200, &poll_body("acked", "0", "", false));

    let command = exec_command(|args| {
        args.detach = true;
        args.exec_id = Some("x-1".into());
    });
    let (result, _, _) = against_daemon(&script, &command).await;
    let rendered = result.expect("a detached start succeeds");

    assert_eq!(
        script.paths(),
        ["POST /v1/exec/start"],
        "a detached exec is exactly one request: no poll, and no ack — the ack releases the output \
         and cannot be undone, so a caller who asked not to wait must not have acked: {:?}",
        script.paths()
    );
    // Reported as running, which is what the daemon just said. Not `exited`: a fast command may
    // already be done, and claiming a phase this process did not observe would hand a caller an
    // `exited` envelope with no output — indistinguishable from a command that produced none.
    assert_eq!(rendered.data["phase"], "running");
    assert_eq!(rendered.data["exitCode"], serde_json::Value::Null);
    assert_eq!(
        rendered.data["execId"], "x-1",
        "the id is the only handle a later poll or ack has, so it has to be in the envelope"
    );
    assert_eq!(
        rendered.already_reported, None,
        "starting successfully is a success; the workload's verdict is not known yet"
    );
}

/// **`exec --poll` is read-only: it sends one GET and never an ack.**
///
/// Two claims, and the second is the one that would be silently wrong. A `--poll` implemented as
/// `wait_and_ack` would return the same envelope on the happy path *and* release the output, so the
/// next `microvm ack` would 409 and the caller's own later read would find nothing. The assertion
/// is therefore on the request list rather than on the result.
///
/// **Guard proof.** Change `poll_existing`'s `session.exec(exec_id).poll()` to `.wait_and_ack(..)`
/// and the `POST /v1/exec/x-1/ack` assertion goes red while the returned envelope stays identical.
#[tokio::test]
async fn polling_an_exec_reads_it_without_acking_it() {
    let script = DaemonScript::new();
    script.reply(200, RUNNING_BODY);

    let command = exec_command(|args| {
        args.command = None;
        args.poll = Some("x-1".into());
    });
    let (result, _, _) = against_daemon(&script, &command).await;
    let rendered = result.expect("polling a running exec is a success, not a failure");

    assert_eq!(
        script.paths(),
        ["GET /v1/exec/x-1"],
        "a poll must be exactly one read: {:?}",
        script.paths()
    );
    assert_eq!(rendered.data["phase"], "running");
    assert_eq!(
        rendered.data["exitCode"],
        serde_json::Value::Null,
        "a running exec has no exit code, and reporting 0 would make an unfinished command look \
         like a passing one"
    );
    assert_eq!(
        rendered.already_reported, None,
        "polling is read-only and repeating it costs nothing, so `not finished yet` is an answer \
         rather than a non-zero exit"
    );
}

/// **`exec --stream` writes one NDJSON record per event and the envelope LAST.**
///
/// The documented exception, asserted the way `conformance/run_rs.py` asserts it: every line of
/// stdout before the last parses as an event, and the last parses as the envelope. Three ways this
/// can be wrong and all three are covered — the envelope first (a caller reading line by line hits
/// the terminator before any output), the envelope pretty-printed (it becomes seven broken
/// records), and the events absent (the whole point of streaming).
///
/// **Guard proof.** Remove the `if self.streaming && self.format.is_json()` early return from
/// `Output::emit` and the last line becomes pretty-printed JSON: `lines.len()` reads 9 instead of
/// 4 and the final-line parse fails. Delete the `ctx.out.stream_line(..)` call in `stream_exec` and
/// the event-count assertion goes red with stdout holding only the envelope.
#[tokio::test]
async fn a_streamed_exec_writes_ndjson_events_then_the_envelope_as_the_final_line() {
    let script = DaemonScript::new();
    script.reply(200, STARTED_BODY).stream(
        200,
        vec![
            sse_output(0, "Y2h1bmstMQo="),
            sse_output(8, "Y2h1bmstMgo="),
            sse_exit(0, 16),
        ],
    );

    let command = exec_command(|args| args.stream = true);
    let (result, stdout, _) = against_daemon(&script, &command).await;
    let rendered = result.expect("the stream completes");

    // The envelope the dispatcher would write, appended here the way `main` does — this guard
    // exercises the handler, so the final write is staged rather than assumed.
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "three events — two output frames and the exit — one line each: {stdout}"
    );

    // Every line is an event, in order, and the bytes are the child's.
    let events: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("an NDJSON line did not parse ({error}): {line:?}"))
        })
        .collect();
    assert_eq!(events[0]["event"], "output");
    assert_eq!(
        events[0]["text"], "chunk-1\n",
        "the base64 in the fake is literal text, so this also proves the decode: {}",
        events[0]
    );
    assert_eq!(events[0]["offset"], 0);
    assert_eq!(events[1]["text"], "chunk-2\n");
    assert_eq!(events[1]["offset"], 8);
    assert_eq!(events[2]["event"], "exit");
    assert_eq!(events[2]["exitCode"], 0);

    // The envelope's discriminant is the *streaming* one, so a consumer branching on `type` knows
    // which parse applied before it reads anything else.
    assert_eq!(rendered.kind, "microvm.exec.stream");
    assert_eq!(rendered.data["events"], 3);
    assert_eq!(rendered.data["bytes"], 16);
    assert_eq!(rendered.data["nextOffset"], 16);
    assert_eq!(rendered.data["gaps"], 0);
    assert_eq!(rendered.data["exitCode"], 0);
    assert_eq!(rendered.already_reported, None);
}

/// **The envelope really is the last line, written compact, through the real `Output`.**
///
/// Separate from the test above because that one asserts the *events* and this one asserts the
/// terminator — and the terminator is what `Output::emit`'s streaming branch is for. Written by
/// staging exactly what `main` does after a handler returns, so the pretty-versus-compact decision
/// under test is the shipped one.
#[test]
fn a_streams_envelope_is_one_compact_line_at_the_end_of_the_ndjson() {
    let mut out = Output::new(Format::Json, false, Vec::new(), Vec::new());
    out.stream_line(&serde_json::json!({"event": "output", "text": "a\n"}));
    out.stream_line(&serde_json::json!({"event": "exit", "exitCode": 0}));

    let mut data = serde_json::Map::new();
    data.insert("events".into(), serde_json::json!(2));
    out.emit(
        &crate::envelope::ok("microvm.exec.stream", data),
        "exit code: 0",
    );

    let stdout = String::from_utf8(out.into_streams().0).expect("utf8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "two events plus one envelope line; a pretty-printed envelope would be nine: {stdout}"
    );
    for (index, line) in lines.iter().enumerate() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|error| panic!("line {index} did not parse ({error}): {line:?}"));
    }
    let last: serde_json::Value =
        serde_json::from_str(lines[2]).expect("the last line is the envelope");
    assert_eq!(last["status"], "ok");
    assert_eq!(last["type"], "microvm.exec.stream");
    // And the two before it are not envelopes, so "the last one" is unambiguous.
    for line in &lines[..2] {
        let event: serde_json::Value = serde_json::from_str(line).expect("an event");
        assert!(
            event.get("status").is_none(),
            "an event must not look like an envelope: {line}"
        );
    }
}

/// **A stream that ends without an exit event reports failure rather than success.**
///
/// Core's own docs say the absence of the terminal event is the *only* thing distinguishing a cut
/// connection from a finished command. So a summary reporting `exitCode: 0` for a cut stream would
/// make a CI step pass on evidence it never received — and it is the plausible mistake, because
/// `Option::unwrap_or(0)` reads as a tidy default.
///
/// **Guard proof.** Change `data.insert("exitCode", json!(exit.and_then(..)))` to
/// `...unwrap_or(0)` and both the `exitCode` and the `already_reported` assertions go red.
///
/// `start_paused` because core's reconnect backoff tops out at four seconds and it makes twenty
/// attempts — a real clock spends about a minute here, which for one test in a hook-run suite is
/// the difference between a gate people run and one they skip. Tokio's auto-advance fires each
/// `sleep` the instant nothing else is runnable, so the *sequence* under test is unchanged.
#[tokio::test(start_paused = true)]
async fn a_cut_stream_reports_no_exit_code_and_earns_a_non_zero_exit() {
    let script = DaemonScript::new();
    script
        .reply(200, STARTED_BODY)
        // One output frame, then the body ends with no exit event. `reconnect` is on by default, so
        // core retries — the queue answers each attempt the same way until it is empty, which is
        // what a permanently cut stream looks like.
        .stream(200, vec![sse_output(0, "Y2h1bmstMQo=")]);
    for _ in 0..21 {
        script.stream(200, vec![]);
    }

    let command = exec_command(|args| args.stream = true);
    let (result, stdout, _) = against_daemon(&script, &command).await;

    // Core gives up after `max_reconnects` with a retryable error rather than a silent end, so this
    // surfaces as a failure — which is the honest outcome and is *also* fine for the property under
    // test: what must not happen is a success envelope claiming exit 0.
    match result {
        Err(failure) => {
            assert_eq!(
                failure.exit,
                Exit::Retryable,
                "a stream that kept dropping is retryable, not a passing command: {}",
                failure.message
            );
            // The one output frame it did deliver stays delivered: those bytes are real output the
            // caller received, and rewriting history would discard them.
            assert!(
                stdout.contains("chunk-1"),
                "events written before the cut must stay written: {stdout}"
            );
        }
        Ok(rendered) => {
            assert_eq!(
                rendered.data["exitCode"],
                serde_json::Value::Null,
                "a cut stream has no exit code; reporting 0 turns a truncated build into a green \
                 one: {:?}",
                rendered.data
            );
            assert_eq!(
                rendered.already_reported,
                Some(Exit::ExecFailed),
                "a stream with no terminal event must not exit 0"
            );
        }
    }
}

/// **`--from-offset` is the offset the stream request carries.**
///
/// The resume property, asserted on the request's query string. A `--from-offset` that parsed and
/// was then ignored would replay from zero: the caller would see every byte again, conclude the
/// resume worked, and have no way to notice — which is the failure mode E2B's cursorless
/// `connect(pid)` has and the reason core's cursor exists.
///
/// **Guard proof.** Change `stream_exec`'s `StreamOptions { offset, .. }` to
/// `StreamOptions::default()` and the offset assertion reads `offset=0`.
#[tokio::test]
async fn a_resume_offset_is_the_offset_the_stream_request_asks_for() {
    let script = DaemonScript::new();
    script.reply(200, STARTED_BODY).stream(
        200,
        vec![sse_output(4096, "Y2h1bmstMgo="), sse_exit(0, 4104)],
    );

    let command = exec_command(|args| {
        args.stream = true;
        args.from_offset = Some(4096);
    });
    let (result, _, _) = against_daemon(&script, &command).await;
    let rendered = result.expect("the resumed stream completes");

    let attach = script
        .requests()
        .into_iter()
        .find(|request| request.path.contains("/stream"))
        .expect("a stream attach went out");
    assert!(
        attach.path.contains("offset=4096"),
        "the resume offset must reach the wire, or the daemon replays from zero and the caller \
         cannot tell: {}",
        attach.path
    );
    // And the summary's `nextOffset` continues from there rather than from zero, so a second
    // resume is correct too.
    assert_eq!(rendered.data["nextOffset"], 4104);
}

/// **`microvm stdin` against an exec that never asked for it surfaces the daemon's 409 as
/// `Conflict`.**
///
/// The opt-in property. The daemon answers 409 `stdin_not_requested` because "the request is
/// well-formed, it is the exec that cannot accept it" (`agentd/src/exec.rs:700`), and that check
/// runs *before* the pipe lookup that answers 410 — so 409 is the status a caller reaches by
/// forgetting `--stdin`, and 410 is the one they reach by writing after EOF. Both collapse onto
/// `ERR_PROTOCOL`; `data.kind` is what separates them, which is why the assertion is on the wire
/// kind and not on the code.
///
/// **Guard proof.** Add a `WireKind::StdinClosed => WireKind::Conflict` remap anywhere on this
/// path and the 410 half of this test goes red while the 409 half stays green — the two really are
/// distinguished rather than coincidentally equal.
#[tokio::test]
async fn writing_stdin_to_an_exec_that_did_not_request_it_is_a_conflict_and_not_a_gone() {
    // The refusal: 409, because the exec was started without `stdin: true`.
    let refused = DaemonScript::new();
    refused.reply(
        409,
        r#"{"error": "stdin_not_requested", "detail": "this exec was started without stdin: true"}"#,
    );
    let command = Command::Stdin(StdinArgs {
        exec_id: "x-1".into(),
        data: Some("hello".into()),
        eof: false,
        attach: attach_flags(),
        region: region_flags(),
    });
    let (result, _, _) = against_daemon(&refused, &command).await;
    let failure = result.expect_err("an exec without a stdin pipe refuses the write");
    assert_eq!(failure.exit, Exit::Protocol);
    assert_eq!(failure.code(), "ERR_PROTOCOL");
    assert_eq!(
        failure.wire_kind,
        Some(microvms_core::WireKind::Conflict),
        "the opt-in refusal is 409/Conflict: the request is well-formed and it is the exec that \
         cannot accept it"
    );
    let envelope = crate::envelope::error(&failure);
    assert_eq!(envelope["data"]["kind"], "Conflict");

    // The other 409-adjacent case, which must NOT be the same kind: the pipe is gone, because an
    // earlier EOF closed it or the child exited. A CLI that mapped both onto one kind would make
    // "you forgot --stdin" indistinguishable from "you wrote too late".
    let gone = DaemonScript::new();
    gone.reply(410, r#"{"error": "stdin_closed"}"#);
    let (result, _, _) = against_daemon(&gone, &command).await;
    let failure = result.expect_err("a closed pipe refuses the write");
    assert_eq!(
        failure.wire_kind,
        Some(microvms_core::WireKind::StdinClosed),
        "410 is a different fact from 409 and the envelope has to say which"
    );
    assert_eq!(failure.exit, Exit::Protocol, "both share the coarse code");
}

/// **`microvm stdin` with neither data nor EOF is refused locally.**
///
/// The daemon answers 200 to a zero-byte write with no signal, which is worse than a refusal: the
/// caller reads the success as delivery. Refused before the call, so `data.kind` is absent — and
/// that absence is itself information, saying the CLI declined rather than the daemon.
#[tokio::test]
async fn a_stdin_write_with_nothing_to_write_is_refused_before_the_call() {
    let script = DaemonScript::new();
    let command = Command::Stdin(StdinArgs {
        exec_id: "x-1".into(),
        data: None,
        eof: false,
        attach: attach_flags(),
        region: region_flags(),
    });
    let (result, _, _) = against_daemon(&script, &command).await;
    let failure = result.expect_err("a write of nothing is not a write");
    assert_eq!(failure.exit, Exit::InvalidArg);
    assert_eq!(
        failure.wire_kind, None,
        "nothing reached the daemon, and that absence is what says the CLI refused"
    );
    assert!(
        script.requests().is_empty(),
        "a request went out for a write with no content: {:?}",
        script.paths()
    );
}

/// **`exec --stdin` writes the bytes and closes in one request, and asks for the pipe at start.**
///
/// Both halves matter and each fails differently. Without `stdin: true` on the start the daemon
/// gives the child `/dev/null` and the write is a 409. Without the EOF the child never sees end of
/// input — `cat` hangs until its timeout, and the daemon's copy of the pipe outlives the child's
/// own `wait()`, so nothing else would ever close it.
///
/// The EOF rides the *same* request as the final chunk, which is core's contract and is why there
/// are two requests here rather than three.
#[tokio::test]
async fn feeding_stdin_asks_for_the_pipe_at_start_and_closes_it_with_the_last_write() {
    let script = DaemonScript::new();
    script
        .reply(200, STARTED_BODY)
        .reply(200, r#"{"exec_id": "x-1", "written": 5, "eof": true}"#)
        .reply(200, &poll_body("exited", "0", "hello", false))
        .reply(200, &poll_body("acked", "0", "hello", false));

    // A pipe rather than the runner's stdin would be better, and is not reachable from here: the
    // handler reads `std::io::stdin()` directly. Under `cargo test` that is an empty or closed
    // descriptor, which reads as zero bytes — enough to exercise the ordering and the flags, which
    // is what this test is for. The byte-level round trip is a live check
    // (`stdin round-tripped through the child`).
    let command = exec_command(|args| args.stdin = true);
    let (result, _, _) = against_daemon(&script, &command).await;
    result.expect("the exec completes");

    let start = script
        .requests()
        .into_iter()
        .find(|request| request.path == "/v1/exec/start")
        .expect("a start went out");
    let body: serde_json::Value = serde_json::from_slice(&start.body).expect("JSON");
    assert_eq!(
        body["stdin"], true,
        "--stdin has to ask for the pipe at start time; the daemon cannot add one later and \
         answers 409 to the write: {body}"
    );

    let write = script
        .requests()
        .into_iter()
        .find(|request| request.path.ends_with("/stdin"))
        .expect("a stdin write went out");
    let body: serde_json::Value = serde_json::from_slice(&write.body).expect("JSON");
    assert_eq!(
        body["signal"], "eof",
        "the EOF must ride the write: nothing else closes the pipe, and a child blocked reading \
         stdin hangs until its timeout: {body}"
    );
}

/// **A second `ack` is a 409, and the two 409s carry different detail.**
///
/// The double-ack check the oracle ran. `agentd/src/exec.rs:854` states why it is not a 200 with an
/// empty body: that "would read as 'the command produced no output'". Both 409s here map to
/// `Conflict` — correctly, since a shell cannot act differently on them — and the *message* is what
/// distinguishes `already_acked` from `still_running`, which is the field a driver reads.
#[tokio::test]
async fn a_second_ack_conflicts_and_says_which_conflict_it_is() {
    let first = DaemonScript::new();
    first
        // The ack, carrying the released output.
        .reply(200, &poll_body("acked", "0", "output", false))
        // A second reply the correct implementation never asks for, queued on purpose. Without it
        // an `ack` that re-polled instead of returning the ack response would die on "the script
        // ran out of replies" — a real failure, but one that names the fake rather than the defect.
        // With it, the break lands on the `stdout` assertion below and says what is wrong: the
        // daemon released the output to the ack, and a poll after it reports `acked` with none, so
        // reading the wrong response is a silent empty-output bug.
        .reply(200, r#"{"exec_id": "x-1", "phase": "acked"}"#);
    let command = Command::Ack(AckArgs {
        exec_id: "x-1".into(),
        attach: attach_flags(),
        region: region_flags(),
    });
    let (result, _, _) = against_daemon(&first, &command).await;
    let rendered = result.expect("the first ack releases the output");
    assert_eq!(script_ack_path(&first), "POST /v1/exec/x-1/ack");
    assert_eq!(rendered.data["phase"], "acked");
    assert_eq!(
        rendered.data["stdout"], "output",
        "the ack response carries the released output; a poll after it reports none, so returning \
         the wrong one is a silent empty-output bug"
    );
    assert_eq!(
        rendered.already_reported, None,
        "an ack's own success is the release; the workload's code is in data.exitCode"
    );

    let second = DaemonScript::new();
    second.reply(
        409,
        r#"{"error": "already_acked", "detail": "output was released by an earlier ack"}"#,
    );
    let (result, _, _) = against_daemon(&second, &command).await;
    let failure = result.expect_err("the second ack is refused");
    assert_eq!(failure.wire_kind, Some(microvms_core::WireKind::Conflict));
    assert_eq!(failure.exit, Exit::Protocol);
    assert!(
        failure.message.contains("already_acked"),
        "the daemon's detail is what separates an already-acked 409 from a still-running one: {}",
        failure.message
    );

    // And the other 409 on the same route, so the two are not conflated: this one means the exec
    // has not exited and the output is still being written, which is a *wait* rather than a
    // handover that already happened.
    let running = DaemonScript::new();
    running.reply(
        409,
        r#"{"error": "still_running", "detail": "exec has not exited"}"#,
    );
    let (result, _, _) = against_daemon(&running, &command).await;
    let failure = result.expect_err("acking a running exec is refused");
    assert!(
        failure.message.contains("still_running"),
        "{}",
        failure.message
    );
}

/// The ack route one script saw. A helper so the assertion above reads as one line.
fn script_ack_path(script: &Arc<DaemonScript>) -> String {
    script
        .paths()
        .into_iter()
        .find(|path| path.contains("/ack"))
        .unwrap_or_default()
}

/// **`cp` hands the daemon the archive bytes unexamined, byte for byte.**
///
/// The confinement guard proof, and the assertion is a **byte scan** of the recorded request rather
/// than a success code — which is the oracle's own falsification for this property. A CLI that
/// validated the archive first would refuse the hostile ones locally, and the four conformance
/// checks would then pass against *this file's* copy of the member rules while the daemon's
/// confined extractor — the thing that actually runs in production — went untested. The plan names
/// that substitution explicitly.
///
/// The payload here is a traversal member (`../../escaped.txt`), which is the first of the oracle's
/// four hostile archives and the one whose refusal the live tier asserts.
///
/// **Guard proof.** Add any pre-flight member check to `cp`'s upload path — even one that only
/// rejects `..` — and the "the bytes went out unchanged" assertion goes red with no request
/// recorded at all.
#[tokio::test]
async fn a_tar_upload_sends_the_archive_bytes_unexamined_including_a_hostile_one() {
    // A tar header naming `../../escaped.txt`, hand-built so the bytes are the test's own rather
    // than a library's opinion of them. Only the fields the assertion depends on are meaningful;
    // this never reaches a real extractor, and the point is that the CLI does not look.
    let mut archive = vec![0u8; 512];
    archive[..16].copy_from_slice(b"../../escaped.tx");
    archive[257..262].copy_from_slice(b"ustar");
    let hostile = TempFile::new("hostile-tar", &archive);

    // The daemon refuses it, which is the outcome the live check asserts: a 400 arriving as
    // `ProtocolError`. What is under test here is that the CLI got far enough to be refused.
    let script = DaemonScript::new();
    script.reply(
        400,
        r#"{"error": "tar_member_refused", "detail": "member escapes the extraction root"}"#,
    );

    let command = Command::Cp(CpArgs {
        src: hostile.path().to_string_lossy().to_string(),
        dst: "vm:/tmp/hostile".into(),
        tar: true,
        mode: None,
        attach: attach_flags(),
        region: region_flags(),
    });
    let (result, _, _) = against_daemon(&script, &command).await;

    let failure = result.expect_err("the daemon refuses a hostile member");
    assert_eq!(
        failure.wire_kind,
        Some(microvms_core::WireKind::ProtocolError),
        "a refused member is a 400/ProtocolError; a 413 would mean merely too big, which is a \
         different fact"
    );
    assert_eq!(failure.exit, Exit::Protocol);

    // The load-bearing assertion. The recorded body is byte-identical to the file, so nothing
    // inspected, rewrote, or sanitized it — the daemon's extractor is the only guard in the path.
    let sent = script
        .requests()
        .into_iter()
        .find(|request| request.path.starts_with("/v1/fs/tar"))
        .expect("the archive reached the tar route rather than being refused locally");
    assert_eq!(
        sent.body, archive,
        "the CLI must hand core the bytes it was given: a pre-flight check here would make the \
         hostile-archive checks test this file's copy of the member rules instead of the daemon's"
    );
    assert_eq!(sent.method, "PUT");
}

/// **`cp` names the direction from the argument, and a single-file upload carries its mode.**
///
/// The mode is octal **as a string** on the wire, which is the daemon's contract: `"644"` and
/// `"0644"` mean the same mode, and an integer would be read as decimal 644 by anything that
/// stringifies it. Asserted on the query string, because that is where it goes.
#[tokio::test]
async fn a_single_file_upload_uses_the_file_route_and_carries_its_octal_mode() {
    let payload = TempFile::new("cp-upload", b"written through the endpoint");
    let script = DaemonScript::new();
    script.reply(200, "");

    let command = Command::Cp(CpArgs {
        src: payload.path().to_string_lossy().to_string(),
        dst: "vm:/tmp/live.txt".into(),
        tar: false,
        mode: Some("0644".into()),
        attach: attach_flags(),
        region: region_flags(),
    });
    let (result, _, _) = against_daemon(&script, &command).await;
    let rendered = result.expect("the upload succeeds");

    let sent = script.requests().pop().expect("a request went out");
    assert_eq!(sent.method, "PUT");
    assert!(
        sent.path.starts_with("/v1/fs/file?"),
        "a single file goes to the file route, not the tar one: {}",
        sent.path
    );
    assert!(
        sent.path.contains("mode=0644"),
        "the mode is octal as a string on the wire: {}",
        sent.path
    );
    assert_eq!(sent.body, b"written through the endpoint");
    assert_eq!(rendered.data["direction"], "upload");
    assert_eq!(rendered.data["bytes"], 28);
}

/// **`cp vm:/path ./local` writes the bytes to disk, unmodified, including non-UTF-8 ones.**
///
/// The download half, and the non-UTF-8 payload is the case worth having: a `cp` that went through
/// a string anywhere would corrupt a tarball or a binary, and the corruption would be invisible
/// until someone tried to use the file.
#[tokio::test]
async fn a_download_writes_the_raw_bytes_to_the_local_path() {
    let bytes = vec![0xffu8, 0x00, 0xfe, b'a', b'\n'];
    let dir = TempDir::new("cp-download");
    let local = dir.0.join("nested").join("out.bin");

    let script = DaemonScript::new();
    script
        .replies
        .lock()
        .expect("not poisoned")
        .push_back((200, bytes.clone()));

    let command = Command::Cp(CpArgs {
        src: "vm:/tmp/bin".into(),
        dst: local.to_string_lossy().to_string(),
        tar: false,
        mode: None,
        attach: attach_flags(),
        region: region_flags(),
    });
    let (result, _, _) = against_daemon(&script, &command).await;
    let rendered = result.expect("the download succeeds");

    assert_eq!(
        std::fs::read(&local).expect("the file was written"),
        bytes,
        "a download that went through a string would corrupt a tarball invisibly"
    );
    assert_eq!(rendered.data["direction"], "download");
    assert_eq!(rendered.data["bytes"], 5);
    assert_eq!(script.requests().pop().expect("a request").method, "GET");
}

/// **`microvm health` reports the two identity flags and warns about a degraded one.**
///
/// The three facts no other command reports. `identityDegraded` is the one with a measurement
/// behind it: without `additionalOsCapabilities: ["ALL"]` the hostname and boot_id steps fail with
/// EPERM even as root, and this flag is how that surfaces — asserting it here is what makes the
/// capability requirement impossible to drop by accident.
///
/// Exit 0 despite the warning, and that is deliberate: the daemon's own contract is that a degraded
/// identity "is never a reason for the daemon to refuse to serve". A non-zero exit would tell a
/// caller their VM is broken when what is true is that an operator may want to drain it.
#[tokio::test]
async fn health_reports_the_identity_flags_and_warns_without_failing_on_a_degraded_one() {
    let script = DaemonScript::new();
    script.reply(
        200,
        r#"{"version": "0.1.0", "bootstrapped": true,
             "disk": {"available_bytes": 1024, "reserve_bytes": 4096, "under_pressure": true},
             "identity_degraded": true, "identity_repaired": true,
             "busy": true, "execs": 2}"#,
    );

    let command = Command::Health(HealthArgs {
        attach: attach_flags(),
        region: region_flags(),
    });
    let (result, _, stderr) = against_daemon(&script, &command).await;
    let rendered = result.expect("a degraded identity is reported, not raised");

    assert_eq!(script.paths(), ["GET /v1/health"]);
    assert_eq!(rendered.data["identityDegraded"], true);
    assert_eq!(rendered.data["identityRepaired"], true);
    assert_eq!(rendered.data["bootstrapped"], true);
    assert_eq!(rendered.data["diskUnderPressure"], true);
    assert_eq!(rendered.data["diskAvailableBytes"], 1024);
    // The activity pair an orchestrator polls on a loop, to decide whether to keep the VM
    // alive. Its own poll is the inbound traffic the platform's idle policy measures —
    // which is the only kind that counts, because the endpoint proxy terminates outside
    // the guest and an in-guest keepalive never reaches it.
    assert_eq!(rendered.data["busy"], true);
    assert_eq!(rendered.data["execs"], 2);
    assert_eq!(
        rendered.already_reported, None,
        "the daemon serves a degraded identity by design; failing here would report a working VM \
         as broken"
    );
    // Warnings rather than progress, so `--quiet` cannot buy silence about either.
    assert!(
        stderr.contains("warning: identityDegraded"),
        "a duplicate machine-id is a condition an operator has to be told about: {stderr}"
    );
    assert!(stderr.contains("warning: diskUnderPressure"), "{stderr}");
}

/// A daemon that has not bootstrapped is a success envelope with a non-zero code.
///
/// Reachable only from inside the VM or over a tunnel — the platform forwards no external traffic
/// until the run hook returns 200 — and a real answer when it happens: the daemon is up and the
/// token is not installed, which needs a different remedy from a dead VM. `null` disk, so the
/// unmeasurable case is covered too: it is distinct from zero, and a monitor that conflated them
/// would page on a missing `statvfs`.
#[tokio::test]
async fn an_unbootstrapped_daemon_is_reported_with_a_non_zero_code_and_a_null_disk() {
    let script = DaemonScript::new();
    script.reply(
        200,
        r#"{"version": "0.1.0", "bootstrapped": false, "disk": null,
             "identity_degraded": false, "identity_repaired": false}"#,
    );
    let command = Command::Health(HealthArgs {
        attach: attach_flags(),
        region: region_flags(),
    });
    let (result, _, _) = against_daemon(&script, &command).await;
    let rendered = result.expect("the daemon answered, so this is a report");
    assert_eq!(rendered.data["bootstrapped"], false);
    assert_eq!(
        rendered.data["diskAvailableBytes"],
        serde_json::Value::Null,
        "unmeasurable is not full, and zero would page a monitor on a missing statvfs"
    );
    assert_eq!(rendered.already_reported, Some(Exit::Platform));
    assert!(
        rendered.text.contains("repair switched off"),
        "identity_repaired: false means opted out, which is not the same as `nothing to do`: {}",
        rendered.text
    );
}

/// A file that cleans itself up, for the two `cp` tests that need real bytes on disk.
struct TempFile(std::path::PathBuf, #[allow(dead_code)] tempfile::TempPath);

impl TempFile {
    fn new(label: &str, bytes: &[u8]) -> Self {
        let file = tempfile::Builder::new()
            .prefix(&format!("microvm-guard-{label}-"))
            .tempfile()
            .expect("a temp file");
        std::fs::write(file.path(), bytes).expect("writes");
        let path = file.into_temp_path();
        Self(path.to_path_buf(), path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

// ── the per-VM history, through the shipped handlers ─────────────────────────
//
// `src/history.rs`'s own tests prove the module; these prove the *wiring* — that the handlers
// really append, with the platform's values, and that the record survives the command that
// wrote it. A history module that worked perfectly and was never called would pass every unit
// test and record nothing.

/// **`terminate` appends a `terminated` event carrying the teardown's own verdict.**
///
/// The values are the platform's: `terminateAccepted` reflects whether the call was accepted,
/// and the file survives the terminate — which is the whole reason history is not the ledger.
///
/// **Guard proof.** Delete the `History::for_vm(..).append(Event::Terminated {..})` block from
/// `lifecycle::terminate` and the read below is empty; the command's envelope and exit are
/// unchanged, which is why only this test catches it. Broken exactly so on 2026-08-25
/// (block commented out, test red on `read.len()`), then restored.
#[tokio::test]
async fn a_taken_vm_name_is_refused_before_any_door_with_its_own_row() {
    // The acceptance criterion, verbatim: collision on reuse of a live name is a local
    // refusal with a stable ERR_* code and **zero billable calls**. The seam is the
    // RefusingSeam, so any AWS reach shows up as an entered door — and the assertion below
    // is that none was.
    let dir = TempDir::new("name-collision");
    crate::ledger::Names::new(&dir.0)
        .register(&crate::ledger::NameRecord {
            name: "ci-runner".into(),
            microvm_id: "mvm-live".into(),
            endpoint: "https://mvm-live.example".into(),
            agent_token: "tok".into(),
            region: "us-east-1".into(),
            at: 1,
            identity_host_seed: None,
            identity_vm_public_key: None,
        })
        .expect("registers");

    let seam = RefusingSeam::new();
    let mut args = run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
        dir.0.clone(),
    );
    args.keep = true;
    args.vm_name = Some("ci-runner".into());
    let (result, _) = dispatch_with(&seam, &Command::Run(args), full_infra()).await;

    let failure = result.expect_err("a taken name is a refusal");
    assert_eq!(failure.exit, Exit::NameTaken);
    assert_eq!(failure.code(), "ERR_NAME_TAKEN");
    assert_eq!(failure.exit.as_u8(), 14);
    assert!(
        failure.message.contains("mvm-live"),
        "the holder is named, so the remedy is actionable: {}",
        failure.message
    );
    assert_eq!(
        seam.doors(),
        Vec::<Door>::new(),
        "the refusal must cost zero AWS calls — no door may have been entered"
    );

    // And an illegal name is the *other* row: fixed by editing the flag, not by a terminate.
    let seam = RefusingSeam::new();
    let mut args = run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
        dir.0.clone(),
    );
    args.keep = true;
    args.vm_name = Some("mvm-lookalike".into());
    let (result, _) = dispatch_with(&seam, &Command::Run(args), full_infra()).await;
    let failure = result.expect_err("an id-shaped name is refused");
    assert_eq!(failure.exit, Exit::InvalidArg);
    assert_eq!(seam.doors(), Vec::<Door>::new(), "still before any door");
}

/// **A registered name substitutes for the id on the lifecycle wire, and a raw-id terminate
/// frees it.**
///
/// The suspend asserts the substitution where it matters — the `GetMicrovm` path the state
/// read hits carries `mvm-live`, never the name. The terminate then addresses the same VM by
/// its raw id and must release the registration anyway, because a registry that keeps
/// claiming a name for a dead VM turns every later `--vm-name` into a false collision.
#[tokio::test]
async fn a_name_resolves_on_the_lifecycle_wire_and_a_terminate_by_id_frees_it() {
    let dir = TempDir::new("name-lifecycle");
    crate::ledger::Names::new(&dir.0)
        .register(&crate::ledger::NameRecord {
            name: "ci-runner".into(),
            microvm_id: "mvm-live".into(),
            endpoint: "https://mvm-live.example".into(),
            agent_token: "tok".into(),
            region: "us-east-1".into(),
            at: 1,
            identity_host_seed: None,
            identity_vm_public_key: None,
        })
        .expect("registers");

    // suspend by name: the wire carries the id.
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .answer(
            "GetMicrovm",
            200,
            r#"{"microvmId": "mvm-live", "state": "RUNNING",
                 "endpoint": "https://mvm-live.example",
                 "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                 "imageVersion": "1", "maximumDurationInSeconds": 3600, "startedAt": 1}"#,
        )
        .answer("SuspendMicrovm", 200, "{}");
    // The post-suspend wait re-reads the state; the second GetMicrovm answer repeats, so the
    // wait sees RUNNING forever — script SUSPENDED as the settled answer instead.
    transport.answer(
        "GetMicrovm",
        200,
        r#"{"microvmId": "mvm-live", "state": "SUSPENDED",
             "endpoint": "https://mvm-live.example",
             "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
             "imageVersion": "1", "maximumDurationInSeconds": 3600, "startedAt": 1}"#,
    );
    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Suspend(SuspendArgs {
        microvm_id: "ci-runner".into(),
        timeout: 30.0,
        state_dir: Some(dir.0.clone()),
        region: region_flags(),
    });
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    result.expect("the suspend succeeds through the name");
    let suspends = transport.paths_of("SuspendMicrovm");
    assert!(
        suspends[0].contains("mvm-live") && !suspends[0].contains("ci-runner"),
        "the wire must carry the resolved id, never the local name: {}",
        suspends[0]
    );

    // terminate by raw id: the name is freed.
    let transport = Arc::new(ScriptedTransport::new());
    transport.answer("TerminateMicrovm", 200, "{}");
    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Terminate(TerminateArgs {
        microvm_id: "mvm-live".into(),
        image_identifier: None,
        image_name: None,
        delete_image: false,
        wait: false,
        state_dir: Some(dir.0.clone()),
        region: region_flags(),
    });
    let (result, stderr) = dispatch_with(&seam, &command, full_infra()).await;
    result.expect("the terminate succeeds");
    assert!(
        crate::ledger::Names::new(&dir.0)
            .lookup("ci-runner")
            .is_none(),
        "a terminate by raw id must free the name its VM held"
    );
    assert!(
        stderr.contains("released name ci-runner"),
        "the release is said, so the operator knows the name is reusable: {stderr}"
    );

    // An unknown name on the same surface fails locally, before any call.
    let transport = Arc::new(ScriptedTransport::new());
    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Suspend(SuspendArgs {
        microvm_id: "never-registered".into(),
        timeout: 30.0,
        state_dir: Some(dir.0.clone()),
        region: region_flags(),
    });
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    let failure = result.expect_err("an unknown name has nothing to suspend");
    assert_eq!(failure.exit, Exit::Precondition);
    assert_eq!(
        transport.calls(),
        Vec::<String>::new(),
        "the miss is local: {:?}",
        transport.calls()
    );
}

/// **`exec --name` attaches with the registered record's triple.**
///
/// Asserted at the seam, which is where the substitution is observable: the `Attach` the
/// handler passes carries the record's endpoint, token, and id, and the caller typed none of
/// them.
#[tokio::test]
async fn an_attached_command_by_name_carries_the_registered_triple() {
    let dir = TempDir::new("name-attach");
    crate::ledger::Names::new(&dir.0)
        .register(&crate::ledger::NameRecord {
            name: "ci-runner".into(),
            microvm_id: "mvm-named".into(),
            endpoint: "https://mvm-named.example".into(),
            agent_token: "tok-named".into(),
            region: "us-west-2".into(),
            at: 1,
            identity_host_seed: None,
            identity_vm_public_key: None,
        })
        .expect("registers");

    /// A seam that records the `Attach` it was handed and then refuses.
    struct AttachRecorder {
        seen: Mutex<Vec<(Attach, String)>>,
    }
    impl CoreSeam for AttachRecorder {
        fn control_plane(&self, _region: Region) -> BoxFuture<'_, Result<ControlPlane, Error>> {
            panic!("an attached command never opens a control plane directly")
        }
        fn open_sandbox(
            &self,
            _region: Region,
            _port: Option<u16>,
        ) -> BoxFuture<'_, Result<Sandbox, Error>> {
            panic!("an attached command never opens a sandbox")
        }
        fn attach_session(
            &self,
            region: Region,
            attach: Attach,
        ) -> BoxFuture<'_, Result<Session, Error>> {
            self.seen
                .lock()
                .expect("not poisoned")
                .push((attach, region.as_str().to_string()));
            Box::pin(async move { Err(Error::new(ErrorKind::Platform, "recorded; stopping")) })
        }
        fn put_artifact(&self, _uri: &str, _bytes: Vec<u8>) -> BoxFuture<'_, Result<(), Error>> {
            panic!("no artifact on this path")
        }
    }

    let seam = AttachRecorder {
        seen: Mutex::new(Vec::new()),
    };
    let command = Command::Health(HealthArgs {
        attach: AttachFlags {
            endpoint: None,
            agent_token: None,
            microvm_id: None,
            name: Some("ci-runner".into()),
            port: None,
            state_dir: Some(dir.0.clone()),
        },
        region: RegionFlags::default(),
    });
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    result.expect_err("the recorder refuses after recording");

    let seen = seam.seen.lock().expect("not poisoned");
    assert_eq!(seen.len(), 1, "exactly one attach was attempted");
    let (attach, region) = &seen[0];
    assert_eq!(attach.endpoint, "https://mvm-named.example");
    assert_eq!(attach.agent_token, "tok-named");
    assert_eq!(attach.microvm_id, "mvm-named");
    assert_eq!(
        region, "us-west-2",
        "with no --region flag, the record's launch region is the default"
    );
}

#[tokio::test]
async fn a_terminate_appends_a_terminated_event_that_survives_the_command() {
    let dir = TempDir::new("history-terminate");
    let transport = Arc::new(ScriptedTransport::new());
    transport.answer("TerminateMicrovm", 200, "{}");

    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Terminate(TerminateArgs {
        microvm_id: "mvm-1".into(),
        image_identifier: None,
        image_name: None,
        delete_image: false,
        wait: false,
        state_dir: Some(dir.0.clone()),
        region: region_flags(),
    });
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    result.expect("the terminate succeeds");

    let read = crate::history::read_events(&dir.0, "mvm-1");
    assert_eq!(read.len(), 1, "the handler must append: {read:?}");
    assert_eq!(read[0]["event"], "terminated");
    assert_eq!(read[0]["terminateAccepted"], true);
    assert_eq!(read[0]["undeleted"], serde_json::json!([]));
    assert_eq!(read[0]["seq"], 0);

    // And a terminate whose call is refused records that verdict rather than a clean one:
    // the failed teardown is exactly the run a caller wants the record of.
    let transport = Arc::new(ScriptedTransport::new());
    transport.answer(
        "TerminateMicrovm",
        409,
        r#"{"message": "ConflictException"}"#,
    );
    let seam = ScriptedSeam {
        transport: Arc::clone(&transport),
        clock: Arc::new(YieldingClock::default()),
    };
    let command = Command::Terminate(TerminateArgs {
        microvm_id: "mvm-1".into(),
        image_identifier: None,
        image_name: None,
        delete_image: false,
        wait: false,
        state_dir: Some(dir.0.clone()),
        region: region_flags(),
    });
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    result.expect("a failed teardown still reports rather than raising");

    let read = crate::history::read_events(&dir.0, "mvm-1");
    assert_eq!(read.len(), 2, "the second append continues the sequence");
    assert_eq!(
        read[1]["seq"], 1,
        "counting the file is what makes two processes one sequence"
    );
    assert_eq!(read[1]["terminateAccepted"], false);
    assert_eq!(read[1]["undeleted"], serde_json::json!(["mvm-1"]));
}

/// **`exec` appends an `exec` event with the daemon's own report, and `--detach` appends one
/// with a null exit code.**
///
/// The null is the honest half: a detached start does not know the outcome, and a record
/// claiming one would be a record this process never observed. The waited exec's fields are
/// read off the daemon's poll body, never off anything the child printed.
///
/// **Guard proof.** Delete the `history.append(Event::Exec {..})` after `wait_and_ack` in
/// `attached::exec` and the first read below is empty while the envelope is byte-identical.
#[tokio::test]
async fn an_exec_appends_the_daemons_report_and_a_detached_one_appends_a_null_code() {
    let dir = TempDir::new("history-exec");
    let script = DaemonScript::new();
    script
        .reply(200, STARTED_BODY)
        .reply(200, &poll_body("exited", "4", "out", true))
        .reply(200, &poll_body("acked", "4", "out", true));
    let command = exec_command(|args| {
        args.exec_id = Some("x-1".into());
        args.attach.state_dir = Some(dir.0.clone());
    });
    let (result, _, _) = against_daemon(&script, &command).await;
    result.expect("the exec completes");

    let read = crate::history::read_events(&dir.0, "mvm-1");
    assert_eq!(read.len(), 1, "{read:?}");
    assert_eq!(read[0]["event"], "exec");
    assert_eq!(read[0]["execId"], "x-1");
    assert_eq!(
        read[0]["exitCode"], 4,
        "the daemon's code, not a success default"
    );
    assert_eq!(read[0]["truncated"], true);
    assert_eq!(read[0]["writersMayBeAlive"], false);

    // The detached shape: started, not waited, so the outcome is honestly unknown.
    let script = DaemonScript::new();
    script.reply(200, STARTED_BODY);
    let command = exec_command(|args| {
        args.detach = true;
        args.exec_id = Some("x-1".into());
        args.attach.state_dir = Some(dir.0.clone());
    });
    let (result, _, _) = against_daemon(&script, &command).await;
    result.expect("a detached start succeeds");

    let read = crate::history::read_events(&dir.0, "mvm-1");
    assert_eq!(read.len(), 2);
    assert_eq!(read[1]["event"], "exec");
    assert_eq!(
        read[1]["exitCode"],
        serde_json::Value::Null,
        "a detached start has no outcome to record: {read:?}"
    );
}

/// **`resume` polls the thawed daemon and lands its hook observations in history.**
/// (issue #80)
///
/// This is the moment suspend-hook firings become visible at all — a frozen VM cannot
/// answer a poll — so the wiring deserves its own guard: delete the post-RUNNING
/// health poll from `lifecycle::resume` and the hook read below is empty while the
/// resume's envelope and exit are byte-identical, which is why nothing else catches it.
#[tokio::test]
async fn a_resume_polls_the_thawed_daemon_and_lands_its_hook_observations() {
    /// `ScriptedSeam` for the control plane, `DaemonScript` for the attach the
    /// post-RUNNING poll makes — `resume` is the one lifecycle command that uses both.
    struct ResumeSeam {
        transport: Arc<ScriptedTransport>,
        clock: Arc<YieldingClock>,
        daemon: Arc<DaemonScript>,
    }
    impl CoreSeam for ResumeSeam {
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
            _region: Region,
            _port: Option<u16>,
        ) -> BoxFuture<'_, Result<Sandbox, Error>> {
            Box::pin(async move { Err(Error::new(ErrorKind::Platform, "resume never launches")) })
        }
        fn attach_session(
            &self,
            _region: Region,
            _attach: Attach,
        ) -> BoxFuture<'_, Result<Session, Error>> {
            let backend = Arc::clone(&self.daemon) as Arc<dyn microvms_core::session::HttpBackend>;
            let built = Session::builder("https://mvm-1.example", "")
                .with_backend(backend)
                .build();
            Box::pin(async move { built })
        }
        fn put_artifact(&self, _uri: &str, _bytes: Vec<u8>) -> BoxFuture<'_, Result<(), Error>> {
            Box::pin(async move { Ok(()) })
        }
    }

    let dir = TempDir::new("history-resume-hooks");
    let transport = Arc::new(ScriptedTransport::new());
    transport.answer("ResumeMicrovm", 200, "{}").answer(
        "GetMicrovm",
        200,
        &microvm_body("RUNNING"),
    );
    let daemon = DaemonScript::new();
    daemon.reply(
        200,
        r#"{"version": "0.1.0", "bootstrapped": true, "disk": null,
             "identity_degraded": false, "identity_repaired": true,
             "hooks": [{"hook": "suspend", "fired_at": 1756500500},
                       {"hook": "resume", "fired_at": 1756500600}],
             "hooks_dropped": 0}"#,
    );
    let seam = ResumeSeam {
        transport,
        clock: Arc::new(YieldingClock::default()),
        daemon: Arc::clone(&daemon),
    };
    let command = Command::Resume(ResumeArgs {
        microvm_id: "mvm-abc123".into(),
        timeout: 30.0,
        state_dir: Some(dir.0.clone()),
        region: region_flags(),
    });
    let (result, _) = dispatch_with(&seam, &command, full_infra()).await;
    result.expect("the resume succeeds");

    assert_eq!(
        daemon.paths(),
        ["GET /v1/health"],
        "one poll, after RUNNING"
    );
    let read = crate::history::read_events(&dir.0, "mvm-abc123");
    let hooks: Vec<(&str, u64)> = read
        .iter()
        .filter(|event| event["event"] == "hookObserved")
        .map(|event| {
            (
                event["hook"].as_str().expect("a hook"),
                event["firedAt"].as_u64().expect("an epoch"),
            )
        })
        .collect();
    assert_eq!(
        hooks,
        [("suspend", 1_756_500_500), ("resume", 1_756_500_600)],
        "the thawed daemon's observations, verbatim: {read:?}"
    );
    // And the `resumed` event still precedes them — the poll is after RUNNING.
    assert_eq!(read[0]["event"], "resumed");
}

/// **`microvm health` lands the daemon's hook observations in the VM's history,
/// deduplicated on the (hook, firedAt) pair.** (issue #80)
///
/// Three polls prove three claims. The first appends the body's two observations with
/// the daemon's own values — never anything the guest printed, which is the forgery
/// property's letter; the daemon-reported caveat (an in-guest caller can forge
/// *additional* firings by posting the unauthenticated hook paths) is documented in
/// `history.rs` and does not change what this asserts, because the values on file are
/// still exactly what the daemon reported. The second poll repeats the identical body
/// and appends nothing. The third carries one new firing and appends exactly it, with
/// `seq` continuing the one sequence.
///
/// **Guard proof.** Delete the `append_unseen_hooks` call from `attached::health` and
/// the first read below is empty while the envelope still carries `hooks`; drop the
/// dedup and the second read counts four. The dedup half was broken exactly so on
/// 2026-08-30 (in `history.rs`'s own falsification), failed as stated, restored.
#[tokio::test]
async fn a_health_poll_lands_hook_observations_in_history_and_a_repeat_appends_nothing() {
    let dir = TempDir::new("history-hooks");
    let health_command = || {
        Command::Health(HealthArgs {
            attach: AttachFlags {
                state_dir: Some(dir.0.clone()),
                ..attach_flags()
            },
            region: region_flags(),
        })
    };
    let body_two_hooks = r#"{"version": "0.1.0", "bootstrapped": true, "disk": null,
             "identity_degraded": false, "identity_repaired": true,
             "hooks": [{"hook": "validate", "fired_at": 1756500000},
                       {"hook": "run", "fired_at": 1756500100}],
             "hooks_dropped": 0}"#;

    // First poll: both observations land, with the daemon's values.
    let script = DaemonScript::new();
    script.reply(200, body_two_hooks);
    let (result, _, _) = against_daemon(&script, &health_command()).await;
    let rendered = result.expect("health answers");
    assert_eq!(
        rendered.data["hooks"],
        serde_json::json!([
            {"hook": "validate", "firedAt": 1756500000_u64},
            {"hook": "run", "firedAt": 1756500100_u64},
        ]),
        "the envelope carries the observations, camelCase like its neighbours"
    );
    assert_eq!(rendered.data["hooksDropped"], 0);
    let read = crate::history::read_events(&dir.0, "mvm-1");
    assert_eq!(read.len(), 2, "{read:?}");
    assert_eq!(read[0]["event"], "hookObserved");
    assert_eq!(read[0]["hook"], "validate");
    assert_eq!(read[0]["firedAt"], 1_756_500_000_u64);
    assert_eq!(read[1]["hook"], "run");

    // Second poll, identical body: dedup proven — nothing appends.
    let script = DaemonScript::new();
    script.reply(200, body_two_hooks);
    let (result, _, _) = against_daemon(&script, &health_command()).await;
    result.expect("health answers again");
    assert_eq!(
        crate::history::read_events(&dir.0, "mvm-1").len(),
        2,
        "a repeat poll must append nothing"
    );

    // Third poll, one new firing: exactly it appends, and seq continues.
    let script = DaemonScript::new();
    script.reply(
        200,
        r#"{"version": "0.1.0", "bootstrapped": true, "disk": null,
             "identity_degraded": false, "identity_repaired": true,
             "hooks": [{"hook": "validate", "fired_at": 1756500000},
                       {"hook": "run", "fired_at": 1756500100},
                       {"hook": "suspend", "fired_at": 1756500900}],
             "hooks_dropped": 0}"#,
    );
    let (result, _, _) = against_daemon(&script, &health_command()).await;
    result.expect("health answers a third time");
    let read = crate::history::read_events(&dir.0, "mvm-1");
    assert_eq!(read.len(), 3, "{read:?}");
    assert_eq!(read[2]["hook"], "suspend");
    assert_eq!(read[2]["firedAt"], 1_756_500_900_u64);
    assert_eq!(read[2]["seq"], 2, "one monotonic sequence across the polls");
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

// ── run <DIR>: the sync round trip (issue #72) ───────────────────────────────
//
// These guards script *both* planes: `ScriptedTransport` answers the control-plane launch
// and `DaemonScript` answers the session the launch builds, joined by
// `Sandbox::with_session_backend` — the daemon-side half of the seam
// `with_control_plane` opens. That is what lets one test drive pack → upload → exec(cwd)
// → download → extract as the shipped `run` handler actually sequences them.

/// A seam that scripts the control plane *and* the daemon behind the launched session.
struct SyncSeam {
    transport: Arc<ScriptedTransport>,
    clock: Arc<YieldingClock>,
    daemon: Arc<DaemonScript>,
}

impl CoreSeam for SyncSeam {
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
        let backend = Arc::clone(&self.daemon) as Arc<dyn microvms_core::session::HttpBackend>;
        Box::pin(
            async move { Ok(Sandbox::with_control_plane(plane).with_session_backend(backend)) },
        )
    }

    fn attach_session(
        &self,
        _region: Region,
        _attach: Attach,
    ) -> BoxFuture<'_, Result<Session, Error>> {
        Box::pin(async move {
            Err(Error::new(
                ErrorKind::Platform,
                "these guards launch rather than attach",
            ))
        })
    }

    fn put_artifact(&self, _uri: &str, _bytes: Vec<u8>) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move { Ok(()) })
    }
}

/// A project tree under a temp dir, removed on drop.
struct ProjectDir(std::path::PathBuf);

impl ProjectDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "microvm-guard-sync-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&path).expect("creates");
        Self(path)
    }
}

impl Drop for ProjectDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The tar bytes a scripted daemon hands back from `GET /v1/fs/tar`.
fn daemon_archive(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, body) in members {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, *body)
            .expect("appends");
    }
    builder.into_inner().expect("finishes")
}

/// The launch script every sync guard shares: launch succeeds, teardown succeeds.
fn sync_launch_script() -> Arc<ScriptedTransport> {
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .answer("RunMicrovm", 200, &microvm_body("PENDING"))
        .answer("GetMicrovm", 200, &microvm_body("RUNNING"))
        .answer(
            "CreateMicrovmAuthToken",
            200,
            r#"{"authToken": {"X-aws-proxy-auth": "opaque"}}"#,
        )
        .answer("TerminateMicrovm", 200, "{}")
        .answer(
            "DeleteMicrovmImage",
            200,
            r#"{"imageIdentifier": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                 "state": "DELETING"}"#,
        );
    transport
}

/// `run <DIR> --image … --exec …` args over a synced project.
fn sync_run_args(dir: &ProjectDir, state_dir: std::path::PathBuf) -> RunArgs {
    let mut args = run_args_for_image(
        "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
        state_dir,
    );
    args.binary = Some(dir.0.clone());
    args.exec = Some("make test".into());
    args
}

/// **`run <DIR>` uploads the packed tree, runs the exec in it, and brings back exactly
/// the glob-matched artifacts.**
///
/// One guard for the whole round trip, asserted on the daemon's recorded requests and the
/// local filesystem: the uploaded tar contains the project file and never `.git`; the
/// exec's start body carries `cwd: /workspace`; the downloaded archive's glob-matched
/// member lands in DIR and the unmatched one does not; and the envelope's `sync` key
/// reports all of it.
///
/// **Falsification** — three breaks, each made on 2026-08-28, each failing exactly where
/// stated, each restored: drop the `cwd` from `launch_and_exec`'s `StartSpec` and the
/// start-body assertion reads null; skip `.git` filtering in `sync::collect` and the
/// uploaded-bytes assertion finds the loose object; extract without the glob set and the
/// unmatched-member assertion finds `secrets.env` on disk.
#[tokio::test]
async fn run_dir_uploads_the_tree_execs_in_it_and_brings_back_matched_artifacts() {
    let state = TempDir::new("sync-round-trip");
    let project = ProjectDir::new("round-trip");
    std::fs::create_dir_all(project.0.join(".git")).expect("git dir");
    std::fs::write(project.0.join(".git/loose-object"), b"never uploaded").expect("blob");
    std::fs::write(project.0.join("main.py"), b"print('hi')").expect("source");
    let file = ConfigFile::new("sync-artifacts", "artifacts = [\"dist/**\"]\n");

    let daemon = DaemonScript::new();
    daemon
        // wait_until_ready's health poll.
        .reply(
            200,
            r#"{"version": "0.1.0", "bootstrapped": true, "disk": null,
                        "identity_degraded": false, "identity_repaired": true}"#,
        )
        // PUT /v1/fs/tar — the upload.
        .reply(200, "{}")
        // The exec: started, then exited 0, then acked.
        .reply(200, STARTED_BODY)
        .reply(200, &poll_body("exited", "0", "", false))
        .reply(200, &poll_body("acked", "0", "", false));
    // GET /v1/fs/tar answers raw tar bytes, not JSON, so it is queued as a raw reply.
    daemon.replies.lock().expect("not poisoned").push_back((
        200,
        daemon_archive(&[
            ("dist/report.txt", b"selected"),
            ("secrets.env", b"never asked for"),
        ]),
    ));
    // The teardown's own health poll (issue #80), carrying the daemon's hook log. The
    // hooks below are what a real launch reports: validate and ready fired in the
    // snapshot VM, run fired in this one.
    daemon.reply(
        200,
        r#"{"version": "0.1.0", "bootstrapped": true, "disk": null,
                    "identity_degraded": false, "identity_repaired": true,
                    "hooks": [{"hook": "validate", "fired_at": 1756500000},
                              {"hook": "run", "fired_at": 1756500100}],
                    "hooks_dropped": 0}"#,
    );

    let seam = SyncSeam {
        transport: sync_launch_script(),
        clock: Arc::new(YieldingClock::default()),
        daemon: Arc::clone(&daemon),
    };
    let mut args = sync_run_args(&project, state.0.clone());
    args.config = crate::cli::ConfigFlags {
        config: Some(file.0.clone()),
        no_config: false,
    };

    let (result, _) = dispatch_with(&seam, &Command::Run(args), full_infra()).await;
    let rendered = result.expect("the sync run succeeds");

    // The upload: PUT /v1/fs/tar carrying the project, not the repository.
    let requests = daemon.requests();
    let upload = requests
        .iter()
        .find(|request| request.method == "PUT" && request.path.starts_with("/v1/fs/tar"))
        .expect("an upload went out");
    assert!(
        upload.path.contains("workspace"),
        "the tree lands in the constant workdir: {}",
        upload.path
    );
    let uploaded = upload.body.clone();
    let mut names = Vec::new();
    for entry in tar::Archive::new(uploaded.as_slice())
        .entries()
        .expect("parses")
    {
        names.push(
            entry
                .expect("a member")
                .path()
                .expect("a path")
                .display()
                .to_string(),
        );
    }
    assert!(names.iter().any(|name| name == "main.py"), "{names:?}");
    assert!(
        !names.iter().any(|name| name.contains(".git")),
        "the repository's object store must not ride along: {names:?}"
    );

    // The exec runs *in* the synced tree.
    let start = requests
        .iter()
        .find(|request| request.path == "/v1/exec/start")
        .expect("a start went out");
    let body: serde_json::Value =
        serde_json::from_slice(&start.body).expect("the start body is JSON");
    assert_eq!(
        body["cwd"], "/workspace",
        "the exec must run in the synced tree, not the image's WORKDIR: {body}"
    );

    // The glob-matched artifact came back; the unmatched member did not.
    assert_eq!(
        std::fs::read_to_string(project.0.join("dist/report.txt")).expect("landed"),
        "selected"
    );
    assert!(
        !project.0.join("secrets.env").exists(),
        "a member no glob asked for must never touch the local disk"
    );

    // And the envelope says what happened.
    assert_eq!(rendered.data["sync"]["workdir"], "/workspace");
    assert_eq!(
        rendered.data["sync"]["artifacts"][0]["path"],
        "dist/report.txt"
    );
    // One member: `main.py`. The `.git` tree was skipped whole.
    assert_eq!(rendered.data["sync"]["uploadedMembers"], 1);

    // The teardown's health poll landed the daemon's hook observations in the VM's
    // history, with the daemon's own values (issue #80). The daemon lane is where the
    // values are proven; this is the wiring — a `run` that never polled would leave no
    // hookObserved lines while every other assertion above stayed green.
    let events = crate::history::read_events(&state.0, "mvm-abc123");
    let hooks: Vec<(&str, u64)> = events
        .iter()
        .filter(|event| event["event"] == "hookObserved")
        .map(|event| {
            (
                event["hook"].as_str().expect("a hook name"),
                event["firedAt"].as_u64().expect("an epoch"),
            )
        })
        .collect();
    assert_eq!(
        hooks,
        [("validate", 1_756_500_000), ("run", 1_756_500_100)],
        "the daemon's observations, verbatim: {events:?}"
    );
}

/// **An exec that fails still gets its artifacts downloaded — CI wants the logs.**
///
/// The run exits `ERR_EXEC_FAILED` exactly as a plain failing exec does, and the download
/// happens anyway: the report a failing test run produces is the artifact the caller most
/// wants, and a sync that only worked on green runs would be a sync nobody could debug
/// with.
///
/// **Falsification** — move the download inside an `exec_exit_code == 0` arm and the
/// artifact assertion finds nothing on disk while the exit assertion still reads 13. Done
/// on 2026-08-28; failed as stated; restored.
#[tokio::test]
async fn a_failing_exec_still_brings_the_artifacts_back() {
    let state = TempDir::new("sync-exec-fails");
    let project = ProjectDir::new("exec-fails");
    std::fs::write(project.0.join("main.py"), b"assert False").expect("source");
    let file = ConfigFile::new("sync-fail-artifacts", "artifacts = [\"report/**\"]\n");

    let daemon = DaemonScript::new();
    daemon
        .reply(
            200,
            r#"{"version": "0.1.0", "bootstrapped": true, "disk": null,
                        "identity_degraded": false, "identity_repaired": true}"#,
        )
        .reply(200, "{}")
        .reply(200, STARTED_BODY)
        .reply(200, &poll_body("exited", "1", "", false))
        .reply(200, &poll_body("acked", "1", "", false));
    daemon
        .replies
        .lock()
        .expect("not poisoned")
        .push_back((200, daemon_archive(&[("report/junit.xml", b"<failure/>")])));
    // The teardown's health poll (issue #80); no hooks in this body, so nothing lands.
    daemon.reply(
        200,
        r#"{"version": "0.1.0", "bootstrapped": true, "disk": null,
                    "identity_degraded": false, "identity_repaired": true}"#,
    );

    let seam = SyncSeam {
        transport: sync_launch_script(),
        clock: Arc::new(YieldingClock::default()),
        daemon: Arc::clone(&daemon),
    };
    let mut args = sync_run_args(&project, state.0.clone());
    args.config = crate::cli::ConfigFlags {
        config: Some(file.0.clone()),
        no_config: false,
    };

    let (result, _) = dispatch_with(&seam, &Command::Run(args), full_infra()).await;
    let rendered = result.expect("a failing workload keeps its success envelope");
    assert_eq!(
        rendered.already_reported,
        Some(Exit::ExecFailed),
        "the workload failed and the caller must see 13"
    );
    assert_eq!(
        std::fs::read_to_string(project.0.join("report/junit.xml")).expect("landed"),
        "<failure/>",
        "the failing run's report is the artifact the caller most wants"
    );
}

/// **A directory positional with nothing supplying an image is refused before any call.**
///
/// Sync mode launches; there is no binary to build from. The refusal is
/// `ERR_PRECONDITION` with the remedy named, and it costs zero doors — asserted the same
/// way the broken-config guard asserts it.
#[tokio::test]
async fn a_sync_dir_without_an_image_is_refused_before_any_call() {
    let state = TempDir::new("sync-no-image");
    let project = ProjectDir::new("no-image");
    let seam = RefusingSeam::new();

    let mut args = sync_run_args(&project, state.0.clone());
    args.image = None;
    args.config = no_config();

    let (result, _) = dispatch_with(&seam, &Command::Run(args), full_infra()).await;
    let failure = result.expect_err("nothing to launch from");
    assert_eq!(failure.exit, Exit::Precondition);
    assert!(failure.message.contains("sync mode"), "{}", failure.message);
    assert!(
        seam.doors().is_empty(),
        "a local refusal must cost zero doors"
    );
}

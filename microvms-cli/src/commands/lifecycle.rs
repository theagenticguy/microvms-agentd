//! `run`, `build`, `exec`, `suspend`, `resume`, `terminate` — the commands that touch AWS.
//!
//! # Two paths, and the split is not arbitrary
//!
//! `run` and `build` go through [`microvms_core::sandbox::Sandbox`], because they *launch*
//! and every one of the STATE-* guards is a property of that type: the bootstrap counter, the
//! suspended window recorded at launch, the teardown ordering.
//!
//! `suspend`, `resume`, and `terminate` go through [`ControlPlane`] directly, because they
//! address a VM this invocation did not launch and `Sandbox` cannot be pointed at one — its
//! fields are private, which is exactly what makes the Z3 proofs claims about the struct
//! rather than about prose. See the packet's §3b gap note for what the attached path loses
//! and what stands in for each closure; the short version is that `suspendedDurationSeconds`
//! is unknowable to a process that did not send the launch, so the service answers, which is
//! the same choice `cli.py:1756` made and for the same stated reason.
//!
//! # The interrupt guard is a `select!`, and the sandbox outlives it (CLI-6)
//!
//! [`do_run`] constructs the `Sandbox` *before* the select and only borrows it inside. So when
//! the select drops a cancelled launch future, the sandbox is still owned here and still holds
//! whatever core recorded before the cancellation — including the `microvm` field, which
//! `sandbox.rs:574` assigns after `RunMicrovm` is accepted and before the RUNNING wait. That
//! is what makes the teardown able to name a VM whose launch never finished, which is the
//! whole of CLI-6: the identifiers are the remedy, and an image wedged in `CREATING` cannot be
//! deleted later at all.

use std::time::Duration;

use microvms_core::control::{ControlPlane, CreateImageRequest, WaitOpts};
use microvms_core::sandbox::{RunRequest, Sandbox, TeardownOpts, TeardownReport};
use microvms_core::{Error, ErrorKind};
use serde_json::{Map, json};

use crate::cli::{BuildArgs, ExecArgs, ResumeArgs, RunArgs, SuspendArgs, TerminateArgs};
use crate::commands::{Ctx, Rendered, response_type};
use crate::exit::Exit;
use crate::ledger::Ledger;
use crate::render::RunOutcome;
use crate::seam::{Attach, resolve_region, state_dir};

/// The future a launch races against. See the module docs.
///
/// Injected rather than `tokio::signal::ctrl_c()` called inline, so the guard can fire it
/// deterministically mid-launch instead of sending a real signal to a test process — which
/// under a parallel runner would interrupt whichever test happened to be running.
pub type Interrupt<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

/// An interrupt that never fires.
///
/// Not gated: the shipped binary always passes [`on_ctrl_c`], so this is only ever called from a
/// test — but gating it with `#[cfg(test)]` here opened a *second* inline test region in this file,
/// and `tests/thinness.rs`'s scan cuts at the first one. That would have silently excluded eight
/// hundred lines of handler from the static thinness check. The guard caught it; recorded because
/// "a helper gated where it is defined" is the innocuous-looking edit that would have done it again.
///
/// A dead-code warning is the cost, and it is paid with the narrow allow below rather than by
/// moving the function into the test module — `guards.rs` is a different module and would have to
/// reach it through `super`, which is worse than one attribute.
#[allow(
    dead_code,
    reason = "the test-only arm of the interrupt seam; see the doc comment"
)]
pub fn never() -> Interrupt<'static> {
    Box::pin(std::future::pending())
}

/// `tokio::signal::ctrl_c`, as the shape [`do_run`] takes.
pub fn on_ctrl_c() -> Interrupt<'static> {
    Box::pin(async {
        // A failure to install the handler is not a reason to refuse to run: the caller asked
        // for a sandbox, and the worst case is that ctrl-c reaches the default disposition
        // and kills the process — which is the behaviour they had before this existed.
        let _ = tokio::signal::ctrl_c().await;
    })
}

// ── run ─────────────────────────────────────────────────────────────────────

/// Build, launch, exec, report, tear down — the whole thing, once.
pub async fn run<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &RunArgs,
    interrupt: Interrupt<'_>,
) -> Result<Rendered, crate::exit::CliError> {
    let region = resolve_region(
        args.region.region.map(|r| r.region()),
        args.region.unlisted_region.as_deref(),
        ctx.env,
    )?;
    let size = args.memory.size_class();
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| format!("microvm-cli-{}", epoch_secs()));

    // What was resolved, before anything is attempted. Not decoration: the next thing that
    // happens is a credential resolution that can hang or fail, and an operator watching a stalled
    // command needs to know which region and which image name it stalled on. It goes *before* the
    // preconditions for the same reason — a caller who mistyped a path should see what the rest of
    // the invocation would have done.
    ctx.out
        .progress(&format!("preparing {name} in {region} ({size})"));

    // Every precondition before anything is created, so a missing role is not discovered
    // after a 45-minute build. `execution_role_arn` is always needed; the build inputs only
    // when this invocation is the one building.
    let building = args.image.is_none();
    if building {
        ctx.infra
            .require(&["execution_role_arn", "build_role_arn"])?;
        let Some(binary) = &args.binary else {
            return Err(crate::exit::CliError::new(
                Exit::Precondition,
                "no BINARY and no --image: `run` either builds an image from a daemon binary or \
                 launches one you name.",
            )
            .suggest("`microvm run ./agentd` builds, `microvm run --image <arn>` launches"));
        };
        if !binary.exists() {
            return Err(crate::exit::CliError::new(
                Exit::Precondition,
                format!("daemon binary not found: {}", binary.display()),
            )
            .suggest("cargo build --release -p agentd --target aarch64-unknown-linux-musl")
            .suggest("`microvm doctor --binary <path>` checks the architecture too"));
        }
    } else {
        ctx.infra.require(&["execution_role_arn"])?;
    }

    let ledger_root = state_dir(args.state_dir.clone(), ctx.env);
    let mut ledger = Ledger::new(region.as_str(), &ledger_root);
    let mut sandbox = ctx.seam.open_sandbox(region, args.port).await?;
    let mut outcome = RunOutcome {
        image_name: Some(name.clone()),
        ..RunOutcome::default()
    };

    // The launch, raced against the interrupt. `Box::pin` so the two arms are the same shape
    // and the select does not need the body to be a named future.
    let launched = {
        let body = Box::pin(launch_and_exec(
            ctx,
            args,
            &mut sandbox,
            &mut ledger,
            &name,
            size,
            &mut outcome,
        ));
        tokio::select! {
            result = body => result,
            () = interrupt => Err(Error::new(
                ErrorKind::Interrupted,
                "interrupted after launch. Tearing down: an image left in CREATING cannot be \
                 deleted afterwards at all, so anything this teardown fails to remove is named \
                 below and in the failure envelope's `data.leaked` (docs/PLATFORM.md, 'The build \
                 log group survives Terraform').",
            )),
        }
    };

    // The identifiers, read off the sandbox *after* the select rather than only inside the
    // cancelled body.
    //
    // This is the defect the CLI-6 guard found in its own first draft, and it is worth stating
    // plainly because the wrong version looks right. `launch_and_exec` records the VM id in
    // `outcome` after its RUNNING wait returns — so an interrupt that lands during that wait
    // cancels the body before the assignment, and the teardown had a VM it could not name. Core
    // had the id the whole time (`sandbox.rs:574` assigns `microvm` when `RunMicrovm` is accepted,
    // which is *before* the wait), and reading it here is what recovers it.
    //
    // Which makes the ownership arrangement above load-bearing rather than incidental: the
    // sandbox is constructed outside the `select!` and only borrowed inside, so a cancelled
    // launch leaves it alive and still holding everything core recorded.
    if let Some(vm) = sandbox.microvm() {
        outcome.microvm_id = Some(vm.id.clone());
        ledger.record_microvm(&vm.id);
    }
    if let Some(image) = sandbox.image() {
        outcome.image_identifier = Some(image.identifier.clone());
        ledger.record_image(&image.identifier, &image.name);
    }

    // Runs however the block above ended, which is CLI-6. Recorded as leaked *before* the
    // delete is attempted — the other order loses the identifier when the process dies inside
    // the call, which is exactly the interrupt case.
    let teardown = tear_down(ctx, &mut sandbox, &mut ledger, args.keep).await;
    outcome.kept = args.keep;
    outcome.leaked = ledger.record.leaked.clone();

    // Cost is attributed whichever way the run ended: a launch that was interrupted still
    // billed for the seconds it ran, and a report only on the happy path is a report that
    // hides the expensive failures.
    attach_cost(ctx, &mut outcome, size, &name);

    if let Err(error) = launched {
        // The failure envelope carries the partial result, which for an interrupt is the whole
        // point: the identifiers are the operator's to-do list.
        let mut failure = crate::exit::classify(&error);
        failure = failure.with_data("leaked", json!(outcome.leaked));
        if let Some(id) = &outcome.microvm_id {
            failure = failure.with_data("microvmId", json!(id));
        }
        if let Some(image) = &outcome.image_identifier {
            failure = failure.with_data("imageIdentifier", json!(image));
        }
        if !teardown.undeleted.is_empty() {
            failure = failure.with_data("undeleted", json!(teardown.undeleted));
        }
        failure = failure.with_data("terminateAccepted", json!(teardown.terminate_accepted));
        return Err(failure);
    }

    let (kind, _) = response_type("run");
    let dense = outcome.render(true);
    let text = outcome.render(false);
    let rendered = Rendered::ok(kind, outcome.to_data(), text, dense);
    // A failing workload keeps its success envelope and earns a non-zero code: the sandbox did
    // its job and the output the caller asked for is in `data`. Mapped onto one stable code
    // rather than passed through raw, because a workload exiting 4 must not be
    // indistinguishable from a credential failure.
    if outcome.exec_exit_code.is_some_and(|code| code != 0) {
        return Ok(rendered.reporting(Exit::ExecFailed));
    }
    Ok(rendered)
}

/// The build/launch/exec body `run` races against the interrupt.
///
/// Separated so the `select!` arm is one expression, and because every `?` in here has to be
/// cancellable — which it is, since the only state that must survive a cancellation lives in
/// `sandbox` and `ledger`, both borrowed rather than owned.
async fn launch_and_exec<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &RunArgs,
    sandbox: &mut Sandbox,
    ledger: &mut Ledger,
    name: &str,
    size: microvms_core::SizeClass,
    outcome: &mut RunOutcome,
) -> Result<(), Error> {
    let started = std::time::Instant::now();

    let image_identifier = match &args.image {
        Some(identifier) => {
            ctx.out
                .progress(&format!("launching from the existing image {identifier}"));
            identifier.clone()
        }
        None => {
            let binary = args.binary.as_ref().expect("checked by the caller");
            ctx.out.progress(&format!("building image {name} ({size})"));
            let request = build_request(ctx, args, name, size, binary)?;
            upload_artifact(ctx, sandbox, &request).await?;
            let image = sandbox.build_image(request).await?;
            let identifier = image.identifier.clone();
            outcome.image_identifier = Some(identifier.clone());
            outcome.build_seconds = started.elapsed().as_secs_f64();
            ledger.record_image(&identifier, name);
            ctx.out.progress(&format!(
                "image {identifier} built in {:.0}s",
                outcome.build_seconds
            ));
            identifier
        }
    };

    let mut request = RunRequest::new()
        .with_image(image_identifier)
        .with_suspended_sec(args.suspended_sec);
    request.execution_role_arn = ctx.infra.execution_role_arn.clone();
    request.max_idle_sec = args.max_idle_sec;
    request.max_duration_sec = args.max_duration_sec;
    request.token_scope = Some(name.to_string());
    if args.egress {
        request = request.with_egress();
    }

    ctx.out.progress("launching");
    let run_started = std::time::Instant::now();
    let session = sandbox.run(request).await?;
    // Read off the session and the sandbox rather than remembered from the request, because
    // the endpoint is what the *service* reported.
    let endpoint = session.endpoint().to_string();
    session
        .wait_until_ready(microvms_core::session::DEFAULT_READY_TIMEOUT)
        .await?;

    let exec = args.exec.clone();
    let timeout = Duration::from_secs_f64(args.timeout.max(0.0));
    if let Some(command) = exec {
        ctx.out.progress(&format!("exec: {command}"));
        let request = start_request(&command, None);
        let result = sandbox
            .session()
            .expect("run() built one")
            .run_sync(request, timeout)
            .await?;
        outcome.exec_exit_code = result.exit_code();
        outcome.stdout = result.stdout().to_string();
        outcome.stderr = result.stderr().to_string();
        outcome.truncated = result
            .outcome
            .as_ref()
            .is_some_and(|outcome| outcome.truncated);
    }
    outcome.running_seconds = run_started.elapsed().as_secs_f64();
    outcome.endpoint = Some(endpoint.clone());
    if let Some(vm) = sandbox.microvm() {
        outcome.microvm_id = Some(vm.id.clone());
        ledger.record_microvm(&vm.id);
    }
    ctx.out.progress(&format!("microvm RUNNING at {endpoint}"));
    Ok(())
}

/// Tears the VM down on the way out, however the block ended, and names what leaked.
///
/// `--keep` is opt-in and that asymmetry is deliberate: a CLI that leaves a billable VM
/// running by default is worse than no CLI, because the bill arrives a month after the person
/// forgot they ran it. `--keep` prints the identifiers precisely because the caller has just
/// taken responsibility for them.
async fn tear_down<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    sandbox: &mut Sandbox,
    ledger: &mut Ledger,
    keep: bool,
) -> TeardownReport {
    if keep {
        ctx.out
            .progress("keeping the microvm and image — you own the bill now");
        ledger.mark_outstanding();
        return TeardownReport::default();
    }
    // Recorded outstanding *first*, cleared only for what a delete reported gone.
    ledger.mark_outstanding();
    ctx.out.progress("tearing down");
    let report = sandbox
        .terminate(
            TeardownOpts::default()
                .deleting_image()
                .deleting_log_group(),
        )
        .await;

    // The VM's own identifier leaves the outstanding list when the terminate was accepted:
    // core puts it in `undeleted` only when the call *failed*, so "accepted" is the honest
    // reading of "no longer this operator's problem".
    let mut still = report.undeleted.clone();
    if report.terminate_accepted
        && let Some(id) = &ledger.record.microvm_id
    {
        still.retain(|entry| entry != id);
    }
    ledger.mark_deleted(&still);
    for identifier in &still {
        // Never suppressed by `--quiet`. A leak nobody is told about is the failure `--quiet`
        // must not be able to buy.
        ctx.out.warn(&format!(
            "could not delete {identifier} — it is still billing. An image in CREATING cannot \
             be deleted at all (docs/PLATFORM.md, '`clientToken` is a permanent idempotency \
             key'), so record this id."
        ));
    }
    ledger.clear();
    report
}

/// Attaches the cost report, warning if the rate table is stale.
///
/// A stale-rate warning reaches stderr even under `--quiet`, for the same reason a leak does:
/// a figure copied into a budget is worse than no figure when it is out of date.
fn attach_cost<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    outcome: &mut RunOutcome,
    size: microvms_core::SizeClass,
    name: &str,
) {
    use microvms_core::cost::{CalendarDate, DurationP, RunUsage, pinned_rates, run_report};

    let seconds = |value: f64| -> Option<DurationP> {
        if value <= 0.0 {
            return None;
        }
        DurationP::measured_secs_f64(value).ok()
    };
    let usage = RunUsage {
        running: seconds(outcome.running_seconds),
        image_build: seconds(outcome.build_seconds),
        // The image's own size is not observable from any API this client calls, so the
        // baseline footprint stands in and the line item's note says so. Omitting the storage
        // line entirely would make a create-and-destroy run look like it cost only its
        // compute, and the one-week minimum retention means storage is in fact the floor.
        image_gb: outcome
            .image_identifier
            .as_ref()
            .map(|_| size.baseline_gb()),
        ..RunUsage::launched()
    };
    let Ok(report) = run_report(
        size,
        &usage,
        &pinned_rates(),
        CalendarDate::today_utc(),
        format!("run {name}"),
    ) else {
        // A cost report that will not compute is not a reason to fail a run that worked. The
        // arithmetic's only fallible inputs are the two float boundaries, and both were
        // already checked above.
        return;
    };
    if let Some(warning) = report.staleness() {
        ctx.out.warn(warning);
    }
    ctx.out.progress(&report.render());
    outcome.cost = Some(crate::render::report_to_json(&report));
}

// ── build ───────────────────────────────────────────────────────────────────

/// Builds an image and waits for it to be usable.
pub async fn build<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &BuildArgs,
) -> Result<Rendered, crate::exit::CliError> {
    let region = resolve_region(
        args.region.region.map(|r| r.region()),
        args.region.unlisted_region.as_deref(),
        ctx.env,
    )?;
    ctx.infra.require(&["build_role_arn"])?;
    if !args.binary.exists() {
        return Err(crate::exit::CliError::new(
            Exit::Precondition,
            format!("daemon binary not found: {}", args.binary.display()),
        )
        .suggest("cargo build --release -p agentd --target aarch64-unknown-linux-musl"));
    }
    let size = args.memory.size_class();
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| format!("microvm-cli-{}", epoch_secs()));

    let mut sandbox = ctx.seam.open_sandbox(region, args.port).await?;
    ctx.out.progress(&format!("building image {name} ({size})"));
    let request = build_request_from(
        ctx,
        &name,
        size,
        &args.binary,
        args.dockerfile.as_deref(),
        args.repair_identity,
        args.artifact_uri.as_deref(),
    )?;
    upload_artifact(ctx, &sandbox, &request).await?;
    let image = sandbox.build_image(request).await?;

    let mut data = Map::new();
    data.insert("imageIdentifier".into(), json!(image.identifier));
    data.insert("imageName".into(), json!(image.name));
    // Named in the payload because the *service* creates it, Terraform never owns it, and
    // `terraform destroy` leaves it behind — so the caller who built this image is the only
    // one who will ever know to delete it.
    data.insert("buildLogGroup".into(), json!(image.build_log_group()));
    data.insert("size".into(), json!(size.to_string()));

    let (kind, _) = response_type("build");
    let dense = format!(
        "{}\t{}\t{}",
        image.identifier,
        image.name,
        image.build_log_group()
    );
    let text = [
        format!("image: {}", image.identifier),
        format!("name: {}", image.name),
        format!("size: {size}"),
        format!("build log group: {}", image.build_log_group()),
        "note: the service created that log group; terraform destroy will not remove it"
            .to_string(),
    ]
    .join("\n");
    Ok(Rendered::ok(kind, data, text, dense))
}

/// The create request for `run`'s arguments.
fn build_request<O: std::io::Write, E: std::io::Write>(
    ctx: &Ctx<'_, O, E>,
    args: &RunArgs,
    name: &str,
    size: microvms_core::SizeClass,
    binary: &std::path::Path,
) -> Result<CreateImageRequest, Error> {
    build_request_from(
        ctx,
        name,
        size,
        binary,
        args.dockerfile.as_deref(),
        args.repair_identity,
        args.artifact_uri.as_deref(),
    )
}

/// The create request, shared by `run` and `build`.
///
/// `size` is a [`microvms_core::SizeClass`] rather than an integer all the way from the
/// parser, so there is no point on this path where an off-table baseline could be written.
fn build_request_from<O: std::io::Write, E: std::io::Write>(
    ctx: &Ctx<'_, O, E>,
    name: &str,
    size: microvms_core::SizeClass,
    binary: &std::path::Path,
    dockerfile: Option<&std::path::Path>,
    repair_identity: bool,
    artifact_uri: Option<&str>,
) -> Result<CreateImageRequest, Error> {
    let bytes = std::fs::read(binary).map_err(|error| {
        Error::new(
            ErrorKind::Precondition,
            format!("could not read {}: {error}", binary.display()),
        )
        .with_source(error)
    })?;
    // Either the caller already uploaded, or a bucket was given and the artifact goes to a
    // derived key. See `seam::CoreSeam::put_artifact` for why the upload is not core's.
    let uri = match (artifact_uri, ctx.infra.bucket.as_deref()) {
        (Some(uri), _) => uri.to_string(),
        (None, Some(bucket)) => format!("s3://{bucket}/{name}.zip"),
        (None, None) => {
            return Err(Error::new(
                ErrorKind::Precondition,
                "no --artifact-uri and no --bucket. CreateMicrovmImage names an artifact that \
                 must already be in S3, and microvms-core does not upload — S3 is deliberately \
                 absent from its dependency set, and an S3 client in this CLI would give it a \
                 second path to AWS. Either pass --bucket (the artifact is uploaded with the \
                 `aws` CLI) or upload it yourself and pass --artifact-uri.",
            ));
        }
    };

    let mut request = CreateImageRequest::new(
        name,
        bytes,
        uri,
        ctx.infra.build_role_arn.clone().unwrap_or_default(),
    );
    request.size = size;
    request.repair_guest_identity = repair_identity;
    // A *label* beside core's own per-attempt nonce, never a token. Core accepts no token at
    // all, which is what makes the wedge unwriteable rather than merely defaulted (TRAP-1).
    request.token_scope = Some(name.to_string());
    if let Some(path) = dockerfile {
        request.dockerfile = Some(std::fs::read_to_string(path).map_err(|error| {
            Error::new(
                ErrorKind::Precondition,
                format!("could not read {}: {error}", path.display()),
            )
            .with_source(error)
        })?);
    }
    Ok(request)
}

/// Puts the artifact where the create request says it is, unless the caller already did.
///
/// Skipped when `--artifact-uri` was given: the caller said the bytes are there, and
/// re-uploading over a URI they own is not this command's business.
async fn upload_artifact<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    sandbox: &Sandbox,
    request: &CreateImageRequest,
) -> Result<(), Error> {
    if ctx.infra.bucket.is_none() {
        // The URI came from `--artifact-uri`; the bytes are the caller's problem and they said
        // so explicitly.
        return Ok(());
    }
    let bytes = sandbox.build_artifact_for(request)?;
    ctx.out.progress(&format!(
        "uploading {} bytes of artifact to {}",
        bytes.len(),
        request.code_artifact_uri
    ));
    ctx.seam
        .put_artifact(&request.code_artifact_uri, bytes)
        .await
}

// ── exec ────────────────────────────────────────────────────────────────────

/// Runs one command in a MicroVM that is already running.
pub async fn exec<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &ExecArgs,
) -> Result<Rendered, crate::exit::CliError> {
    let region = resolve_region(
        args.region.region.map(|r| r.region()),
        args.region.unlisted_region.as_deref(),
        ctx.env,
    )?;
    let session = ctx
        .seam
        .attach_session(
            region,
            Attach {
                endpoint: args.endpoint.clone(),
                agent_token: args.agent_token.clone(),
                microvm_id: args.microvm_id.clone(),
                port: args.port,
            },
        )
        .await?;

    ctx.out.progress(&format!("exec: {}", args.command));
    let request = start_request(&args.command, args.cwd.clone());
    let exec_id = request.exec_id.clone();
    let result = session
        .run_sync(request, Duration::from_secs_f64(args.timeout.max(0.0)))
        .await?;

    let mut data = Map::new();
    data.insert("execId".into(), json!(result.exec_id));
    data.insert("exitCode".into(), json!(result.exit_code()));
    data.insert("stdout".into(), json!(result.stdout()));
    data.insert("stderr".into(), json!(result.stderr()));
    let truncated = result
        .outcome
        .as_ref()
        .is_some_and(|outcome| outcome.truncated);
    data.insert("truncated".into(), json!(truncated));

    let (kind, _) = response_type("exec");
    let code = result.exit_code();
    let dense = format!(
        "exit\t{}\n{}",
        code.map(|c| c.to_string()).unwrap_or_default(),
        result.stdout()
    );
    let mut lines: Vec<String> = Vec::new();
    for part in [result.stdout(), result.stderr()] {
        if !part.is_empty() {
            lines.push(part.trim_end_matches('\n').to_string());
        }
    }
    lines.push(match code {
        Some(code) => format!("exit code: {code}"),
        // A signal death is not an exit code, and reporting it as one — 0, or 128+n — is how
        // a CI caller reads a killed process as a pass.
        None => format!("exec {exec_id} died to a signal rather than exiting"),
    });
    let rendered = Rendered::ok(kind, data, lines.join("\n"), dense);
    if code != Some(0) {
        return Ok(rendered.reporting(Exit::ExecFailed));
    }
    Ok(rendered)
}

/// A start request for one shell command.
///
/// `shell: true` with a single-element command, which is the `run_sync(command, shell=True)`
/// shape `cli.py` uses. A bare string with `shell: false` would become a one-element argv —
/// never whitespace-split — so passing a shell line that way silently looks for a binary
/// named `ls -la`.
///
/// The type comes from the `protocol` crate rather than from `microvms_core`, because
/// `Session::run`'s signature names it and core does not re-export it. See the packet's gap
/// note and the reason beside that dependency in `Cargo.toml`.
fn start_request(command: &str, cwd: Option<String>) -> protocol::exec::StartRequest {
    // Every field written out rather than `..Default::default()`, and not only because
    // `StartRequest` has no `Default`: this struct is the wire contract, so a field added on the
    // daemon side should break this build and make someone decide what the CLI sends. A struct
    // update would have silently defaulted it.
    protocol::exec::StartRequest {
        // The idempotency key. A caller whose retry must be safe across its own restart
        // supplies a stable one, which this surface deliberately does not expose: `microvm
        // exec` is one shot, and an id flag would invite reusing one.
        exec_id: format!("x-{:016x}", epoch_nanos()),
        command: vec![command.to_string()],
        shell: true,
        cwd,
        env: std::collections::HashMap::new(),
        // No demotion: the daemon's own user is what the image chose, and a uid flag on this
        // surface would be a number with no way to check it means anything in that guest.
        user: None,
        group: None,
        // The client-side deadline is the caller's `--timeout`, applied by `run_sync`. Sending it
        // as the *daemon's* budget too would kill the child at a deadline the caller cannot see
        // in the exit code.
        timeout_sec: None,
        // Opt-in, and not asked for here: a child holding an open stdin pipe nobody will ever
        // write to is a child that blocks forever the first time it reads.
        stdin: false,
    }
}

// ── suspend / resume / terminate (the attached path) ─────────────────────────

/// Freezes a MicroVM.
///
/// Reads the state first and refuses locally from anything but RUNNING. That costs one
/// `GetMicrovm` where [`Sandbox::suspend`] costs none — see the packet's §3b note — and it is
/// still worth doing: `SuspendMicrovm` against a non-running id answers about the id rather
/// than saying which of two things the caller got wrong, and a suspend issued from SUSPENDED
/// is a caller who believes they resumed.
pub async fn suspend<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &SuspendArgs,
) -> Result<Rendered, crate::exit::CliError> {
    let region = resolve_region(
        args.region.region.map(|r| r.region()),
        args.region.unlisted_region.as_deref(),
        ctx.env,
    )?;
    let plane = ctx.seam.control_plane(region).await?;
    let current = plane.get_microvm(&args.microvm_id).await?;
    if current.state != "RUNNING" {
        return Err(crate::exit::CliError::new(
            Exit::InvalidArg,
            format!(
                "microvm {} is {} and a suspend is only issued from RUNNING (STATE-5). Refused \
                 here rather than by the service, because the service's answer about a \
                 non-running id does not say which of the two things went wrong — and a suspend \
                 issued from SUSPENDED is a caller who believes they resumed.",
                args.microvm_id, current.state,
            ),
        )
        .with_data("state", json!(current.state)));
    }

    ctx.out.progress(&format!("suspending {}", args.microvm_id));
    plane.suspend(&args.microvm_id).await?;
    // TERMINATED is *wanted* rather than failed on: a VM that dies while suspending is a state
    // to report, not an error raised out of the middle of a teardown path.
    let settled = plane
        .wait_for_state(
            &args.microvm_id,
            &microvms_core::control::microvm::SUSPEND_WANTED,
            &[],
            wait_opts(args.timeout),
        )
        .await?;

    let mut data = Map::new();
    data.insert("microvmId".into(), json!(args.microvm_id));
    data.insert("state".into(), json!(settled.state));
    let (kind, _) = response_type("suspend");
    let rendered = Rendered::ok(
        kind,
        data,
        format!("{} is {}", args.microvm_id, settled.state),
        format!("{}\t{}", args.microvm_id, settled.state),
    );
    if settled.state != "SUSPENDED" {
        // The caller asked for SUSPENDED and did not get it. A success envelope, because the
        // state really is what the payload says — and a non-zero code, because a script
        // branching on `$?` needs to know the freeze did not happen.
        return Ok(rendered.reporting(Exit::Platform));
    }
    Ok(rendered)
}

/// Thaws a suspended MicroVM and reports its endpoint.
///
/// The suspended window is **not** checked here, and that is correct rather than a gap:
/// `suspendedDurationSeconds` exists only in the `RunMicrovm` request, so a process that did
/// not send the launch cannot know it. Inventing a default would either reject an open window
/// or accept a closed one, both worse than letting the service answer — which is what
/// `fail_on: DEAD_STATES` does, failing fast with `stateReason` instead of burning the poll
/// timeout.
pub async fn resume<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &ResumeArgs,
) -> Result<Rendered, crate::exit::CliError> {
    let region = resolve_region(
        args.region.region.map(|r| r.region()),
        args.region.unlisted_region.as_deref(),
        ctx.env,
    )?;
    let plane = ctx.seam.control_plane(region).await?;
    ctx.out.progress(&format!("resuming {}", args.microvm_id));
    plane.resume(&args.microvm_id).await?;
    let running = plane
        .wait_for_state(
            &args.microvm_id,
            &["RUNNING"],
            &microvms_core::constants::DEAD_STATES,
            wait_opts(args.timeout),
        )
        .await?;

    let mut data = Map::new();
    data.insert("microvmId".into(), json!(args.microvm_id));
    data.insert("state".into(), json!("RUNNING"));
    // The endpoint the service just reported rather than one the caller passed: it is measured
    // not to change across a cycle, and reading it from the response is what makes that a
    // fact this code depends on rather than an assumption it encodes.
    data.insert("endpoint".into(), json!(running.endpoint));
    let (kind, _) = response_type("resume");
    Ok(Rendered::ok(
        kind,
        data,
        format!("{} is RUNNING at {}", args.microvm_id, running.endpoint),
        format!("{}\tRUNNING\t{}", args.microvm_id, running.endpoint),
    ))
}

/// Tears down a MicroVM, and optionally its image and build log group.
///
/// Never fails on a teardown failure — it reports the identifier instead. An identifier you
/// can read is the only remedy for a resource that would not delete.
///
/// The order is the VM, then the image, then the log group **last**, because the service can
/// recreate a group deleted before its image; `_tear_down` in `cli.py:851` records leaking one
/// on the first live run by getting this backwards.
pub async fn terminate<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &TerminateArgs,
) -> Result<Rendered, crate::exit::CliError> {
    let region = resolve_region(
        args.region.region.map(|r| r.region()),
        args.region.unlisted_region.as_deref(),
        ctx.env,
    )?;
    let plane = ctx.seam.control_plane(region).await?;

    ctx.out
        .progress(&format!("terminating {}", args.microvm_id));
    let mut leaked: Vec<String> = Vec::new();
    let mut log_groups: Vec<String> = Vec::new();
    let mut state = "TERMINATING".to_string();

    // 1. The VM. A failure is recorded rather than raised, for the reason above.
    match plane.terminate(&args.microvm_id).await {
        Ok(()) => {}
        Err(error) => {
            leaked.push(args.microvm_id.clone());
            ctx.out.warn(&format!(
                "the terminate call for {} failed: {error}. The VM is still billing; record \
                 this id.",
                args.microvm_id
            ));
        }
    }
    if args.wait && leaked.is_empty() {
        match plane
            .wait_for_state(&args.microvm_id, &["TERMINATED"], &[], wait_opts(300.0))
            .await
        {
            Ok(settled) => state = settled.state,
            // Not a leak: the platform accepted the terminate, so the VM is on its way out and
            // TERMINATING is the honest state to report.
            Err(error) => ctx.out.warn(&format!(
                "{} did not reach TERMINATED before the deadline: {error}",
                args.microvm_id
            )),
        }
    }

    // 2. The image, retrying — an image in CREATING refuses deletion and a VM still
    //    terminating holds a reference to it.
    if args.delete_image {
        let identifier = args
            .image_identifier
            .as_deref()
            .expect("clap's `requires` guarantees this");
        ctx.out.progress(&format!("deleting image {identifier}"));
        let deleted = plane
            .delete_image(
                identifier,
                microvms_core::sandbox::DEFAULT_DELETE_ATTEMPTS,
                microvms_core::sandbox::DEFAULT_DELETE_BACKOFF,
            )
            .await;
        if !deleted {
            leaked.push(identifier.to_string());
            ctx.out.warn(&format!(
                "could not delete image {identifier} — it is still billing storage. An image in \
                 CREATING cannot be deleted at all (docs/PLATFORM.md, '`clientToken` is a \
                 permanent idempotency key')."
            ));
        }

        // 3. The log group, LAST, and **named rather than deleted**: CloudWatch is absent from
        //    core's dependency set, so a clean-looking teardown over an accumulating group is
        //    the alternative — which is how six of them were found. Reported as a named leak,
        //    not an error.
        if let Some(name) = &args.image_name {
            let group = format!(
                "{}/{name}",
                microvms_core::control::image::BUILD_LOG_GROUP_PREFIX
            );
            log_groups.push(group.clone());
            ctx.out.warn(&format!(
                "the build log group {group} was not deleted: neither microvms-core nor this CLI \
                 carries a CloudWatch client. It is service-created, so no Terraform stack owns \
                 it and `terraform destroy` leaves it behind — delete it with `aws logs \
                 delete-log-group --log-group-name {group}`."
            ));
        } else {
            ctx.out.warn(
                "--image-name was not given, so this image's build log group could not even be \
                 named. The group is /aws/lambda-microvms/<image-name> and the service created \
                 it, which means nothing else will ever remove it.",
            );
        }
    }

    let mut data = Map::new();
    data.insert("microvmId".into(), json!(args.microvm_id));
    data.insert("imageIdentifier".into(), json!(args.image_identifier));
    data.insert("leaked".into(), json!(leaked));
    // Separate from `leaked` because they are different claims: `leaked` is "a delete was
    // attempted and failed", and this is "no client here can delete it at all". Collapsing
    // them would make a normal `--delete-image` teardown look like a failed one.
    data.insert("undeletedLogGroups".into(), json!(log_groups));
    data.insert("state".into(), json!(state));

    let (kind, _) = response_type("terminate");
    let mut lines = vec![format!("terminated {} ({state})", args.microvm_id)];
    lines.extend(leaked.iter().map(|id| format!("LEAKED: {id}")));
    lines.extend(
        log_groups
            .iter()
            .map(|group| format!("NOT DELETED (no CloudWatch client): {group}")),
    );
    let rendered = Rendered::ok(
        kind,
        data,
        lines.join("\n"),
        format!("{}\t{}\t{}", args.microvm_id, state, leaked.join(",")),
    );
    if !leaked.is_empty() {
        // A leak the caller must act on. The log group is *not* in this condition: it is a
        // normal outcome of a teardown by a client without CloudWatch, and failing over it
        // would make every successful `--delete-image` exit non-zero.
        return Ok(rendered.reporting(Exit::Platform));
    }
    Ok(rendered)
}

/// A lifecycle wait with the caller's deadline and core's poll interval.
fn wait_opts(timeout_sec: f64) -> WaitOpts {
    WaitOpts {
        timeout: Duration::from_secs_f64(timeout_sec.max(0.0)),
        poll_interval: Duration::from_secs(5),
        // No stall grace: that is the image build's TRAP-2 probe, and a lifecycle transition
        // has no build list to probe.
        stall_grace: Duration::MAX,
    }
}

/// Seconds since the epoch, for a per-invocation image name.
fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

/// Nanoseconds since the epoch, for an exec id.
fn epoch_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or_default()
}

/// Re-exported so [`ControlPlane`] is nameable in this module's docs.
#[allow(unused_imports, reason = "named in the module documentation above")]
use ControlPlane as _DocsOnly;

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell command becomes a one-element argv with `shell: true`.
    ///
    /// The alternative — `shell: false` with the same one-element command — silently looks for
    /// a binary literally named `ls -la`, because the daemon never whitespace-splits.
    #[test]
    fn a_shell_command_is_one_element_with_the_shell_flag_set() {
        let request = start_request("pytest -q && echo done", Some("/workspace".into()));
        assert!(request.shell, "a shell line needs the shell flag");
        assert_eq!(request.command, ["pytest -q && echo done"]);
        assert_eq!(request.cwd.as_deref(), Some("/workspace"));
        assert!(
            !request.stdin,
            "an exec with no writer must not hold a pipe"
        );
    }

    /// Two exec ids from one process differ.
    ///
    /// The id is the daemon's idempotency key: two execs sharing one means the second is
    /// answered from the first's record and the caller reads someone else's output.
    #[test]
    fn two_exec_ids_differ() {
        let first = start_request("a", None).exec_id;
        let second = start_request("b", None).exec_id;
        assert_ne!(first, second);
        assert!(first.starts_with("x-"), "{first}");
    }

    /// The wait carries the caller's deadline and never a negative one.
    ///
    /// `Duration::from_secs_f64` panics on a negative, and `--timeout -1` is a thing someone
    /// types.
    #[test]
    fn a_negative_timeout_becomes_zero_rather_than_a_panic() {
        assert_eq!(wait_opts(-5.0).timeout, Duration::ZERO);
        assert_eq!(wait_opts(300.0).timeout, Duration::from_secs(300));
        assert_eq!(wait_opts(300.0).poll_interval, Duration::from_secs(5));
    }
}

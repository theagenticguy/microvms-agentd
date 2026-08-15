// SPDX-License-Identifier: Apache-2.0
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
//! rather than about prose.
//!
//! # There is no `Sandbox::attach`, and adding one would cost the proofs
//!
//! The obvious tidy-up is a `Sandbox::attach(control, microvm_id)` that hands these three
//! commands the same type `run` and `build` use. It was assessed and refused, and the reason
//! is not the one that looks obvious.
//!
//! The obvious reason is STATE-12: `suspendedDurationSeconds` exists only in the
//! `RunMicrovm` **request**, `GetMicrovm` does not return it, so a process that did not send
//! the launch cannot know the window. That is true, and on its own it would only mean an
//! attached sandbox carries `suspended_window: None` and lets the service answer — which is
//! exactly what [`microvms_core::sandbox::Sandbox`]'s `require_open_suspended_window`
//! already does for that case, and what `cli.py:1756` chose for the same stated reason. A
//! documented limitation, not a blocker.
//!
//! The real reason is the **initial state**. `spec/core.symspec.json`'s state model declares
//! exactly one — `vm_state = PENDING and token_installed = false and image_exists = false
//! and was_terminated = false and bootstrap_count = 0` — and `model/src/client.rs`'s
//! `init_states` returns exactly that one state. Every claim proved over either model is a
//! claim about what is *reachable from there*: "bootstrap happens at most once" is a
//! statement about paths out of `bootstrap_count = 0`, and "TERMINATED never returns to
//! RUNNING" is a statement about paths out of PENDING. An `attach` constructor would
//! manufacture a sandbox at `vm_state = RUNNING, token_installed = true, bootstrap_count =
//! 1` — a second initial state neither model enumerates, so neither proof would say anything
//! about it. That is not a limitation to document; it is a set of green checks that quietly
//! stop covering the code they name.
//!
//! And the thing the constructor was supposed to buy is already here, in a better form.
//! [`suspend`] below reads the state with `GetMicrovm` and refuses locally from anything but
//! RUNNING — STATE-5's local half, on the attached path, costing one read. An attached
//! `Sandbox` could not do better: it would have to be *told* its lifecycle at construction,
//! and a constructor with a `lifecycle` parameter is a constructor a caller can lie to. The
//! private fields exist precisely so that nobody can, so a door that takes the state as an
//! argument is the one door that reopens what they close. The service's answer is the
//! stronger source here, not the weaker one.
//!
//! What the attached path therefore gives up, in full: no `Drop` warning naming an
//! abandoned VM (nothing owns the id past the command), no launch-time window guard (there
//! is no window to guard — see above), and no `bootstrap_count`, which is correct because
//! this invocation bootstrapped nothing. Each of those is a property of *having launched*,
//! and a process that did not launch has none of them to lose.
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
//!
//! (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)

use std::time::Duration;

use microvms_core::control::{ControlPlane, CreateImageRequest, WaitOpts};
use microvms_core::sandbox::{RunRequest, Sandbox, TeardownOpts, TeardownReport};
use microvms_core::{Error, ErrorKind};
use serde_json::{Map, json};

use crate::cli::{BuildArgs, ResumeArgs, RunArgs, SuspendArgs, TerminateArgs};
use crate::commands::{Ctx, Rendered, response_type};
use crate::exit::Exit;
use crate::ledger::Ledger;
use crate::render::RunOutcome;
use crate::seam::state_dir;

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
    let region = args.region.resolve(ctx.env)?;
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
            // A bare name is resolved to its ARN through the image listing before the
            // launch: `RunMicrovm.imageIdentifier` takes an ARN, and a name sent verbatim
            // is answered with HTTP 400 "Malformed ARN" — a message that says nothing
            // about names. An identifier already shaped like an ARN passes through with
            // zero extra calls (core checks the prefix first), so a caller who holds the
            // ARN pays nothing for the convenience existing.
            let resolved = sandbox.resolve_image_arn(identifier).await?;
            if resolved != *identifier {
                ctx.out
                    .progress(&format!("resolved image name {identifier} to {resolved}"));
            }
            ctx.out
                .progress(&format!("launching from the existing image {resolved}"));
            resolved
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
    // One pair at a time through the builder rather than assigning the collected vector,
    // so the flag's repeatability and the map's key-uniqueness meet in one place: a
    // caller who passes the same key twice gets the last value, the way every other
    // KEY=VALUE surface behaves.
    for (key, value) in &args.launch_env {
        request = request.with_launch_env(key, value);
    }
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
        let request = start_request(StartSpec::command(&command));
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
    // The token the sandbox minted (or was given): without it the envelope's
    // agentToken is null and `run --keep` hands the caller a VM they cannot
    // exec into — the first live run found exactly that, as a bootstrap
    // replay that answered 409 to a token spelled "None".
    outcome.agent_token = sandbox
        .session()
        .map(|session| session.agent_token().to_string());
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

/// Builds an image and waits for it to be usable — or, under `--reuse`, finds the one
/// whose content hash already built.
///
/// # How `--reuse` decides
///
/// The name is derived, not chosen: `<prefix>-<hash12>`, where the prefix is `--name` (or
/// the stable stem `microvm-cli`) and the hash is a sha256 over the build inputs — the
/// binary's bytes and the Dockerfile. The listing is then checked for that **exact** name:
/// a hit skips the build and the upload entirely, and the envelope reports the existing
/// image with `reused: true`; a miss builds under the derived name, so the *next*
/// invocation with the same inputs hits.
///
/// The prefix default is `microvm-cli` rather than the per-invocation
/// `microvm-cli-<epoch>` the plain path uses, deliberately: a name containing a timestamp
/// never matches across invocations, which would make `--reuse` a flag that always
/// misses. The hash supplies the uniqueness the timestamp supplied — and unlike a
/// timestamp it collides exactly when reuse is correct.
///
/// # Why the hash is in the name at all
///
/// Recreating an image under a previously-used fixed name can serve a stale snapshot
/// (measured; the same hazard class as the clientToken replay in docs/PLATFORM.md).
/// Content-keying the name closes that: unchanged inputs reuse, changed inputs get a
/// fresh name and therefore a fresh build under it.
pub async fn build<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &BuildArgs,
) -> Result<Rendered, crate::exit::CliError> {
    let region = args.region.resolve(ctx.env)?;
    ctx.infra.require(&["build_role_arn"])?;
    if !args.binary.exists() {
        return Err(crate::exit::CliError::new(
            Exit::Precondition,
            format!("daemon binary not found: {}", args.binary.display()),
        )
        .suggest("cargo build --release -p agentd --target aarch64-unknown-linux-musl"));
    }
    let size = args.memory.size_class();
    let seed = if args.reuse {
        // A stable stem: see the function docs on why the epoch default would make
        // `--reuse` a flag that always misses.
        args.name
            .clone()
            .unwrap_or_else(|| "microvm-cli".to_string())
    } else {
        args.name
            .clone()
            .unwrap_or_else(|| format!("microvm-cli-{}", epoch_secs()))
    };

    let sandbox = ctx.seam.open_sandbox(region, args.port).await?;
    let mut request = build_request_from(
        ctx,
        &seed,
        size,
        &args.binary,
        args.dockerfile.as_deref(),
        args.repair_identity,
        args.artifact_uri.as_deref(),
    )?;

    let name;
    if args.reuse {
        let hash = sandbox.artifact_content_hash_for(&request);
        name = format!("{seed}-{}", &hash[..12]);
        // The request was built under the seed; the derived name replaces it everywhere
        // the seed landed — the name, the token label, and the derived artifact key (but
        // not a caller-supplied --artifact-uri, which is theirs).
        request.name = name.clone();
        request.token_scope = Some(name.clone());
        if args.artifact_uri.is_none()
            && let Some(bucket) = ctx.infra.bucket.as_deref()
        {
            request.code_artifact_uri = format!("s3://{bucket}/{name}.zip");
        }
        ctx.out.progress(&format!(
            "checking for an existing image named {name} (content hash {})",
            &hash[..12]
        ));
        if let Some(existing) = sandbox.find_image_by_name(&name).await? {
            ctx.out.progress(&format!(
                "reusing {} — the build inputs are unchanged, so no build was started",
                existing.image_arn
            ));
            return Ok(render_build(
                &existing.image_arn,
                &name,
                size,
                true,
                &format!(
                    "{}/{name}",
                    microvms_core::control::image::BUILD_LOG_GROUP_PREFIX
                ),
            ));
        }
        ctx.out
            .progress(&format!("no image named {name}; building it"));
    } else {
        name = seed;
    }

    let mut sandbox = sandbox;
    ctx.out.progress(&format!("building image {name} ({size})"));
    upload_artifact(ctx, &sandbox, &request).await?;
    let image = sandbox.build_image(request).await?;
    Ok(render_build(
        &image.identifier,
        &image.name,
        size,
        false,
        &image.build_log_group(),
    ))
}

/// The `build` envelope, shared by the built and the reused outcomes so the two cannot
/// carry different keys.
///
/// `reused` is always present — `false` for a plain build — so a consumer never has to
/// guard against a missing key. `size` on a reused image is the *requested* class, and
/// the text says so: the class an existing image was created with is not observable from
/// the listing, and `--memory` is deliberately not part of the reuse identity.
fn render_build(
    identifier: &str,
    name: &str,
    size: microvms_core::SizeClass,
    reused: bool,
    build_log_group: &str,
) -> Rendered {
    let mut data = Map::new();
    data.insert("imageIdentifier".into(), json!(identifier));
    data.insert("imageName".into(), json!(name));
    // Named in the payload because the *service* creates it, Terraform never owns it, and
    // `terraform destroy` leaves it behind — so the caller who built this image is the only
    // one who will ever know to delete it.
    data.insert("buildLogGroup".into(), json!(build_log_group));
    data.insert("size".into(), json!(size.to_string()));
    data.insert("reused".into(), json!(reused));

    let (kind, _) = response_type("build");
    let dense = format!("{identifier}\t{name}\t{build_log_group}");
    let mut lines = vec![format!("image: {identifier}"), format!("name: {name}")];
    if reused {
        lines.push(
            "reused: yes — the content hash matched an existing image; nothing was built"
                .to_string(),
        );
        lines.push(format!(
            "size: {size} (requested; a reused image keeps the class it was created with)"
        ));
    } else {
        lines.push(format!("size: {size}"));
    }
    lines.push(format!("build log group: {build_log_group}"));
    lines.push(
        "note: the service created that log group; terraform destroy will not remove it"
            .to_string(),
    );
    Rendered::ok(kind, data, lines.join("\n"), dense)
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

// ── the shared start request ─────────────────────────────────────────────────

/// What a caller asks of one exec. See [`start_request`].
///
/// A struct rather than four parameters, because two of the four are `Option<String>` and
/// `start_request(cmd, cwd, exec_id)` is how a working directory ends up as an idempotency key.
/// `run`'s own exec fills only `command` and takes the defaults for the rest.
pub struct StartSpec<'a> {
    /// The shell line to run.
    pub command: &'a str,
    /// Omitted inherits the image WORKDIR, which is not the same as `/`.
    pub cwd: Option<String>,
    /// A caller-supplied stable id, or `None` for a fresh one. See [`start_request`].
    pub exec_id: Option<String>,
    /// Whether the child gets a writable stdin pipe.
    pub stdin: bool,
    /// The child's whole environment. The daemon starts every exec from `env_clear()` — the
    /// agent token must never leak into a child — so this map is not merged into anything:
    /// what is here is everything the child sees.
    pub env: std::collections::HashMap<String, String>,
    /// Numeric uid to demote to. `None` runs as the daemon's own user.
    pub user: Option<u32>,
    /// Numeric gid to demote to. `None` keeps the daemon's own group.
    pub group: Option<u32>,
}

impl<'a> StartSpec<'a> {
    /// The one-shot shape: a command, nothing else.
    pub fn command(command: &'a str) -> Self {
        Self {
            command,
            cwd: None,
            exec_id: None,
            stdin: false,
            env: std::collections::HashMap::new(),
            user: None,
            group: None,
        }
    }
}

/// A start request for one shell command.
///
/// `shell: true` with a single-element command, which is the `run_sync(command, shell=True)`
/// shape `cli.py` uses. A bare string with `shell: false` would become a one-element argv —
/// never whitespace-split — so passing a shell line that way silently looks for a binary
/// named `ls -la`.
///
/// The type comes from the `protocol` crate rather than from `microvms_core`. Core does
/// re-export it (`pub use protocol;`), so this is no longer forced — the reason beside that
/// dependency in `Cargo.toml` says why the direct edge stays: it resolves identically either
/// way, it is ARCH-2's own contract, and `tests/thinness.rs` allowlists it by name.
///
/// Shared with [`crate::commands::attached`] rather than duplicated, because the field this
/// deliberately leaves unset — `timeout_sec` — is a decision with a reason, and a second
/// constructor is where it silently acquires a different answer.
pub fn start_request(spec: StartSpec<'_>) -> microvms_core::protocol::exec::StartRequest {
    // Every field written out rather than `..Default::default()`, and not only because
    // `StartRequest` has no `Default`: this struct is the wire contract, so a field added on the
    // daemon side should break this build and make someone decide what the CLI sends. A struct
    // update would have silently defaulted it.
    microvms_core::protocol::exec::StartRequest {
        // The idempotency key, generated unless the caller supplied one. The default is fresh and
        // that is the safe direction: a reused id is answered from the first exec's record, so the
        // second caller reads someone else's output. `exec --exec-id` is the opt-in for a caller
        // whose retry must survive its own restart — the daemon returns success for a known id
        // without spawning a second child, which is the whole value of a key.
        exec_id: spec
            .exec_id
            .unwrap_or_else(|| format!("x-{:016x}", epoch_nanos())),
        command: vec![spec.command.to_string()],
        shell: true,
        cwd: spec.cwd,
        // Verbatim, not merged: the daemon `env_clear()`s before applying this map
        // (`agentd/src/exec.rs:1003`), so the caller's `--env` flags are the child's whole
        // environment and there is nothing on this side to merge them into.
        env: spec.env,
        // Forwarded as the numbers the caller gave, unvalidated. The earlier reason for
        // leaving these `None` — "a uid flag on this surface would be a number with no way to
        // check it means anything in that guest" — still holds as far as it goes, but it
        // holds equally against the Python and Node bindings, which do expose them; the guest's
        // uid space is unknowable from *any* client, and the daemon's spawn failure for a uid
        // it cannot assume is the real check. What the reason bought was parity-breaking
        // caution, not a guard: `--user`/`--group` now forward, and omission stays the
        // default, which is "run as the daemon's own user".
        user: spec.user,
        group: spec.group,
        // The client-side deadline is the caller's `--timeout`, applied by `run_sync`. Sending it
        // as the *daemon's* budget too would kill the child at a deadline the caller cannot see
        // in the exit code.
        timeout_sec: None,
        // Opt-in, and `run`'s exec never asks: a child holding an open stdin pipe nobody will
        // ever write to is a child that blocks forever the first time it reads. `exec --stdin`
        // sets this *and* feeds the pipe, which is the only combination that is safe.
        stdin: spec.stdin,
    }
}

// ── suspend / resume / terminate (the attached path) ─────────────────────────

/// Freezes a MicroVM.
///
/// Reads the state first and refuses locally from anything but RUNNING. That costs one
/// `GetMicrovm` where [`Sandbox::suspend`] costs none — a launched sandbox already knows its
/// lifecycle, and this one cannot — and it is still worth doing: `SuspendMicrovm` against a
/// non-running id answers about the id rather than saying which of two things the caller got
/// wrong, and a suspend issued from SUSPENDED is a caller who believes they resumed.
///
/// This read is also what makes STATE-5's local half hold on the attached path at all, and
/// the module docs' assessment of a `Sandbox::attach` constructor turns on it: the service's
/// answer is a stronger source for "is this RUNNING" than a lifecycle a constructor would have
/// to be handed.
pub async fn suspend<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &SuspendArgs,
) -> Result<Rendered, crate::exit::CliError> {
    let region = args.region.resolve(ctx.env)?;
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
    let region = args.region.resolve(ctx.env)?;
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
    let region = args.region.resolve(ctx.env)?;
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
        let request = start_request(StartSpec {
            cwd: Some("/workspace".into()),
            ..StartSpec::command("pytest -q && echo done")
        });
        assert!(request.shell, "a shell line needs the shell flag");
        assert_eq!(request.command, ["pytest -q && echo done"]);
        assert_eq!(request.cwd.as_deref(), Some("/workspace"));
        assert!(
            !request.stdin,
            "an exec with no writer must not hold a pipe"
        );
    }

    /// Two exec ids from one process differ, **unless** the caller named one.
    ///
    /// Both halves, because they are the two sides of one decision. The generated id must differ
    /// per invocation: two execs sharing one means the second is answered from the first's record
    /// and the caller reads someone else's output. And `--exec-id` must be forwarded *verbatim*,
    /// because the entire value of an idempotency key is that the retry sends the identical one —
    /// a key this CLI decorated (prefixed, suffixed, hashed) would address a different exec on the
    /// retry and spawn the second child the key exists to prevent.
    #[test]
    fn a_generated_exec_id_is_fresh_and_a_supplied_one_is_forwarded_verbatim() {
        let first = start_request(StartSpec::command("a")).exec_id;
        let second = start_request(StartSpec::command("b")).exec_id;
        assert_ne!(first, second);
        assert!(first.starts_with("x-"), "{first}");

        let stable = start_request(StartSpec {
            exec_id: Some("conformance-retry-1".into()),
            ..StartSpec::command("a")
        });
        assert_eq!(
            stable.exec_id, "conformance-retry-1",
            "a decorated key addresses a different exec on the retry, which spawns the second \
             child the key exists to prevent"
        );
        // And twice, so the forwarding is not merely a pass-through of the first call.
        assert_eq!(
            start_request(StartSpec {
                cwd: Some("/elsewhere".into()),
                exec_id: Some("conformance-retry-1".into()),
                stdin: true,
                ..StartSpec::command("different command entirely")
            })
            .exec_id,
            "conformance-retry-1"
        );
    }

    /// `stdin: true` reaches the wire only when it was asked for.
    ///
    /// The opt-in property, at the one place it is decided. A default of `true` would give every
    /// task command a surprise open descriptor, and the first tool that probes for input would
    /// behave differently for a reason nobody could see.
    #[test]
    fn a_stdin_pipe_is_requested_only_when_asked_for() {
        assert!(!start_request(StartSpec::command("cat")).stdin);
        assert!(
            start_request(StartSpec {
                stdin: true,
                ..StartSpec::command("cat")
            })
            .stdin
        );
    }

    /// The parsed `--env` map and the `--user`/`--group` numbers reach the request verbatim.
    ///
    /// Verbatim is the property: keys and values must not be swapped, decorated, or merged
    /// into anything, because the daemon `env_clear()`s and applies exactly this map — a
    /// mangled key here is a variable the child silently does not have. Keys and values are
    /// deliberately distinguishable strings, so a swap fails on both assertions rather than
    /// passing by symmetry.
    ///
    /// **Guard proof.** Swap key and value in the collection feeding `spec.env` (build the map
    /// as `(v, k)`) and the `PATH` lookup below reads `None`; drop `user: spec.user` back to
    /// `None` and the uid assertion goes red. Both were done on 2026-08-14 and both failed as
    /// stated, then were restored.
    #[test]
    fn env_user_and_group_reach_the_start_request_verbatim() {
        let request = start_request(StartSpec {
            env: std::collections::HashMap::from([
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("EMPTY".to_string(), String::new()),
            ]),
            user: Some(1000),
            group: Some(2000),
            ..StartSpec::command("env")
        });
        assert_eq!(request.env.len(), 2);
        assert_eq!(
            request.env.get("PATH").map(String::as_str),
            Some("/usr/bin:/bin"),
            "the key must stay the key: a swap makes a variable the child silently lacks"
        );
        assert_eq!(
            request.env.get("EMPTY").map(String::as_str),
            Some(""),
            "an empty value survives to the wire; it is not the same as unset"
        );
        assert_eq!(request.user, Some(1000));
        assert_eq!(request.group, Some(2000));

        // And the defaults stay the defaults: no demotion, empty environment. The one-shot
        // constructor is what `run --exec` uses, so a stray value here would give every run's
        // exec an environment nobody asked for.
        let bare = start_request(StartSpec::command("true"));
        assert!(bare.env.is_empty());
        assert_eq!(bare.user, None);
        assert_eq!(bare.group, None);
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

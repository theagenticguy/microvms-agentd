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
use crate::history::{Event, History};
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

/// `run`'s arguments after the config merge, plus the `resolvedConfig` report.
///
/// One struct so the merge happens exactly once, before anything is attempted, and the
/// rest of `run` reads effective values without knowing a file exists. `resolved` is the
/// envelope's `resolvedConfig`: each knob's winning value and the source it came from
/// (`flag`, `config`, `env` — the region only — or `default`), because "which source won"
/// is a question a caller should read off the answer rather than re-derive from the
/// precedence rules.
pub struct MergedRunArgs {
    pub args: RunArgs,
    pub config_path: Option<std::path::PathBuf>,
    /// The artifact globs from the file's `artifacts` key. Validated by the loader;
    /// consumed by `run <DIR>`'s download selection (issue #72), which is the only
    /// consumer a glob list has — a plain `run` brings nothing back.
    pub artifacts: Vec<String>,
    pub resolved: serde_json::Map<String, serde_json::Value>,
}

/// Applies `microvm.toml` to `run`'s arguments: flags win, then the file, then defaults.
///
/// Fails with `ERR_CONFIG` — before any billable call — when the file cannot be used, and
/// with the *flag's* vocabulary when a file value is outside the flag's domain, because
/// the file must not be a quieter side door past the parser's closed sets.
///
/// `env` is here for the report's sake, not the merge's: the region is the one knob whose
/// chain continues past the file into `$AWS_REGION`/`$AWS_DEFAULT_REGION`, and a
/// `resolvedConfig` that answered `default` while the launch went where the environment
/// pointed would be the report re-deriving wrongly — the one thing it exists to prevent.
pub fn merge_config(
    args: &RunArgs,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<MergedRunArgs, crate::exit::CliError> {
    let loaded = crate::config::load(
        args.config.config.as_deref(),
        args.config.no_config,
        std::path::Path::new("."),
    )
    .map_err(config_error)?;
    let (config_path, config) = match loaded {
        Some((path, config)) => (Some(path), config),
        None => (None, crate::config::ProjectConfig::default()),
    };

    let mut merged = args.clone();
    let mut resolved = Map::new();
    let mut report = |knob: &str, value: serde_json::Value, source: crate::config::Source| {
        resolved.insert(
            knob.to_string(),
            json!({"value": value, "source": source.as_str()}),
        );
    };

    // The Option-shaped knobs: typed is `is_some()`, and there is no built-in default to
    // label — an absent one resolves to `null` from `default`.
    //
    // `image` and `binary` are one decision wearing two knobs: `run` builds exactly when
    // the merged `image` is absent. So a *typed* BINARY positional with no typed `--image`
    // suppresses the file's `image` — the caller who wrote `microvm run ./fresh-agentd` in
    // a project whose file pins `image` asked to build and launch that binary, and a file
    // that silently won would run their tests against the stale pinned image. This is the
    // documented precedence, not an exception to it: the positional is the flag layer for
    // the pair. A positional that names a *directory* is sync mode (issue #72), which
    // launches rather than builds — there the file's `image` is exactly what `run .`
    // wants, so the suppression checks the path's shape.
    let positional_is_binary = args.binary.as_deref().is_some_and(|path| !path.is_dir());
    let image_config = if positional_is_binary && args.image.is_none() {
        None
    } else {
        config.image.clone()
    };
    let image = crate::config::pick(
        args.image.is_some(),
        args.image.clone(),
        image_config.map(Some),
    );
    report("image", json!(image.value), image.source);
    merged.image = image.value;

    // The typed positional also beats the file's `binary`; and when the file's `image`
    // won (nothing typed for the pair), the file's `binary` is dead weight the launch
    // ignores — merged anyway so the report stays honest about where each value came from.
    let binary = crate::config::pick(
        args.binary.is_some(),
        args.binary.clone(),
        config.binary.map(Some),
    );
    report(
        "binary",
        json!(binary.value.as_ref().map(|p| p.display().to_string())),
        binary.source,
    );
    merged.binary = binary.value;

    let exec = crate::config::pick(
        args.exec.is_some(),
        args.exec.clone(),
        config.exec.map(Some),
    );
    report("exec", json!(exec.value), exec.source);
    merged.exec = exec.value;

    // The clap-defaulted knobs: typed is the `value_source` answer `args.explicit`
    // carries, because the parsed field holds a value either way.
    // Already validated by the loader; the expect documents the invariant.
    let memory_config = config.memory.map(|mib| {
        crate::cli::memory_from_mib(mib).expect("config::load validated the memory domain")
    });
    let memory = crate::config::pick(args.explicit.memory, args.memory, memory_config);
    report(
        "memory",
        json!(memory.value.size_class().baseline_mib()),
        memory.source,
    );
    merged.memory = memory.value;

    let max_idle = crate::config::pick(
        args.explicit.max_idle_sec,
        args.max_idle_sec,
        config.max_idle_sec,
    );
    report("maxIdleSec", json!(max_idle.value), max_idle.source);
    merged.max_idle_sec = max_idle.value;

    let suspended = crate::config::pick(
        args.explicit.suspended_sec,
        args.suspended_sec,
        config.suspended_sec,
    );
    report("suspendedSec", json!(suspended.value), suspended.source);
    merged.suspended_sec = suspended.value;

    let max_duration = crate::config::pick(
        args.explicit.max_duration_sec,
        args.max_duration_sec,
        config.max_duration_sec,
    );
    report(
        "maxDurationSec",
        json!(max_duration.value),
        max_duration.source,
    );
    merged.max_duration_sec = max_duration.value;

    // `--egress` is SetTrue: parsed `true` is the evidence it was typed, so the file can
    // turn egress on for a project but a flag can only add it, never subtract — which is
    // the direction a boolean flag can express, and `egress = false` in a file is the
    // default restated rather than an override.
    let egress = crate::config::pick(args.egress, args.egress, config.egress);
    report("egress", json!(egress.value), egress.source);
    merged.egress = egress.value;

    // `--auto-resume` is SetTrue like `--egress` and merges the same way: the file can
    // enable it for a project, the flag can only add it, and `auto-resume = false` in a
    // file is the default restated rather than an override.
    let auto_resume = crate::config::pick(args.auto_resume, args.auto_resume, config.auto_resume);
    report("autoResume", json!(auto_resume.value), auto_resume.source);
    merged.auto_resume = auto_resume.value;

    // The region: a config value joins the flag chain *above* the environment, because the
    // file is project state and the environment is machine state. The closed set only —
    // the loader already refused an unlisted name with the flag's own remedy (and doctor
    // validates through the same loader, so the two commands cannot disagree), which is
    // why this is an expect rather than a second refusal.
    let region_config = config.region.as_deref().map(|name| {
        crate::cli::RegionArg::from_name(name).expect("config::load validated the region domain")
    });
    let region = crate::config::pick(
        args.region.region.is_some() || args.region.unlisted_region.is_some(),
        args.region.region,
        region_config.map(Some),
    );
    if args.region.unlisted_region.is_none() {
        merged.region.region = region.value;
    }
    // The report continues down the chain the run itself will walk: past the file sit
    // `$AWS_REGION`/`$AWS_DEFAULT_REGION`, then the built-in. A report that said
    // `default: null` while the launch went where the environment pointed would be the
    // report lying about the one knob whose chain does not end at the file.
    let (region_value, region_source) = match (
        merged
            .region
            .region
            .map(|r| r.region().as_str().to_string())
            .or_else(|| merged.region.unlisted_region.clone()),
        region.source,
    ) {
        (Some(value), source) => (Some(value), source),
        (None, _) => match env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION")) {
            // Reported as the environment's word, unvalidated: `resolve` refuses an
            // unlisted name later with the remedy attached, and this report must not
            // pre-empt that refusal by pretending the value was something else.
            Some(name) => (Some(name), crate::config::Source::Env),
            None => (
                Some(microvms_core::Region::UsEast1.as_str().to_string()),
                crate::config::Source::Default,
            ),
        },
    };
    report("region", json!(region_value), region_source);

    // The launch env, merged per key with the flag pair winning its own key.
    let env_source = match (args.launch_env.is_empty(), &config.env) {
        (false, _) => crate::config::Source::Flag,
        (true, Some(_)) => crate::config::Source::Config,
        (true, None) => crate::config::Source::Default,
    };
    merged.launch_env = crate::config::merge_env(&args.launch_env, config.env.as_ref());
    report(
        "launchEnv",
        json!(
            merged
                .launch_env
                .iter()
                .map(|(key, value)| (key.clone(), json!(value)))
                .collect::<Map<_, _>>()
        ),
        env_source,
    );

    // The logging pair: Option-shaped knobs, so typed is `is_some()`. The file's values
    // were already validated by the loader with the flags' own vocabulary; the
    // stream-needs-a-group rule is re-checked on the *merged* pair, because a flag stream
    // over a file with no group is a combination neither layer saw alone.
    let log_group = crate::config::pick(
        args.log_group.is_some(),
        args.log_group.clone(),
        config.log_group.clone().map(Some),
    );
    report("logGroup", json!(log_group.value), log_group.source);
    merged.log_group = log_group.value;

    let log_stream = crate::config::pick(
        args.log_stream.is_some(),
        args.log_stream.clone(),
        config.log_stream.clone().map(Some),
    );
    report("logStream", json!(log_stream.value), log_stream.source);
    merged.log_stream = log_stream.value;

    if merged.log_stream.is_some() && merged.log_group.is_none() {
        return Err(crate::exit::CliError::new(
            Exit::InvalidArg,
            "--log-stream needs a log group: the stream lives inside the group, and \
             without one the service creates a group with random stream names, so the \
             configured stream would name a location that does not exist.",
        )
        .suggest("pass --log-group, or set `log-group` in microvm.toml"));
    }

    // Artifact globs have no flag spelling, so the file is their only source.
    let artifacts = config.artifacts.clone().unwrap_or_default();
    report(
        "artifacts",
        json!(artifacts),
        if config.artifacts.is_some() {
            crate::config::Source::Config
        } else {
            crate::config::Source::Default
        },
    );

    Ok(MergedRunArgs {
        args: merged,
        config_path,
        artifacts,
        resolved,
    })
}

/// A [`crate::config::ConfigError`] as the `ERR_CONFIG` row, with the remedy attached.
fn config_error(error: crate::config::ConfigError) -> crate::exit::CliError {
    crate::exit::CliError::new(Exit::Config, error.to_string())
        .suggest("`microvm doctor` validates the config file alongside every other prerequisite")
        .suggest("--no-config ignores the file for this invocation")
}

/// A [`crate::sync::SyncError`] as the `ERR_SYNC` row.
fn sync_error(error: crate::sync::SyncError) -> crate::exit::CliError {
    crate::exit::CliError::new(Exit::Sync, error.to_string())
        .suggest("the failure is on this machine's filesystem; the platform was not involved")
}

/// Everything `run <DIR>`'s sync needs past the launch: where the tree came from, its
/// packed bytes, and which members to bring back.
struct SyncPlan {
    dir: std::path::PathBuf,
    archive: Vec<u8>,
    members: usize,
    globs: Vec<String>,
}

/// Build, launch, exec, report, tear down — the whole thing, once.
pub async fn run<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    caller_args: &RunArgs,
    interrupt: Interrupt<'_>,
) -> Result<Rendered, crate::exit::CliError> {
    // The config merge, before anything else: a broken file must cost zero AWS calls, and
    // everything below reads the effective values without knowing a file exists.
    let MergedRunArgs {
        mut args,
        config_path,
        artifacts,
        resolved,
    } = merge_config(caller_args, ctx.env)?;
    if let Some(path) = &config_path {
        ctx.out
            .progress(&format!("using project config {}", path.display()));
    }

    // `run <DIR>` (issue #72): a positional that names a directory is a project to sync,
    // not a binary to bake. Decided on the *typed* positional, never a config `binary` —
    // a file key is a path to a daemon binary by schema, and a directory it accidentally
    // names should fail the binary check loudly rather than silently switch modes. Packed
    // here, before anything is attempted: a tree the pack cannot read must cost zero AWS
    // calls, exactly like a broken config file.
    let sync_dir = match &caller_args.binary {
        Some(path) if path.is_dir() => Some(path.clone()),
        _ => None,
    };
    let mut packed: Option<crate::sync::Packed> = None;
    if let Some(dir) = &sync_dir {
        args.binary = None; // the positional was a directory, not a binary to build from
        if args.image.is_none() {
            return Err(crate::exit::CliError::new(
                Exit::Precondition,
                format!(
                    "{} is a directory (sync mode), and sync mode launches an existing image: \
                     there is no binary to build one from.",
                    dir.display()
                ),
            )
            .suggest("pass --image <arn-or-name>, or pin `image` in microvm.toml"));
        }
        let work = crate::sync::pack(dir).map_err(sync_error)?;
        ctx.out.progress(&format!(
            "packed {} ({} member(s), {} byte(s))",
            dir.display(),
            work.members,
            work.archive.len()
        ));
        packed = Some(work);
    }
    let args = &args;

    let region = args.region.resolve(ctx.env)?;
    let size = args.memory.size_class();
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| format!("microvm-cli-{}", epoch_secs()));

    // The VM name's two local refusals, before anything else: a credential resolution can
    // hang, so even the zero-billable-call guarantee undersells why this goes first — a
    // caller with a bad name should learn it instantly. Grammar first (an illegal name is
    // ERR_INVALID_ARG, fixed by editing the flag), then the collision (ERR_NAME_TAKEN, its
    // own row, fixed by terminating the holder or picking another name). Both cost one file
    // read and zero AWS calls, which is the acceptance criterion on the registry itself.
    if let Some(vm_name) = &args.vm_name {
        if let Err(reason) = crate::ledger::validate_name(vm_name) {
            return Err(crate::exit::CliError::new(Exit::InvalidArg, reason)
                .suggest("names take ASCII letters, digits, `-` and `_`, up to 128 bytes"));
        }
        let names = crate::ledger::Names::new(&state_dir(args.state_dir.clone(), ctx.env));
        if let Some(holder) = names.lookup(vm_name) {
            return Err(crate::exit::CliError::new(
                Exit::NameTaken,
                format!(
                    "the name {vm_name:?} is registered to {} — refused locally, before any \
                     AWS call. A name addresses exactly one live VM; reusing it would point \
                     every later `--name {vm_name}` at whichever registration came last.",
                    if holder.microvm_id.is_empty() {
                        "a torn record (a process died mid-register; inspect the file)".to_string()
                    } else {
                        holder.microvm_id.clone()
                    },
                ),
            )
            .suggest(format!(
                "`microvm terminate {vm_name}` tears that VM down and frees the name"
            ))
            .suggest("or pick another name — the registry is one file per name")
            .with_data("vmName", json!(vm_name))
            .with_data("microvmId", json!(holder.microvm_id)));
        }
    }

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
    //
    // A building run with **no binary at all** provisions one (`provision.rs`): the daemon
    // is this product's own component, and `microvm run --exec "…"` on a fresh machine is
    // the headline the provisioning chain exists for. Provisioned *after* the role check
    // deliberately — a missing role refuses in microseconds, and the fetch it pre-empts
    // costs seconds of network.
    let building = args.image.is_none();
    let mut agentd: Option<crate::provision::Resolved> = None;
    let daemon_binary: Option<std::path::PathBuf> = if building {
        ctx.infra
            .require(&["execution_role_arn", "build_role_arn"])?;
        match &args.binary {
            Some(binary) => {
                if !binary.exists() {
                    return Err(crate::exit::CliError::new(
                        Exit::Precondition,
                        format!("daemon binary not found: {}", binary.display()),
                    )
                    .suggest("cargo build --release -p agentd --target aarch64-unknown-linux-musl")
                    .suggest("`microvm doctor --binary <path>` checks the architecture too")
                    .suggest(
                        "or pass no binary at all: the CLI provisions its own version's \
                         release asset",
                    ));
                }
                Some(binary.clone())
            }
            None => {
                let state = state_dir(args.state_dir.clone(), ctx.env);
                let resolved = {
                    let out = &mut *ctx.out;
                    crate::provision::resolve(
                        &state,
                        env!("CARGO_PKG_VERSION"),
                        ctx.env,
                        ctx.fetch,
                        &mut |line| out.progress(line),
                    )?
                };
                let path = resolved.path.clone();
                agentd = Some(resolved);
                Some(path)
            }
        }
    } else {
        ctx.infra.require(&["execution_role_arn"])?;
        None
    };

    let ledger_root = state_dir(args.state_dir.clone(), ctx.env);
    let mut ledger = Ledger::new(region.as_str(), &ledger_root);
    // Kept as a string before the sandbox takes the `Region`: the history event below wants
    // the name, and the type is not `Copy`.
    let region_name = region.as_str().to_string();
    let mut sandbox = ctx.seam.open_sandbox(region, args.port).await?;
    let mut outcome = RunOutcome {
        image_name: Some(name.clone()),
        ..RunOutcome::default()
    };
    let mut exec_report: Option<ExecReport> = None;
    let sync_plan = sync_dir
        .as_ref()
        .zip(packed.take())
        .map(|(dir, packed)| SyncPlan {
            dir: dir.clone(),
            archive: packed.archive,
            members: packed.members,
            globs: artifacts.clone(),
        });
    let mut downloaded: Option<Vec<u8>> = None;
    let mut download_error: Option<String> = None;

    // The launch, raced against the interrupt. `Box::pin` so the two arms are the same shape
    // and the select does not need the body to be a named future.
    let launched = {
        let body = Box::pin(launch_and_exec(
            ctx,
            args,
            daemon_binary.as_deref(),
            &mut sandbox,
            &mut ledger,
            &name,
            size,
            &mut outcome,
            &mut exec_report,
            sync_plan.as_ref(),
            &mut downloaded,
            &mut download_error,
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

    // The daemon's hook log, read *before* the teardown because a terminated VM cannot
    // answer, and best-effort throughout: this is the teardown path, and a health fetch
    // that failed must not displace the run's real outcome. Short deadline for the same
    // reason — a wedged endpoint must not hold the teardown hostage for the transport's
    // full 60s. Only attempted when a session is in hand; a launch that never built one
    // has no endpoint to ask.
    let observed_hooks: Vec<microvms_core::protocol::health::HookObservation> =
        match sandbox.session() {
            Some(session) => {
                match tokio::time::timeout(Duration::from_secs(5), session.health()).await {
                    Ok(Ok(health)) => health.hooks,
                    _ => Vec::new(),
                }
            }
            None => Vec::new(),
        };

    // Runs however the block above ended, which is CLI-6. Recorded as leaked *before* the
    // delete is attempted — the other order loses the identifier when the process dies inside
    // the call, which is exactly the interrupt case.
    let teardown = tear_down(ctx, &mut sandbox, &mut ledger, args.keep).await;
    outcome.kept = args.keep;
    outcome.leaked = ledger.record.leaked.clone();

    // The VM's history, keyed by the id the service answered — which is why it is written
    // here rather than as each step happened: `imageBuilt` predates the launch, and until
    // `RunMicrovm` is accepted there is no id to file it under. A launch that never got an
    // id writes nothing, because a history nobody can look up is not a record. Every value
    // below is the platform's (the sandbox's own `Microvm` and the `TeardownReport`), and
    // every append swallows its failures — this is the teardown path.
    if let Some(vm) = sandbox.microvm() {
        let history = History::for_vm(&ledger_root, &vm.id);
        if building && let Some(image) = sandbox.image() {
            history.append(Event::ImageBuilt {
                image_identifier: image.identifier.clone(),
                image_name: image.name.clone(),
            });
        }
        history.append(Event::Launched {
            image_identifier: vm.image_arn.clone(),
            endpoint: vm.endpoint.clone(),
            region: region_name.clone(),
        });
        if let Some(exec) = &exec_report {
            history.append(Event::Exec {
                exec_id: exec.exec_id.clone(),
                exit_code: exec.exit_code,
                truncated: exec.truncated,
                writers_may_be_alive: exec.writers_may_be_alive,
            });
        }
        // The daemon's own hook observations, fetched above while the VM could still
        // answer. Deduplicated on (hook, firedAt) so a `run` against a VM some other
        // command already polled appends only what is new. The run hook is the one a
        // plain launch always produces; validate/ready appear when the snapshot VM's
        // memory carried them into this one.
        crate::history::append_unseen_hooks(&ledger_root, &vm.id, &observed_hooks);
        if !args.keep {
            history.append(Event::Terminated {
                terminate_accepted: teardown.terminate_accepted,
                undeleted: teardown.undeleted.clone(),
            });
        }
    }

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

    // The artifact extraction, only now: the archive is the VM's word and the VM is gone
    // (or kept, but either way past teardown), so a failure *here* is purely this
    // machine's filesystem — `ERR_SYNC`, its own row, and the run around it was fine.
    // `extract_artifacts` writes only glob-selected regular files (never under `.git`)
    // through `unpack_in`'s traversal refusal; anything else in the archive is skipped.
    //
    // The report covers all three shapes a sync run can end in, because each has a
    // different next action: artifacts listed (read them), `error` set (the download
    // failed; the exec result above is still real), or `note` set (no globs configured,
    // so nothing was ever going to come back and nothing was transferred).
    if let Some(plan) = &sync_plan {
        let mut report = Map::new();
        report.insert("workdir".into(), json!(crate::sync::REMOTE_WORKDIR));
        report.insert("uploadedBytes".into(), json!(plan.archive.len()));
        report.insert("uploadedMembers".into(), json!(plan.members));
        let artifacts = match &downloaded {
            Some(archive) => {
                let artifacts = crate::sync::extract_artifacts(archive, &plan.globs, &plan.dir)
                    .map_err(sync_error)?;
                ctx.out.progress(&format!(
                    "brought back {} artifact(s) into {}",
                    artifacts.len(),
                    plan.dir.display()
                ));
                artifacts
            }
            None => {
                if let Some(error) = &download_error {
                    report.insert("error".into(), json!(error));
                } else {
                    report.insert(
                        "note".into(),
                        json!("no artifacts globs configured; the workdir was not downloaded"),
                    );
                }
                Vec::new()
            }
        };
        report.insert(
            "artifacts".into(),
            json!(
                artifacts
                    .iter()
                    .map(|artifact| json!({"path": artifact.path, "bytes": artifact.bytes}))
                    .collect::<Vec<_>>()
            ),
        );
        outcome.sync = Some(serde_json::Value::Object(report));
    }

    // The name registration, only now: the launch succeeded, the teardown was skipped
    // (`--vm-name` requires `--keep`), and every field the record carries is the service's
    // own answer read off the outcome. Registering before this point would leave a name
    // pointing at a VM that failed to launch — and the collision check at the top already
    // guaranteed the slot is free, so the one way `register` fails is the filesystem's.
    // That failure is a hard error rather than a swallowed one, deliberately: the VM is up
    // and billing either way, but a caller who was told "registered" and later finds
    // `--name` answering "no VM named" has a phantom worse than a loud failure now.
    if let Some(vm_name) = &args.vm_name
        && let Some(record) = name_record_for_kept(vm_name, &outcome, &region_name)
    {
        let names = crate::ledger::Names::new(&state_dir(args.state_dir.clone(), ctx.env));
        names.register(&record).map_err(|error| {
            crate::exit::CliError::new(
                Exit::Precondition,
                format!(
                    "the VM launched and is RUNNING, but its name could not be registered: \
                     {error}. Address it by the identifiers below; they are in this \
                     envelope's data.",
                ),
            )
            .with_data("microvmId", json!(record.microvm_id))
            .with_data("endpoint", json!(record.endpoint))
            .with_data("vmName", json!(vm_name))
        })?;
        outcome.vm_name = Some(vm_name.clone());
        ctx.out.progress(&format!(
            "registered name {vm_name} for {}",
            record.microvm_id
        ));
    }

    let (kind, _) = response_type("run");
    let dense = outcome.render(true);
    let text = outcome.render(false);
    let mut data = outcome.to_data();
    // What each knob resolved to and which source won — the file's whole value is that a
    // caller can stop passing flags, so the envelope has to answer "what did this run
    // actually use" without them re-deriving the precedence.
    data.insert("resolvedConfig".into(), serde_json::Value::Object(resolved));
    data.insert(
        "configPath".into(),
        json!(config_path.as_ref().map(|path| path.display().to_string())),
    );
    // Null when the caller supplied the binary themselves — `resolvedConfig.binary`
    // already tells that story, and repeating it here would be two keys for one fact.
    data.insert("agentd".into(), agentd_report(agentd.as_ref()));
    let rendered = Rendered::ok(kind, data, text, dense);
    // A failing workload keeps its success envelope and earns a non-zero code: the sandbox did
    // its job and the output the caller asked for is in `data`. Mapped onto one stable code
    // rather than passed through raw, because a workload exiting 4 must not be
    // indistinguishable from a credential failure.
    if outcome.exec_exit_code.is_some_and(|code| code != 0) {
        return Ok(rendered.reporting(Exit::ExecFailed));
    }
    Ok(rendered)
}

/// `quickstart`: `run` with every decision pre-made (issue #75).
///
/// The run arguments come from **parsing `run`'s own command line** rather than from a
/// hand-built `RunArgs`: a struct literal here would be a second copy of every default,
/// and the copies drift — `--memory`'s default changed once already, and a quickstart
/// still launching the old class would be exactly the stale-first-impression this
/// command exists to prevent. Parsing keeps one source of truth at the cost of one
/// in-process parse.
///
/// Everything else — provisioning the daemon, the preconditions, the teardown-by-default,
/// the envelope — is `run`'s, byte for byte, which is why the envelope keeps the
/// `microvm.run` discriminant: a consumer that learned to read one has learned both.
pub async fn quickstart<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &crate::cli::QuickstartArgs,
    interrupt: Interrupt<'_>,
) -> Result<Rendered, crate::exit::CliError> {
    use clap::Parser as _;
    let parsed = crate::cli::Cli::try_parse_from(["microvm", "run", "--exec", &args.exec])
        .map_err(|error| {
            crate::exit::CliError::new(
                Exit::InvalidArg,
                format!("quickstart could not synthesize its run: {error}"),
            )
        })?;
    let crate::cli::Command::Run(mut run_args) = parsed.command else {
        return Err(crate::exit::CliError::new(
            Exit::Unexpected,
            "quickstart parsed a `run` invocation and got a different command back — a \
             dispatch defect in this binary, not anything the caller did",
        ));
    };
    run_args.region = args.region.clone();
    run_args.infra = args.infra.clone();
    run_args.state_dir = args.state_dir.clone();

    ctx.out.progress(
        "quickstart: provision the daemon, build an image, launch a VM, run the command, \
         report the cost, tear everything down — nothing survives this invocation",
    );
    ctx.out.progress(
        "expect a few minutes on a first run; most of it is the image build, and \
         `microvm run --image <name>` reuses it afterwards",
    );
    run(ctx, &run_args, interrupt).await
}

/// What `run --exec`'s one exec reported, for the history record.
///
/// A separate struct from [`RunOutcome`] because the two answer different callers: the
/// outcome renders the envelope and flattens the daemon's report into it, and history needs
/// the report's own fields — `writers_may_be_alive` in particular, which the envelope never
/// carried and which must stay an `Option` so its absence is an absence.
struct ExecReport {
    exec_id: String,
    exit_code: Option<i32>,
    truncated: bool,
    writers_may_be_alive: Option<bool>,
}

/// The build/launch/exec body `run` races against the interrupt.
///
/// Separated so the `select!` arm is one expression, and because every `?` in here has to be
/// cancellable — which it is, since the only state that must survive a cancellation lives in
/// `sandbox` and `ledger`, both borrowed rather than owned.
#[allow(
    clippy::too_many_arguments,
    reason = "the borrows are the design: everything that must survive a cancelled launch \
              lives with the caller, and a bundling struct would hide which ones do"
)]
async fn launch_and_exec<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &RunArgs,
    daemon_binary: Option<&std::path::Path>,
    sandbox: &mut Sandbox,
    ledger: &mut Ledger,
    name: &str,
    size: microvms_core::SizeClass,
    outcome: &mut RunOutcome,
    exec_report: &mut Option<ExecReport>,
    sync: Option<&SyncPlan>,
    downloaded: &mut Option<Vec<u8>>,
    download_error: &mut Option<String>,
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
            // Resolved by the caller: the typed positional, the config file's `binary`,
            // or the provisioning chain — whichever won, the path is real by here.
            let binary = daemon_binary.expect("checked by the caller");
            ctx.out.progress(&format!("building image {name} ({size})"));
            let request = build_request(ctx, args, name, size, binary)?;
            // Before the upload, so a request core itself would refuse costs zero transport
            // calls — the guards inside `build_image` run after the S3 PUT (issue #47).
            sandbox.preflight(&request)?;
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
    // Pinned only when asked for, so an unpinned launch emits byte-for-byte the request this
    // CLI always sent. Core refuses an illegal `Version` before the call, which is why nothing
    // is validated here — a second check in the CLI would be a second message to keep right.
    request.image_version = args.image_version.clone();
    request.execution_role_arn = ctx.infra.execution_role_arn.clone();
    request.max_idle_sec = args.max_idle_sec;
    request.auto_resume = args.auto_resume;
    request.max_duration_sec = args.max_duration_sec;
    request.token_scope = Some(name.to_string());
    // One pair at a time through the builder rather than assigning the collected vector,
    // so the flag's repeatability and the map's key-uniqueness meet in one place: a
    // caller who passes the same key twice gets the last value, the way every other
    // KEY=VALUE surface behaves.
    for (key, value) in &args.launch_env {
        request = request.with_launch_env(key, value);
    }
    if args.identity {
        request = request.with_identity();
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

    // The synced tree goes up before any exec: the daemon extracts it (its openat2
    // confinement is the upload's whole trust story), and the exec below runs *in* it.
    if let Some(plan) = sync {
        ctx.out.progress(&format!(
            "uploading {} member(s) to {}",
            plan.members,
            crate::sync::REMOTE_WORKDIR
        ));
        sandbox
            .session()
            .expect("run() built one")
            .upload_tar(crate::sync::REMOTE_WORKDIR, &plan.archive)
            .await?;
    }

    let exec = args.exec.clone();
    let timeout = Duration::from_secs_f64(args.timeout.max(0.0));
    if let Some(command) = exec {
        ctx.out.progress(&format!("exec: {command}"));
        // Widened at the call site rather than through a second constructor — the comment on
        // `start_request` names a second constructor as where a field silently acquires a
        // different answer. Demotion is the difference between a working agent and one that
        // refuses its own tools as root, so `run --exec` carries the same `--user`/`--group`
        // that `exec` does, through the same spec.
        let request = start_request(StartSpec {
            user: args.user,
            group: args.group,
            // The synced tree is the working directory: `run . --exec "make test"` means
            // "run make test in my project", and an exec that started in the image's own
            // WORKDIR would make every command spell the path itself.
            cwd: sync.map(|_| crate::sync::REMOTE_WORKDIR.to_string()),
            ..StartSpec::command(&command)
        });
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
        // For the history record, off the daemon's own report — the id it confirmed and the
        // outcome fields it carried, never anything the child printed.
        *exec_report = Some(ExecReport {
            exec_id: result.exec_id.clone(),
            exit_code: result.exit_code(),
            truncated: outcome.truncated,
            writers_may_be_alive: result
                .outcome
                .as_ref()
                .map(|outcome| outcome.writers_may_be_alive),
        });
    }

    // The workdir comes back *here*, after the exec block rather than inside its success
    // arm: an exec that exited non-zero is exactly when CI wants the logs and reports the
    // globs name, and `run_sync`'s `?` above only fires when the exec never ran at all —
    // nothing to collect in that case. Raw bytes only: the *extraction* is local work with
    // its own exit row (`ERR_SYNC`), so it happens back in `run`.
    //
    // Two deliberate non-`?`s. The download is skipped entirely when no glob could
    // select anything — a `run .` with no `artifacts` key would otherwise pull the whole
    // post-build workdir over the wire to extract zero files. And a download *failure* is
    // recorded rather than propagated: by this line the exec has run and its result is in
    // `outcome`, and a `?` here would convert a green test run whose workload deleted its
    // own cwd (`make clean`) into a failure envelope with the passing output discarded.
    // The caller reads what happened in the envelope's `sync.error`.
    if let Some(plan) = sync
        && !plan.globs.is_empty()
    {
        match sandbox
            .session()
            .expect("run() built one")
            .download_tar(crate::sync::REMOTE_WORKDIR)
            .await
        {
            Ok(bytes) => *downloaded = Some(bytes),
            Err(error) => {
                ctx.out.warn(&format!(
                    "the workdir could not be downloaded, so no artifacts came back: {error}"
                ));
                *download_error = Some(error.to_string());
            }
        }
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
    // The identity pair, when --identity generated one. Core's own base64 helpers rather
    // than an encoding crate here, so the envelope, the registry, and the tunnel flags all
    // read one spelling that `from_encoded_parts` is guaranteed to accept back.
    if let Some(identity) = sandbox.tunnel_identity() {
        outcome.identity_host_seed = Some(identity.host_seed_base64());
        outcome.identity_vm_public_key = Some(identity.vm_public_key_base64());
    }
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
    // The same provisioning chain as `run` (`provision.rs`), for the same reason: the
    // caller's intent is "an image with the daemon in it", and which bytes that means is
    // this product's own knowledge. Role check first — it refuses in microseconds and the
    // fetch costs seconds of network.
    let mut agentd: Option<crate::provision::Resolved> = None;
    let binary: std::path::PathBuf = match &args.binary {
        Some(binary) => {
            if !binary.exists() {
                return Err(crate::exit::CliError::new(
                    Exit::Precondition,
                    format!("daemon binary not found: {}", binary.display()),
                )
                .suggest("cargo build --release -p agentd --target aarch64-unknown-linux-musl")
                .suggest(
                    "or pass no binary at all: the CLI provisions its own version's release \
                     asset",
                ));
            }
            binary.clone()
        }
        None => {
            let state = state_dir(args.state_dir.clone(), ctx.env);
            let resolved = {
                let out = &mut *ctx.out;
                crate::provision::resolve(
                    &state,
                    env!("CARGO_PKG_VERSION"),
                    ctx.env,
                    ctx.fetch,
                    &mut |line| out.progress(line),
                )?
            };
            let path = resolved.path.clone();
            agentd = Some(resolved);
            path
        }
    };
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
        BuildSpec {
            name: &seed,
            size,
            binary: &binary,
            dockerfile: args.dockerfile.as_deref(),
            repair_identity: args.repair_identity,
            artifact_uri: args.artifact_uri.as_deref(),
            base_image_version: args.base_image_version.as_deref(),
            log_group: args.log_group.as_deref(),
            log_stream: args.log_stream.as_deref(),
        },
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
            // The derived default group and a null stream, whatever the flags said: no
            // build ran, so no stream was resolved, and the group the *original* build
            // wrote to is not observable from a listing — the reuse identity is
            // binary+Dockerfile only, so the earlier build's logging config may differ.
            return Ok(render_build(
                &existing.image_arn,
                &name,
                size,
                true,
                &format!(
                    "{}/{name}",
                    microvms_core::control::image::BUILD_LOG_GROUP_PREFIX
                ),
                None,
                agentd.as_ref(),
            ));
        }
        ctx.out
            .progress(&format!("no image named {name}; building it"));
    } else {
        name = seed;
    }

    let mut sandbox = sandbox;
    ctx.out.progress(&format!("building image {name} ({size})"));
    // Before the upload, for the reason `run`'s build arm gives: a locally-refused request
    // must cost zero transport calls (issue #47).
    sandbox.preflight(&request)?;
    upload_artifact(ctx, &sandbox, &request).await?;
    let image = sandbox.build_image(request).await?;
    Ok(render_build(
        &image.identifier,
        &image.name,
        size,
        false,
        &image.build_log_group(),
        image.log_stream.as_deref(),
        agentd.as_ref(),
    ))
}

/// The envelope's `agentd` value: how a provisioned daemon got here, or null when the
/// caller supplied one (the positional or the config file's `binary` — `resolvedConfig`
/// tells that story on `run`, and repeating it here would be two keys for one fact).
fn agentd_report(agentd: Option<&crate::provision::Resolved>) -> serde_json::Value {
    match agentd {
        Some(resolved) => json!({
            "path": resolved.path.display().to_string(),
            "source": resolved.source.as_str(),
            "verified": match resolved.source {
                crate::provision::Source::Fetched(verification) => json!(verification.as_str()),
                _ => serde_json::Value::Null,
            },
        }),
        None => serde_json::Value::Null,
    }
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
    log_stream: Option<&str>,
    agentd: Option<&crate::provision::Resolved>,
) -> Rendered {
    let mut data = Map::new();
    data.insert("imageIdentifier".into(), json!(identifier));
    data.insert("imageName".into(), json!(name));
    // Named in the payload because the *service* creates it, Terraform never owns it, and
    // `terraform destroy` leaves it behind — so the caller who built this image is the only
    // one who will ever know to delete it.
    data.insert("buildLogGroup".into(), json!(build_log_group));
    // The RESOLVED exact stream name — the configured prefix plus the per-build `/<16
    // hex>` discriminator — or null when no stream was configured. The discriminator is
    // minted fresh inside core's create call, so this envelope is the only place a caller
    // (or an agent) can learn which stream this build's logs went to.
    data.insert("logStream".into(), json!(log_stream));
    data.insert("size".into(), json!(size.to_string()));
    data.insert("reused".into(), json!(reused));
    data.insert("agentd".into(), agentd_report(agentd));

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
    if let Some(stream) = log_stream {
        lines.push(format!(
            "log stream: {stream} (the configured prefix plus this build's own suffix; \
             all three build VMs write to this one exact stream)"
        ));
    }
    lines.push(
        "note: the service created that log group; terraform destroy will not remove it"
            .to_string(),
    );
    Rendered::ok(kind, data, lines.join("\n"), dense)
}

/// What a caller asks of one image build. See [`build_request_from`].
///
/// A struct rather than six parameters, and the same reason [`StartSpec`] gives applies with
/// more force here: three of them are `Option`, two of the three are `Option<&str>`, and
/// `build_request_from(ctx, name, size, binary, dockerfile, repair, uri, version)` is how an
/// artifact URI ends up where a base version was meant. `run` and `build` fill it differently
/// and the differences are the interesting part, so they are named at each call site rather
/// than positional.
struct BuildSpec<'a> {
    /// The image name, which is also the token label and the derived artifact key.
    pub name: &'a str,
    pub size: microvms_core::SizeClass,
    /// The aarch64 daemon binary to bake in as the image CMD.
    pub binary: &'a std::path::Path,
    /// A Dockerfile to use instead of the library's default. Its FROM must match the base.
    pub dockerfile: Option<&'a std::path::Path>,
    /// Whether to widen the guest so `sethostname` and the boot_id bind mount work.
    pub repair_identity: bool,
    /// Where the artifact already is, or `None` to derive a key under `--bucket`.
    pub artifact_uri: Option<&'a str>,
    /// `baseImageVersion`, or `None` to take whatever the service defaults to.
    ///
    /// **`run` always passes `None`, and the asymmetry with `build` is deliberate.** A pinned
    /// base is a property of a durable artifact, and `run`'s build is the
    /// build-and-throw-away shape whose image is deleted on the way out — so a flag there
    /// would pin a base nothing could later read back. Someone who cares which base their
    /// image sits on is running `microvm build`, whose image outlives the command.
    pub base_image_version: Option<&'a str>,
    /// `logging.cloudWatch.logGroup`, or `None` for the service default.
    pub log_group: Option<&'a str>,
    /// The caller's log-stream **prefix**; core appends the per-build discriminator and
    /// the resolved exact name comes back on the image for the envelope to report.
    pub log_stream: Option<&'a str>,
}

/// The create request for `run`'s arguments.
fn build_request<'a, O: std::io::Write, E: std::io::Write>(
    ctx: &Ctx<'_, O, E>,
    args: &'a RunArgs,
    name: &'a str,
    size: microvms_core::SizeClass,
    binary: &'a std::path::Path,
) -> Result<CreateImageRequest, Error> {
    build_request_from(
        ctx,
        BuildSpec {
            name,
            size,
            binary,
            dockerfile: args.dockerfile.as_deref(),
            repair_identity: args.repair_identity,
            artifact_uri: args.artifact_uri.as_deref(),
            // See the field: `run`'s image is thrown away, so there is nothing to pin for.
            base_image_version: None,
            // Unlike the pinned base, logging IS wired on `run`'s build arm: a throwaway
            // image's build still writes logs, and the config file's whole audience is
            // `microvm run` in a configured project.
            log_group: args.log_group.as_deref(),
            log_stream: args.log_stream.as_deref(),
        },
    )
}

/// The create request, shared by `run` and `build`.
///
/// `size` is a [`microvms_core::SizeClass`] rather than an integer all the way from the
/// parser, so there is no point on this path where an off-table baseline could be written.
fn build_request_from<O: std::io::Write, E: std::io::Write>(
    ctx: &Ctx<'_, O, E>,
    spec: BuildSpec<'_>,
) -> Result<CreateImageRequest, Error> {
    let BuildSpec {
        name,
        size,
        binary,
        dockerfile,
        repair_identity,
        artifact_uri,
        base_image_version,
        log_group,
        log_stream,
    } = spec;
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
    // Pinned only when asked for. Core refuses an illegal `Version` before the artifact is
    // uploaded, so there is no check here — the create call happens after the upload, and one
    // message about it in one place is the whole point of the guard living in core.
    request.base_image_version = base_image_version.map(str::to_string);
    // The stream is a *prefix* by core's contract: the discriminator is appended inside
    // `create_image` and the resolved exact name comes back on the image, so this CLI
    // never holds — and can never leak into a report — a stream name the wire did not
    // carry.
    request.log_group = log_group.map(str::to_string);
    request.log_stream = log_stream.map(str::to_string);
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
            .unwrap_or_else(microvms_core::session::mint_exec_id),
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
    let microvm_id =
        crate::commands::resolve_vm_identifier(ctx, &args.microvm_id, args.state_dir.clone())?;
    let plane = ctx.seam.control_plane(region).await?;
    let current = plane.get_microvm(&microvm_id).await?;
    if current.state != "RUNNING" {
        return Err(crate::exit::CliError::new(
            Exit::InvalidArg,
            format!(
                "microvm {} is {} and a suspend is only issued from RUNNING (STATE-5). Refused \
                 here rather than by the service, because the service's answer about a \
                 non-running id does not say which of the two things went wrong — and a suspend \
                 issued from SUSPENDED is a caller who believes they resumed.",
                microvm_id, current.state,
            ),
        )
        .with_data("state", json!(current.state)));
    }

    ctx.out.progress(&format!("suspending {}", microvm_id));
    plane.suspend(&microvm_id).await?;
    // TERMINATED is *wanted* rather than failed on: a VM that dies while suspending is a state
    // to report, not an error raised out of the middle of a teardown path.
    let settled = plane
        .wait_for_state(
            &microvm_id,
            &microvms_core::control::microvm::SUSPEND_WANTED,
            &[],
            wait_opts(args.timeout),
        )
        .await?;

    // Recorded only when the service really answered SUSPENDED: history carries what the
    // platform reported, and a freeze that settled TERMINATED is not a suspension however
    // the command exits.
    if settled.state == "SUSPENDED" {
        History::for_vm(&state_dir(args.state_dir.clone(), ctx.env), &microvm_id)
            .append(Event::Suspended);
    }

    let mut data = Map::new();
    data.insert("microvmId".into(), json!(microvm_id));
    data.insert("state".into(), json!(settled.state));
    let (kind, _) = response_type("suspend");
    let rendered = Rendered::ok(
        kind,
        data,
        format!("{} is {}", microvm_id, settled.state),
        format!("{}\t{}", microvm_id, settled.state),
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
    let microvm_id =
        crate::commands::resolve_vm_identifier(ctx, &args.microvm_id, args.state_dir.clone())?;
    let plane = ctx.seam.control_plane(region.clone()).await?;
    ctx.out.progress(&format!("resuming {}", microvm_id));
    plane.resume(&microvm_id).await?;
    let running = plane
        .wait_for_state(
            &microvm_id,
            &["RUNNING"],
            &microvms_core::constants::DEAD_STATES,
            wait_opts(args.timeout),
        )
        .await?;

    // The wait returned, so the service reported RUNNING — which is what `resumed` means.
    let root = state_dir(args.state_dir.clone(), ctx.env);
    History::for_vm(&root, &microvm_id).append(Event::Resumed);

    // The daemon's hook log, best-effort, and this is the moment that makes suspend-hook
    // firings visible at all: a frozen VM cannot answer a poll, so the record of the
    // suspend hook is readable only after the thaw. `/v1/health` is unauthenticated at
    // the daemon — no bearer is sent — so the attach carries an empty agent token; the
    // proxy credential it does need is minted through the same seam every attached
    // command uses. Every failure is swallowed: a resume whose VM is RUNNING must not
    // fail over a history nicety, so the short deadline bounds what a wedged endpoint
    // can cost.
    let attach = crate::seam::Attach {
        endpoint: running.endpoint.clone(),
        agent_token: String::new(),
        microvm_id: microvm_id.clone(),
        port: None,
    };
    if let Ok(Ok(session)) = tokio::time::timeout(
        Duration::from_secs(10),
        ctx.seam.attach_session(region, attach),
    )
    .await
        && let Ok(Ok(health)) = tokio::time::timeout(Duration::from_secs(5), session.health()).await
    {
        crate::history::append_unseen_hooks(&root, &microvm_id, &health.hooks);
    }

    let mut data = Map::new();
    data.insert("microvmId".into(), json!(microvm_id));
    data.insert("state".into(), json!("RUNNING"));
    // The endpoint the service just reported rather than one the caller passed: it is measured
    // not to change across a cycle, and reading it from the response is what makes that a
    // fact this code depends on rather than an assumption it encodes.
    data.insert("endpoint".into(), json!(running.endpoint));
    let (kind, _) = response_type("resume");
    Ok(Rendered::ok(
        kind,
        data,
        format!("{} is RUNNING at {}", microvm_id, running.endpoint),
        format!("{}\tRUNNING\t{}", microvm_id, running.endpoint),
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
    let microvm_id =
        crate::commands::resolve_vm_identifier(ctx, &args.microvm_id, args.state_dir.clone())?;
    let plane = ctx.seam.control_plane(region).await?;

    ctx.out.progress(&format!("terminating {}", microvm_id));
    let mut leaked: Vec<String> = Vec::new();
    let mut log_groups: Vec<String> = Vec::new();
    let mut state = "TERMINATING".to_string();

    // 1. The VM. A failure is recorded rather than raised, for the reason above.
    match plane.terminate(&microvm_id).await {
        Ok(()) => {}
        Err(error) => {
            leaked.push(microvm_id.clone());
            ctx.out.warn(&format!(
                "the terminate call for {} failed: {error}. The VM is still billing; record \
                 this id.",
                microvm_id
            ));
        }
    }
    if args.wait && leaked.is_empty() {
        match plane
            .wait_for_state(&microvm_id, &["TERMINATED"], &[], wait_opts(300.0))
            .await
        {
            Ok(settled) => state = settled.state,
            // Not a leak: the platform accepted the terminate, so the VM is on its way out and
            // TERMINATING is the honest state to report.
            Err(error) => ctx.out.warn(&format!(
                "{} did not reach TERMINATED before the deadline: {error}",
                microvm_id
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

    // The teardown verdict, in the TeardownReport's own terms: whether the terminate call
    // was accepted, and what a delete was asked for and did not remove. Appended however
    // the calls above went — a terminate that failed is exactly the run a caller will want
    // the record of — and the append swallows its own failures, because this is the
    // teardown path and a history error must not displace the real outcome.
    History::for_vm(&state_dir(args.state_dir.clone(), ctx.env), &microvm_id).append(
        Event::Terminated {
            terminate_accepted: !leaked.contains(&microvm_id),
            undeleted: leaked.clone(),
        },
    );

    // The name is released only when the terminate was accepted, and by the VM id rather
    // than by the spelling the caller used — `terminate mvm-…` on a VM that was named must
    // still free the name, or the registry keeps refusing a name whose VM is gone. A
    // terminate that failed keeps the registration, because the VM is still billing and the
    // name still addresses it.
    if !leaked.contains(&microvm_id) {
        let names = crate::ledger::Names::new(&state_dir(args.state_dir.clone(), ctx.env));
        if let Some(freed) = names.release_by_vm(&microvm_id) {
            ctx.out
                .progress(&format!("released name {freed} — it can be reused"));
        }
    }

    let mut data = Map::new();
    data.insert("microvmId".into(), json!(microvm_id));
    data.insert("imageIdentifier".into(), json!(args.image_identifier));
    data.insert("leaked".into(), json!(leaked));
    // Separate from `leaked` because they are different claims: `leaked` is "a delete was
    // attempted and failed", and this is "no client here can delete it at all". Collapsing
    // them would make a normal `--delete-image` teardown look like a failed one.
    data.insert("undeletedLogGroups".into(), json!(log_groups));
    data.insert("state".into(), json!(state));

    let (kind, _) = response_type("terminate");
    let mut lines = vec![format!("terminated {} ({state})", microvm_id)];
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
        format!("{}\t{}\t{}", microvm_id, state, leaked.join(",")),
    );
    if !leaked.is_empty() {
        // A leak the caller must act on. The log group is *not* in this condition: it is a
        // normal outcome of a teardown by a client without CloudWatch, and failing over it
        // would make every successful `--delete-image` exit non-zero.
        return Ok(rendered.reporting(Exit::Platform));
    }
    Ok(rendered)
}

/// The registry record for a kept, named launch — or `None` when the outcome is missing an
/// identifier, which means the launch did not fully succeed and no name should point at it.
///
/// Every field is the service's own answer read off the outcome, never the request's: the
/// endpoint is what `RunMicrovm` reported and the token is the one the session minted. A
/// record built from the request would be one the registry vouches for and the daemon
/// refuses.
fn name_record_for_kept(
    vm_name: &str,
    outcome: &RunOutcome,
    region_name: &str,
) -> Option<crate::ledger::NameRecord> {
    Some(crate::ledger::NameRecord {
        name: vm_name.to_string(),
        microvm_id: outcome.microvm_id.clone()?,
        endpoint: outcome.endpoint.clone()?,
        agent_token: outcome.agent_token.clone()?,
        region: region_name.to_string(),
        at: epoch_secs(),
        // Present exactly when `--identity` generated material; `tunnel --name
        // --verify-identity` reads them back in a later process. Not `?`-propagated:
        // their absence is a launch without the flag, never a failed launch.
        identity_host_seed: outcome.identity_host_seed.clone(),
        identity_vm_public_key: outcome.identity_vm_public_key.clone(),
    })
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

    /// `run --exec` and `exec` produce the same `StartRequest` for the same inputs.
    ///
    /// The two paths *agreeing* is the property, not either one in isolation: both build
    /// their spec and hand it to the one `start_request`, and this test is what makes a
    /// second constructor — "where it silently acquires a different answer" — fail loudly
    /// instead. The specs below are built exactly the way each command builds its own:
    /// `run` widens the one-shot constructor with the demotion pair, `exec` writes every
    /// field. `exec_id` is compared by shape rather than value, because freshness per
    /// invocation is that field's own tested property.
    #[test]
    fn run_exec_and_exec_agree_on_the_start_request_they_send() {
        let command = "id -u";
        let (user, group) = (Some(1000), Some(2000));

        // `run --exec 'id -u' --user 1000 --group 2000`, as lifecycle.rs builds it.
        let from_run = start_request(StartSpec {
            user,
            group,
            ..StartSpec::command(command)
        });
        // `microvm exec 'id -u' --user 1000 --group 2000`, as attached.rs builds it —
        // every field written, the flag-less ones at their parsed defaults.
        let from_exec = start_request(StartSpec {
            command,
            cwd: None,
            exec_id: None,
            stdin: false,
            env: std::collections::HashMap::new(),
            user,
            group,
        });

        assert_eq!(from_run.command, from_exec.command);
        assert_eq!(from_run.shell, from_exec.shell);
        assert_eq!(from_run.cwd, from_exec.cwd);
        assert_eq!(from_run.env, from_exec.env);
        assert_eq!(from_run.user, from_exec.user);
        assert_eq!(from_run.group, from_exec.group);
        assert_eq!(from_run.stdin, from_exec.stdin);
        assert_eq!(from_run.timeout_sec, from_exec.timeout_sec);
        assert!(from_run.exec_id.starts_with("x-"), "{}", from_run.exec_id);
        assert!(from_exec.exec_id.starts_with("x-"), "{}", from_exec.exec_id);
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

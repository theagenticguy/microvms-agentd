// SPDX-License-Identifier: Apache-2.0
//! `ls`, `history`, `logs`, `manifest`, `constants`, `dockerfile` — the commands that touch no
//! account.
//!
//! Grouped by that property rather than by shape, because it is the property the behavioral
//! thinness guard cares about: `tests/thinness.rs` asserts that every command *not* in this
//! module goes through the seam, and that the ones here are named with a reason. A new
//! AWS-touching command is therefore covered by the guard by default and can only leave the
//! net by someone writing its name in that list.
//!
//! `logs` is the exception that proves the rule: it is *about* an AWS resource and reaches no
//! account, because neither this crate nor `microvms-core` carries a CloudWatch client. See
//! [`logs`] for why that is reported as a failure rather than as an empty list.

use serde_json::{Map, Value, json};

use crate::cli::{DockerfileArgs, HistoryArgs, LogsArgs, LsArgs};
use crate::commands::{Ctx, Rendered, response_type};
use crate::exit::{CliError, Exit};
use crate::seam::state_dir;
use crate::{history, ledger};

/// Lists what this CLI created and could not confirm it deleted.
pub fn ls<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &LsArgs,
) -> Result<Rendered, CliError> {
    let root = state_dir(args.state_dir.clone(), ctx.env);
    let runs = ledger::read_all(&root);

    let mut data = Map::new();
    data.insert("runs".into(), json!(runs));
    let (kind, _) = response_type("ls");

    let dense = runs
        .iter()
        .map(|run| {
            format!(
                "{}\t{}\t{}",
                run["runId"].as_str().unwrap_or_default(),
                run["microvmId"].as_str().unwrap_or_default(),
                joined(&run["leaked"]),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = if runs.is_empty() {
        "nothing outstanding".to_string()
    } else {
        runs.iter()
            .map(|run| {
                format!(
                    "{}  microvm={} image={} leaked={}",
                    run["runId"].as_str().unwrap_or_default(),
                    run["microvmId"].as_str().unwrap_or("-"),
                    run["imageIdentifier"].as_str().unwrap_or("-"),
                    if joined(&run["leaked"]).is_empty() {
                        "-".to_string()
                    } else {
                        joined(&run["leaked"])
                    },
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(Rendered::ok(kind, data, text, dense))
}

/// Prints what was asked of one MicroVM and what the platform reported back.
///
/// Reads the local per-VM history rather than asking AWS, for `ls`'s reason turned around:
/// the record's value is that it survives the VM, and no `GetMicrovm` can answer about an id
/// the platform has already forgotten. A VM with no history file is a clean empty result,
/// not an error — asking about a VM this state dir never saw is a question, not a mistake,
/// since the id may be real and the record may live in another machine's state directory.
pub fn history<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &HistoryArgs,
) -> Result<Rendered, CliError> {
    let root = state_dir(args.state_dir.clone(), ctx.env);
    let events = history::read_events(&root, &args.microvm_id);

    let mut data = Map::new();
    data.insert("microvmId".into(), json!(args.microvm_id));
    data.insert("events".into(), json!(events));
    let (kind, _) = response_type("history");

    let describe = |event: &Value| -> String {
        // The event-specific fields, in one terse tail. Unknown keys render too, so a
        // record written by a newer build still reads rather than printing blank.
        let mut fields: Vec<String> = event
            .as_object()
            .map(|object| {
                object
                    .iter()
                    .filter(|(key, _)| !matches!(key.as_str(), "seq" | "at" | "event"))
                    .map(|(key, value)| match value {
                        Value::String(text) => format!("{key}={text}"),
                        other => format!("{key}={other}"),
                    })
                    .collect()
            })
            .unwrap_or_default();
        fields.sort();
        fields.join(" ")
    };
    let line_of = |event: &Value, separator: &str| -> String {
        format!(
            "{}{separator}{}{separator}{}{separator}{}",
            event["seq"]
                .as_u64()
                .map(|seq| seq.to_string())
                .unwrap_or_else(|| "-".to_string()),
            event["at"]
                .as_u64()
                .map(|at| at.to_string())
                .unwrap_or_else(|| "-".to_string()),
            event["event"].as_str().unwrap_or("unreadable"),
            describe(event),
        )
    };

    let dense = events
        .iter()
        .map(|event| line_of(event, "\t"))
        .collect::<Vec<_>>()
        .join("\n");
    let text = if events.is_empty() {
        format!("no history for {} in this state dir", args.microvm_id)
    } else {
        events
            .iter()
            .map(|event| line_of(event, "  "))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(Rendered::ok(kind, data, text, dense))
}

/// A JSON array of strings as a comma-separated list.
fn joined(value: &Value) -> String {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

/// Names an image's build log group, and says why it cannot read it.
///
/// # Why this fails rather than returning an empty list
///
/// `cli.py:2041` reads the group through boto3. Neither `microvms-core` nor this crate has a
/// CloudWatch client — T-W2-2 froze core's dependency set, and adding one *here* would give
/// the CLI a second path to AWS, which is exactly what CLI-2 forbids and what the thinness
/// guard is.
///
/// # Adding a reader to core was assessed and refused, and not on grounds of size
///
/// The mechanics are genuinely cheap. `FilterLogEvents` is one signed POST to
/// `logs.<region>.amazonaws.com` and `microvms-core/src/control/transport.rs` already signs
/// SigV4 over an arbitrary method, path, and JSON body. It looks like well under a hundred
/// lines plus a recorder fake. Three things say no anyway, and the third is decisive.
///
/// **The transport is single-service by construction, and that is load-bearing.** Its
/// `signingName` is a `const` (`lambda`), the endpoint comes from one `endpoint_for(region)`,
/// the API version is the drift gate's `MODEL_API_VERSION` prefixed onto every path, and
/// `Call` has no header field at all — `content-type: application/json` is written inline
/// before signing, because the signature covers it. CloudWatch Logs needs a **different
/// signing name**, a **different host**, an **`X-Amz-Target` header**, and no API-version
/// path segment. Every one of those is a parameter the type deliberately does not have.
/// Adding them turns "this transport can only talk to lambda-microvms" — a property a
/// reviewer checks by reading four constants — into a runtime argument, and that property is
/// what makes `transport.rs`'s "nothing here reads the service model at runtime" checkable.
/// A second, separate transport avoids that and is no longer the contained change; it is a
/// second signer, a second error taxonomy (`classify_failure`'s seven statuses are the
/// *lambda-microvms* modeled exceptions, documented as such), and a second recorder.
///
/// **The diagnostic it would strengthen is already unconditional, on purpose.**
/// `control/image.rs`'s `build_failure` names the log-group prefix on every failure rather
/// than only on an empty group, and its own comment calls that a deliberate weakening.
/// Re-reading that page: naming the prefix always is *not much worse* than naming it
/// conditionally, because the sentence a reader acts on ("the role must grant logs on
/// `/aws/lambda-microvms/*`, and `/aws/lambda/microvms/*` is the plausible wrong one") is
/// identical either way. What a reader loses is one bit — whether the group was empty — and
/// they can get that bit from the `aws logs tail` line this command already suggests.
///
/// **And the read would most often fail with a permissions error, which is a worse
/// diagnostic than this one.** `conformance/infra/main.tf` grants `logs:CreateLogGroup`,
/// `CreateLogStream`, and `PutLogEvents` to the build and execution roles, and grants
/// `FilterLogEvents`, `GetLogEvents`, and `DescribeLogStreams` to **nobody** — the caller's
/// own identity included. So a reader shipped today would answer `AccessDeniedException` for
/// a caller whose account is set up exactly as this project documents, and an
/// `AccessDeniedException` about a log group is precisely the message that sends someone to
/// audit the *build* role's log policy, which is the confusion the prefix finding exists to
/// prevent. Making it work means also granting a read on the caller's identity, which is a
/// Terraform change plus a documented new requirement — and at that point the caller has the
/// `aws logs tail` invocation this command hands them anyway.
///
/// So: accepted. The remedy stays the suggestion below, and the honest reason it is a
/// suggestion is that this client refuses to guess at a second service rather than that
/// nobody has written the code.
///
/// So this command derives the group, names it in the payload, and exits `ERR_PRECONDITION`
/// with the `aws logs` invocation that reads it. Deliberately a failure and not a success with
/// `lines: []`, because an empty list is the wire shape for "the group exists and has no
/// events" — and that is the *exact* confusion the build-role-prefix finding exists to
/// prevent. A role granted the plausible-but-wrong `/aws/lambda/microvms/*` produces builds
/// that write no logs at all, every failure then reads `reason=unknown`, and a client that
/// answered identically for "I cannot read" and "there was nothing to read" would make that
/// indistinguishable a second time.
///
/// (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)
pub fn logs<O: std::io::Write, E: std::io::Write>(
    _ctx: &mut Ctx<'_, O, E>,
    args: &LogsArgs,
) -> Result<Rendered, CliError> {
    let group = format!(
        "{}/{}",
        microvms_core::control::image::BUILD_LOG_GROUP_PREFIX,
        args.image_name
    );
    Err(CliError::new(
        Exit::Precondition,
        format!(
            "the build log group for {} is {group}, and this client cannot read it: CloudWatch \
             Logs is absent from microvms-core's dependency set, and an AWS SDK client in the CLI \
             would give it a second path to AWS (CLI-2). The group name is the part that is easy \
             to get wrong — a build role granted /aws/lambda/microvms/* instead of \
             /aws/lambda-microvms/* produces builds that write no logs at all, and every failure \
             then reads reason=unknown.",
            args.image_name,
        ),
    )
    .suggest(format!("aws logs tail {group} --since 1h --format short"))
    .suggest(format!(
        "the build role must grant logs on {}/*",
        microvms_core::control::image::BUILD_LOG_GROUP_PREFIX
    ))
    .with_data("logGroup", json!(group))
    // Explicitly null rather than an empty array, so a consumer cannot read "no events".
    .with_data("lines", Value::Null))
}

/// The whole command surface, derived from the clap tree.
///
/// # Derived, never written down
///
/// The commands, their parameters, and every parameter's closed domain come from
/// [`clap::Command`] introspection; the exit table comes from [`crate::exit::EXIT_TABLE`],
/// which the runtime also reads. So the manifest cannot drift from what this binary accepts,
/// and a command added without a `RESPONSE_TYPES` row fails `tests/manifest.rs` rather than
/// shipping undescribed — which is the whole of the manifest's value to an agent: not that it
/// is accurate, but that it cannot be wrong.
///
/// `choices` is the CLI-5 witness. An option whose library counterpart is S1 must report a
/// closed set here, because the CLI is where an S1 guard is most easily downgraded to a
/// convenience string flag.
pub fn manifest<O: std::io::Write, E: std::io::Write>(
    _ctx: &mut Ctx<'_, O, E>,
) -> Result<Rendered, CliError> {
    let built = crate::manifest::build();
    let (kind, _) = response_type("manifest");
    let data = built.as_object().cloned().unwrap_or_default();
    let text = crate::manifest::render(&built, false);
    let dense = crate::manifest::render(&built, true);
    Ok(Rendered::ok(kind, data, text, dense))
}

/// Emits every service constraint this client believes (TRAP-12).
///
/// `microvms_core::constants::as_json()` verbatim, keyed as `sandbox.py` names them, so
/// `scripts/check-model-drift.py` can compare it against the pinned botocore model *and* against
/// the Python client's own constants — which is the only check available for a value no API
/// answers, like the region list.
///
/// The `data` wrapping is deliberate and so is the escape from it: `--emit-json` writes the
/// bare object, because the drift gate compares key-for-key and an envelope would put every
/// comparison behind `["data"]["constants"]` for no gain. Under the global `--json` the same
/// object arrives inside the envelope, so both consumers are served without a second source of
/// truth.
pub fn constants<O: std::io::Write, E: std::io::Write>(
    _ctx: &mut Ctx<'_, O, E>,
) -> Result<Rendered, CliError> {
    let emitted = microvms_core::constants::as_json();
    let mut data = Map::new();
    data.insert("constants".into(), emitted.clone());
    let (kind, _) = response_type("constants");
    // The bare object as the text rendering, which is what `--emit-json` prints: the one stdout
    // write in this binary that is not an envelope, and the reason is in the flag's help.
    let text = serde_json::to_string_pretty(&emitted).unwrap_or_else(|_| emitted.to_string());
    Ok(Rendered::ok(kind, data, text, emitted.to_string()))
}

/// Prints the Dockerfile stanza that wraps a base image with agentd.
///
/// # Reused, never duplicated
///
/// The stanza comes from [`microvms_core::control::default_dockerfile`] — the same function
/// `microvm build` bakes when no `--dockerfile` is given (`microvms-core/src/control/image.rs`,
/// `build_artifact_for`). A copy here would be a second Dockerfile that could drift from the
/// one the default build produces, and the whole point of this command is that appending your
/// own `RUN` layers to its output *is* the default build plus your layers.
///
/// # The two traps ride along as comments
///
/// Both are platform constraints microvms-core enforces, and both bite one build cycle after
/// the mistake is made, so the stanza itself says them where an editor will read them:
///
/// - The `FROM` must be the ref that pairs with the create call's `baseImageArn`;
///   `require_matching_from` refuses a Dockerfile whose FROM disagrees.
/// - A `WORKDIR` is required when the base declares none — the managed al2023 base does not,
///   and `require_workdir` refuses inheriting a working directory that does not exist.
///
/// `--from` overrides only the docker ref, for a caller pairing a different managed base; the
/// emitted comment then reminds them the `baseImageArn` has to change with it.
pub fn dockerfile<O: std::io::Write, E: std::io::Write>(
    _ctx: &mut Ctx<'_, O, E>,
    args: &DockerfileArgs,
) -> Result<Rendered, CliError> {
    let mut base = microvms_core::control::BaseImage::al2023();
    if let Some(from) = args.from.as_deref() {
        base.docker_ref = from.to_string();
    }
    let stanza =
        microvms_core::control::default_dockerfile(args.port, args.workdir.as_deref(), &base);

    let mut header = vec![
        "# agentd wrapper stanza — append your own RUN layers below the chmod line.".to_string(),
        format!(
            "# The FROM must match the managed base's docker_ref: baseImageArn ({}) and the",
            base.name
        ),
        "# FROM select the same base, and microvms-core refuses a Dockerfile whose FROM"
            .to_string(),
        "# disagrees (require_matching_from).".to_string(),
    ];
    if args.workdir.is_none() {
        header.push(
            "# No --workdir was given. The managed base declares no WorkingDir, so a WORKDIR"
                .to_string(),
        );
        header.push(
            "# is required here: without one every relative path resolves against `/`, and"
                .to_string(),
        );
        header.push(
            "# microvms-core refuses inherit_workdir when nothing declares one \
             (require_workdir)."
                .to_string(),
        );
    }
    let text = format!("{}\n{stanza}", header.join("\n"));

    let mut data = Map::new();
    data.insert("stanza".into(), json!(text));
    data.insert("baseImageName".into(), json!(base.name));
    data.insert("baseImageDockerRef".into(), json!(base.docker_ref));
    data.insert("port".into(), json!(args.port));
    data.insert("workdir".into(), json!(args.workdir));
    let (kind, _) = response_type("dockerfile");

    // Dense drops the header: the constraints are for a human editing the file, and a
    // token-paying consumer asked for the stanza itself.
    Ok(Rendered::ok(kind, data, text, stanza))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Format, Output};
    use crate::seam::Infra;

    /// A context over buffers and an empty environment.
    fn ctx<'a>(
        out: &'a mut Output<Vec<u8>, Vec<u8>>,
        seam: &'a dyn crate::seam::CoreSeam,
        env: &'a dyn Fn(&str) -> Option<String>,
    ) -> Ctx<'a, Vec<u8>, Vec<u8>> {
        Ctx {
            seam,
            out,
            infra: Infra::default(),
            env,
        }
    }

    /// `ls` with nothing outstanding says so rather than printing an empty line.
    #[test]
    fn ls_with_an_empty_state_directory_says_nothing_outstanding() {
        let mut out = Output::new(Format::Plain, false, Vec::new(), Vec::new());
        let env = |_: &str| Some("/nonexistent-microvm-state".to_string());
        let seam = crate::seam::PanickingSeam;
        let mut context = ctx(&mut out, &seam, &env);
        let rendered = ls(
            &mut context,
            &LsArgs {
                state_dir: Some(std::path::PathBuf::from("/nonexistent-microvm-state")),
            },
        )
        .expect("ls never fails");
        assert_eq!(rendered.text, "nothing outstanding");
        assert_eq!(rendered.data["runs"], json!([]));
    }

    /// `history` for a VM this state dir never saw is a clean empty success.
    ///
    /// Not an error: the id may be real and the record may live in another machine's state
    /// directory, so asking is a question rather than a mistake. A command that failed here
    /// would make "did anything happen to this VM" unanswerable for the empty case, which is
    /// half the question's value.
    #[test]
    fn history_for_an_unseen_vm_is_a_clean_empty_result() {
        let mut out = Output::new(Format::Plain, false, Vec::new(), Vec::new());
        let env = |_: &str| None;
        let seam = crate::seam::PanickingSeam;
        let mut context = ctx(&mut out, &seam, &env);
        let rendered = history(
            &mut context,
            &HistoryArgs {
                microvm_id: "mvm-never-seen".to_string(),
                state_dir: Some(std::path::PathBuf::from("/nonexistent-microvm-state")),
            },
        )
        .expect("an unseen VM is a question, not a mistake");
        assert_eq!(rendered.kind, "microvm.history");
        assert_eq!(rendered.data["microvmId"], "mvm-never-seen");
        assert_eq!(rendered.data["events"], json!([]));
        assert!(rendered.text.contains("no history"), "{}", rendered.text);
    }

    /// `history` renders one terse line per event, and the envelope carries them verbatim.
    ///
    /// The events in `data` are exactly what `history::read_events` produced — a rendering
    /// that reshaped them would be a second wire format for one file.
    #[test]
    fn history_renders_one_line_per_event_and_the_envelope_carries_them_verbatim() {
        let dir = std::env::temp_dir().join(format!(
            "microvm-cli-local-history-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp dir");

        let file = crate::history::History::for_vm(&dir, "mvm-1");
        file.append(crate::history::Event::Launched {
            image_identifier: "arn:image".to_string(),
            endpoint: "https://mvm-1.example".to_string(),
            region: "us-east-1".to_string(),
        });
        file.append(crate::history::Event::Terminated {
            terminate_accepted: true,
            undeleted: Vec::new(),
        });

        let mut out = Output::new(Format::Plain, false, Vec::new(), Vec::new());
        let env = |_: &str| None;
        let seam = crate::seam::PanickingSeam;
        let mut context = ctx(&mut out, &seam, &env);
        let rendered = history(
            &mut context,
            &HistoryArgs {
                microvm_id: "mvm-1".to_string(),
                state_dir: Some(dir.clone()),
            },
        )
        .expect("history never fails");

        assert_eq!(
            rendered.data["events"],
            json!(crate::history::read_events(&dir, "mvm-1")),
            "the envelope is the file's own shape"
        );
        let lines: Vec<&str> = rendered.text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per event: {}", rendered.text);
        assert!(lines[0].contains("launched"), "{}", lines[0]);
        assert!(
            lines[0].contains("endpoint=https://mvm-1.example"),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("terminated"), "{}", lines[1]);
        // Dense is the same rows, tab-separated, with seq in field one.
        let dense: Vec<&str> = rendered.dense_text.lines().collect();
        assert!(dense[0].starts_with("0\t"), "{}", rendered.dense_text);
        assert!(dense[1].starts_with("1\t"), "{}", rendered.dense_text);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **`logs` fails rather than reporting an empty list, and names the group.**
    ///
    /// The distinction it protects: an empty `lines` array is the wire shape for "the group has
    /// no events", which is exactly what a wrong build-role prefix produces — so a client that
    /// answered identically for "cannot read" would recreate the confusion the finding exists
    /// to prevent.
    #[test]
    fn logs_names_the_group_and_refuses_to_imply_it_is_empty() {
        let mut out = Output::new(Format::Plain, false, Vec::new(), Vec::new());
        let env = |_: &str| None;
        let seam = crate::seam::PanickingSeam;
        let mut context = ctx(&mut out, &seam, &env);
        let failure = logs(
            &mut context,
            &LogsArgs {
                image_name: "agentd-conformance".to_string(),
                region: crate::cli::RegionFlags::default(),
            },
        )
        .expect_err("this client cannot read CloudWatch");

        assert_eq!(failure.exit, Exit::Precondition);
        assert_eq!(
            failure.data["logGroup"],
            "/aws/lambda-microvms/agentd-conformance"
        );
        assert_eq!(
            failure.data["lines"],
            Value::Null,
            "an empty array would read as 'the group has no events'"
        );
        // The prefix that is easy to get wrong is named, and so is the way to read the group.
        assert!(
            failure.message.contains("/aws/lambda/microvms/*"),
            "{failure:?}"
        );
        assert!(
            failure
                .suggestions
                .iter()
                .any(|hint| hint.contains("aws logs tail")),
            "{failure:?}"
        );
    }

    /// `constants` emits the bare object as its text rendering, keyed for the drift gate.
    ///
    /// The keys are the coupling: `scripts/check-model-drift.py` looks them up by name, and a
    /// rename makes it report nothing disagreed — which is worse than a crash.
    #[test]
    fn constants_emits_the_object_the_drift_gate_reads() {
        let mut out = Output::new(Format::Plain, false, Vec::new(), Vec::new());
        let env = |_: &str| None;
        let seam = crate::seam::PanickingSeam;
        let mut context = ctx(&mut out, &seam, &env);
        let rendered = constants(&mut context).expect("constants never fails");

        // The text rendering is the bare object, unwrapped.
        let parsed: Value = serde_json::from_str(&rendered.text).expect("valid JSON");
        assert_eq!(parsed, microvms_core::constants::as_json());
        assert_eq!(parsed["MODEL_API_VERSION"], "2025-09-09");
        assert_eq!(parsed["MAX_RUN_HOOK_PAYLOAD_BYTES"], 4096);
        assert!(parsed["MICROVM_REGIONS"].is_array());
        // And the envelope form carries the identical object under `constants`.
        assert_eq!(rendered.data["constants"], parsed);
    }

    /// A `dockerfile` invocation with `args`, rendered.
    fn render_dockerfile(args: &DockerfileArgs) -> Rendered {
        let mut out = Output::new(Format::Plain, false, Vec::new(), Vec::new());
        let env = |_: &str| None;
        let seam = crate::seam::PanickingSeam;
        let mut context = ctx(&mut out, &seam, &env);
        dockerfile(&mut context, args).expect("dockerfile never fails")
    }

    /// **The stanza is the default build's Dockerfile, with the daemon lines intact.**
    ///
    /// Asserted on the load-bearing lines rather than on the whole text, because the claim
    /// is about what an image built from this stanza *does*: `FROM` the managed base's pair,
    /// `COPY agentd` so the binary is in the image, `ENTRYPOINT []` plus `CMD ["/agentd"]` so
    /// the daemon is the container CMD — the deployment invariant the trust boundary rests on
    /// (`docs/PROTOCOL.md`, "Trust boundary") — and the requested port in both the env knob
    /// and the EXPOSE.
    ///
    /// **Falsification** — drop the `CMD ["/agentd"]` line from
    /// `microvms_core::control::default_dockerfile` and the CMD assertion goes red: an image
    /// built from the stanza would boot the base's own entrypoint and the run hook would
    /// time out. Broken and confirmed red on 2026-08-14, then restored.
    #[test]
    fn the_stanza_carries_the_daemon_lines_the_default_build_bakes() {
        let rendered = render_dockerfile(&DockerfileArgs {
            from: None,
            port: 9000,
            workdir: None,
        });

        let base = microvms_core::control::BaseImage::al2023();
        let stanza = &rendered.dense_text;
        assert!(
            stanza.contains(&format!("FROM {}", base.docker_ref)),
            "the default FROM is the managed base's pair: {stanza}"
        );
        assert!(stanza.contains("COPY agentd /agentd"), "{stanza}");
        assert!(stanza.contains("RUN chmod 0755 /agentd"), "{stanza}");
        assert!(
            stanza.contains("ENTRYPOINT []"),
            "the trust boundary rests on the daemon being the container CMD: {stanza}"
        );
        assert!(stanza.contains(r#"CMD ["/agentd"]"#), "{stanza}");
        assert!(stanza.contains("ENV AGENTD_PORT=9000"), "{stanza}");
        assert!(stanza.contains("EXPOSE 9000"), "{stanza}");

        // The envelope names the base *pair*, because a consumer needs both halves: the
        // docker_ref for the FROM and the name the baseImageArn is derived from.
        assert_eq!(rendered.data["baseImageName"], json!(base.name));
        assert_eq!(rendered.data["baseImageDockerRef"], json!(base.docker_ref));
        assert_eq!(rendered.data["port"], json!(9000));
        assert_eq!(rendered.data["workdir"], Value::Null);
        // And the human text is the stanza plus the header naming both platform traps.
        assert!(
            rendered.text.contains("require_matching_from"),
            "{}",
            rendered.text
        );
        assert!(
            rendered.text.contains("require_workdir"),
            "{}",
            rendered.text
        );
        assert!(rendered.text.ends_with(&rendered.dense_text));
        assert_eq!(rendered.data["stanza"], json!(rendered.text));
    }

    /// A `--workdir` lands as both the `mkdir` and the `WORKDIR`, and a `--port` reaches
    /// every port-bearing line.
    #[test]
    fn a_workdir_and_a_port_reach_the_stanza() {
        let rendered = render_dockerfile(&DockerfileArgs {
            from: None,
            port: 8125,
            workdir: Some("/workspace".into()),
        });
        let stanza = &rendered.dense_text;
        assert!(stanza.contains("RUN mkdir -p /workspace"), "{stanza}");
        assert!(stanza.contains("WORKDIR /workspace"), "{stanza}");
        assert!(stanza.contains("ENV AGENTD_PORT=8125"), "{stanza}");
        assert!(stanza.contains("EXPOSE 8125"), "{stanza}");
        assert_eq!(rendered.data["workdir"], json!("/workspace"));
        // With a workdir given, the header's workdir warning has nothing to warn about.
        assert!(
            !rendered.text.contains("No --workdir was given"),
            "{}",
            rendered.text
        );
    }

    /// `--from` replaces the FROM ref and nothing else, and the header still names the
    /// agreement constraint — because a caller who changed the ref is exactly the caller
    /// about to hit `require_matching_from`.
    #[test]
    fn a_from_override_replaces_the_ref_and_keeps_the_constraint_comment() {
        let rendered = render_dockerfile(&DockerfileArgs {
            from: Some("public.ecr.aws/example/other:2024".into()),
            port: 9000,
            workdir: Some("/srv".into()),
        });
        let stanza = &rendered.dense_text;
        assert!(
            stanza.contains("FROM public.ecr.aws/example/other:2024"),
            "{stanza}"
        );
        assert!(
            !stanza.contains("amazonlinux:2023-minimal"),
            "the default ref must not survive an override: {stanza}"
        );
        // The daemon lines are unchanged by the override.
        assert!(stanza.contains(r#"CMD ["/agentd"]"#), "{stanza}");
        assert!(
            rendered.text.contains("require_matching_from"),
            "{}",
            rendered.text
        );
        assert_eq!(
            rendered.data["baseImageDockerRef"],
            json!("public.ecr.aws/example/other:2024")
        );
    }
}

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
//! account, because neither this crate nor `microvms-core` carries a CloudWatch client. It
//! succeeds by naming the group and printing the `aws logs tail` invocation that reads it —
//! see [`logs`] for why the reading is the AWS CLI's job and why `lines` is still null.

use serde_json::{Map, Value, json};

use crate::cli::{DockerfileArgs, HistoryArgs, LogsArgs, LsArgs};
use crate::commands::{Ctx, Rendered, response_type};
use crate::exit::CliError;
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
    // A registered name resolves to its VM id here too, so the one identifier grammar holds
    // across the whole surface. An *unregistered* name still reads as a clean empty history
    // — `resolve_vm_identifier`'s miss is a failure, and this command's contract is that
    // asking about an unseen VM is a question, not a mistake — so the miss is mapped back
    // to the identifier itself and the empty read below answers honestly.
    let microvm_id =
        crate::commands::resolve_vm_identifier(ctx, &args.microvm_id, args.state_dir.clone())
            .unwrap_or_else(|_| args.microvm_id.clone());
    let root = state_dir(args.state_dir.clone(), ctx.env);
    let events = history::read_events(&root, &microvm_id);

    let mut data = Map::new();
    data.insert("microvmId".into(), json!(microvm_id));
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
        format!("no history for {microvm_id} in this state dir")
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

/// Names an image's build log group and prints the `aws logs tail` invocation that reads it.
///
/// # A success, with the reading delegated to the AWS CLI
///
/// `cli.py:2041` reads the group through boto3. Neither `microvms-core` nor this crate has a
/// CloudWatch client, and adding one *here* would give the CLI a second path to AWS, which
/// is exactly what CLI-2 forbids and what the thinness guard is. If log reading matters,
/// the client belongs in core behind the seam.
///
/// The command used to exit `ERR_PRECONDITION` with the tail invocation as a suggestion — a
/// failure whose remedy was on the failure itself. That inverted the developer experience:
/// the caller asked a legitimate question ("where are my build logs and how do I read
/// them?") and got a non-zero exit for asking. Per the 0.6.0 ruling on #79, the answer is a
/// success: the group, the working `aws logs tail` command, and the AWS CLI v2 floor —
/// `aws logs tail` does not exist in AWS CLI v1 (verified present in 2.35.7). The Terraform
/// stack pairs this with a managed read policy (`logs_read_policy_arn`) granting
/// `FilterLogEvents`/`GetLogEvents`/`DescribeLogStreams` on the build log groups, so the
/// printed command works on a fresh install once that policy is attached to the caller.
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
/// **And the read grant is the caller's, not a role's.** `conformance/infra/main.tf` grants
/// `logs:CreateLogGroup`, `CreateLogStream`, and `PutLogEvents` to the build and execution
/// roles — the *write* side. The read side (`FilterLogEvents`, `GetLogEvents`,
/// `DescribeLogStreams`) belongs to whatever identity runs the tail, which the module cannot
/// know, so it ships as the standalone `logs_read_policy_arn` managed policy for the caller
/// to attach. A reader baked into this client would have been exercising that same grant —
/// with a second transport as the price and nothing the printed command does not already do.
///
/// So this command derives the group, names it in the payload, and succeeds with the
/// `aws logs tail` invocation that reads it. `lines` is still explicitly `null` and never
/// `[]`, because an empty list is the wire shape for "the group exists and has no events" —
/// and that is the *exact* confusion the build-role-prefix finding exists to prevent. A role
/// granted the plausible-but-wrong `/aws/lambda/microvms/*` produces builds that write no
/// logs at all, every failure then reads `reason=unknown`, and a client that implied it had
/// read the group when it had not would make that indistinguishable a second time.
///
/// (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)
pub fn logs<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &LogsArgs,
) -> Result<Rendered, CliError> {
    // Resolved the same way every AWS command resolves it, and pinned into the printed
    // command: without `--region`, the tail reads whatever region the caller's shell
    // defaults to — ResourceNotFound at best, a same-named group in the wrong region at
    // worst, and this command's whole product is that the printed line is runnable as-is.
    let region = args.region.resolve(ctx.env)?;
    let group = format!(
        "{}/{}",
        microvms_core::control::image::BUILD_LOG_GROUP_PREFIX,
        args.image_name
    );
    let tail = format!("aws logs tail {group} --since 1h --format short --region {region}");
    let requires = "AWS CLI v2 — aws logs tail does not exist in v1 (verified present in \
                    2.35.7)";

    let mut data = Map::new();
    data.insert("logGroup".into(), json!(group));
    // Explicitly null rather than an empty array, so a consumer cannot read "no events":
    // this client did not read the group, and an empty list is the wire shape for "the
    // group exists and has no events" — the wrong-prefix signature this field must never
    // counterfeit.
    data.insert("lines".into(), Value::Null);
    data.insert("tailCommand".into(), json!(tail));
    data.insert("tailRequires".into(), json!(requires));
    // The build topology, labelled by role, so an agent handed this envelope knows what
    // it is looking for inside the group (issue #98). Measured 2026-08: one image build
    // runs three VMs and emits three log streams, and `logging.logStream` is an EXACT
    // stream name — so a configured stream collapses all three into one, distinguishable
    // across builds only by the per-build `/<16 hex>` suffix this client appends.
    data.insert(
        "streams".into(),
        json!([
            {
                "role": "docker-build",
                "description": "zip pull and docker image build — the VM that assembles \
                                the image from the code artifact",
            },
            {
                "role": "snapshot-graviton3",
                "description": "snapshot build for Graviton 3 — the snapshot VM is the \
                                one that starts the app, so app startup logs are here",
            },
            {
                "role": "snapshot-graviton4",
                "description": "snapshot build for Graviton 4 — the same snapshot pass \
                                for the other chipset generation; also starts the app",
            },
        ]),
    );

    let (kind, _) = response_type("logs");
    let text = format!(
        "build log group: {group}\n\
         read it with:    {tail}\n\
         requires:        {requires}\n\
         \n\
         This client does not read CloudWatch (CLI-2: no second path to AWS). The identity \
         running the tail needs logs:FilterLogEvents, logs:GetLogEvents and \
         logs:DescribeLogStreams on the group — the Terraform stack's logs_read_policy_arn \
         output is exactly that grant. The group name is the part that is easy to get wrong: \
         a build role granted /aws/lambda/microvms/* instead of /aws/lambda-microvms/* \
         produces builds that write no logs at all, and every failure then reads \
         reason=unknown. Inside the group, one build is THREE streams — data.streams labels \
         them by role — with random service-chosen names by default; a configured logStream \
         collapses all three into one exact stream (the member is not a prefix), \
         distinguished across builds only by the per-build /<16 hex> suffix this client \
         appends, and the resolved name is on the build envelope's logStream."
    );
    let dense = format!("{group}\t{tail}\t{requires}");
    Ok(Rendered::ok(kind, data, text, dense))
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
            fetch: &crate::provision::PanickingFetch,
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
        file.append(crate::history::Event::HookObserved {
            hook: "validate".to_string(),
            fired_at: 1_756_500_000,
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
        assert_eq!(lines.len(), 3, "one line per event: {}", rendered.text);
        assert!(lines[0].contains("launched"), "{}", lines[0]);
        assert!(
            lines[0].contains("endpoint=https://mvm-1.example"),
            "{}",
            lines[0]
        );
        // The hook line reads `hookObserved  firedAt=... hook=validate` — the generic
        // field tail, which is what keeps a record from a newer build renderable too.
        assert!(lines[1].contains("hookObserved"), "{}", lines[1]);
        assert!(lines[1].contains("hook=validate"), "{}", lines[1]);
        assert!(lines[1].contains("firedAt=1756500000"), "{}", lines[1]);
        assert!(lines[2].contains("terminated"), "{}", lines[2]);
        // Dense is the same rows, tab-separated, with seq in field one.
        let dense: Vec<&str> = rendered.dense_text.lines().collect();
        assert!(dense[0].starts_with("0\t"), "{}", rendered.dense_text);
        assert!(dense[1].starts_with("1\t"), "{}", rendered.dense_text);
        assert!(dense[2].starts_with("2\t"), "{}", rendered.dense_text);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **`logs` succeeds with the working `aws logs tail` command, and still refuses to
    /// imply the group is empty.**
    ///
    /// The success path is the 0.6.0 ruling on #79: asking where the build logs are and how
    /// to read them is a legitimate question, so the answer is the group, the tail command,
    /// and the AWS CLI v2 floor — exit 0. The distinction the old failure protected survives
    /// unchanged: `lines` is explicitly null, never an empty array, because an empty array
    /// is the wire shape for "the group has no events", which is exactly what a wrong
    /// build-role prefix produces — and this client did not read the group.
    #[test]
    fn logs_succeeds_with_the_tail_command_and_refuses_to_imply_it_is_empty() {
        let mut out = Output::new(Format::Plain, false, Vec::new(), Vec::new());
        let env = |_: &str| None;
        let seam = crate::seam::PanickingSeam;
        let mut context = ctx(&mut out, &seam, &env);
        let rendered = logs(
            &mut context,
            &LogsArgs {
                image_name: "agentd-conformance".to_string(),
                region: crate::cli::RegionFlags::default(),
            },
        )
        .expect("naming the group and the command that reads it is a success");

        assert_eq!(rendered.kind, "microvm.logs");
        assert_eq!(rendered.already_reported, None, "a plain success");
        assert_eq!(
            rendered.data["logGroup"],
            "/aws/lambda-microvms/agentd-conformance"
        );
        assert_eq!(
            rendered.data["lines"],
            Value::Null,
            "an empty array would read as 'the group has no events'"
        );
        // The working command, verbatim and runnable, in the payload and both renderings —
        // region-pinned, so it reads the same group this invocation resolved rather than
        // whatever region the caller's shell happens to default to.
        let tail = "aws logs tail /aws/lambda-microvms/agentd-conformance --since 1h \
                    --format short --region us-east-1";
        assert_eq!(rendered.data["tailCommand"], tail);
        assert!(rendered.text.contains(tail), "{}", rendered.text);
        assert!(
            rendered.dense_text.contains(tail),
            "{}",
            rendered.dense_text
        );
        // The version floor travels beside the command: `aws logs tail` is v2-only.
        let requires = rendered.data["tailRequires"]
            .as_str()
            .expect("a version note");
        assert!(requires.contains("AWS CLI v2"), "{requires}");
        assert!(rendered.text.contains("AWS CLI v2"), "{}", rendered.text);
        assert!(
            rendered.dense_text.contains("AWS CLI v2"),
            "{}",
            rendered.dense_text
        );
        // The prefix that is easy to get wrong is still named, and so is the read grant.
        assert!(
            rendered.text.contains("/aws/lambda/microvms/*"),
            "{}",
            rendered.text
        );
        assert!(
            rendered.text.contains("logs_read_policy_arn"),
            "the Terraform read grant is where a fresh install's AccessDenied gets fixed: {}",
            rendered.text
        );

        // The build topology, labelled by role (issue #98): one build is three VMs and
        // three streams, and the envelope is where an agent learns what to look for
        // inside the group. Exactly three, each with a role and a description, and the
        // three roles are the measured ones — the snapshot VMs are the ones that start
        // the app, so the descriptions have to say where app logs land.
        let streams = rendered.data["streams"]
            .as_array()
            .expect("a streams array");
        assert_eq!(streams.len(), 3, "one build is exactly three streams");
        let roles: Vec<&str> = streams
            .iter()
            .map(|stream| stream["role"].as_str().expect("a role"))
            .collect();
        assert_eq!(
            roles,
            ["docker-build", "snapshot-graviton3", "snapshot-graviton4"]
        );
        for stream in streams {
            assert!(
                stream["description"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "every role carries a description: {stream}"
            );
        }
        assert!(
            streams[1]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("starts the app"),
            "the snapshot VM is the one that starts the app, and the topology has to \
             say so: {streams:?}"
        );
        // The collapse hazard is in the text: a configured logStream is an exact
        // name, not a prefix, and only the per-build suffix keeps builds apart.
        assert!(
            rendered.text.contains("collapses all three"),
            "{}",
            rendered.text
        );
        assert!(rendered.text.contains("/<16 hex>"), "{}", rendered.text);
    }

    /// **The tail command is pinned to the resolved region, not the shell's default.**
    ///
    /// `--region` on `logs` resolves through the same [`crate::cli::RegionFlags::resolve`]
    /// every AWS command uses, and the resolved name lands in `tailCommand`. Without the
    /// pin, `microvm logs my-image --region eu-west-1` would hand back a command that reads
    /// whatever region the caller's shell defaults to — ResourceNotFound at best, a
    /// same-named group's logs from the wrong region at worst.
    ///
    /// **Falsification** — run 2026-08-31. Written while `logs` still discarded its `Ctx`
    /// and never read `args.region`; both assertions failed (no `--region` in the command),
    /// and resolving the region in `logs` turned it green.
    #[test]
    fn logs_pins_the_tail_command_to_the_resolved_region() {
        let mut out = Output::new(Format::Plain, false, Vec::new(), Vec::new());
        let env = |_: &str| None;
        let seam = crate::seam::PanickingSeam;
        let mut context = ctx(&mut out, &seam, &env);
        let rendered = logs(
            &mut context,
            &LogsArgs {
                image_name: "agentd-conformance".to_string(),
                region: crate::cli::RegionFlags {
                    region: Some(crate::cli::RegionArg::EuWest1),
                    unlisted_region: None,
                },
            },
        )
        .expect("a resolvable region is a success");

        let tail = rendered.data["tailCommand"].as_str().expect("a command");
        assert!(
            tail.ends_with("--region eu-west-1"),
            "the flag's region is the command's region: {tail}"
        );
        assert!(
            !tail.contains("us-east-1"),
            "the default did not leak past an explicit flag: {tail}"
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

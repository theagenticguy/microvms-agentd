// SPDX-License-Identifier: Apache-2.0
//! `ls`, `logs`, `manifest`, `constants` — the commands that touch no account.
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

use crate::cli::{LogsArgs, LsArgs};
use crate::commands::{Ctx, Rendered, response_type};
use crate::exit::{CliError, Exit};
use crate::ledger;
use crate::seam::state_dir;

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
/// `scripts/check-model-drift` can compare it against the pinned botocore model *and* against
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

    /// A seam that panics if entered, since none of these commands may touch AWS.
    struct NoAws;

    impl crate::seam::CoreSeam for NoAws {
        fn control_plane(
            &self,
            _region: microvms_core::Region,
        ) -> crate::seam::futures_util_shim::BoxFuture<
            '_,
            Result<microvms_core::control::ControlPlane, microvms_core::Error>,
        > {
            panic!("a local command reached the control plane")
        }

        fn open_sandbox(
            &self,
            _region: microvms_core::Region,
            _port: Option<u16>,
        ) -> crate::seam::futures_util_shim::BoxFuture<
            '_,
            Result<microvms_core::sandbox::Sandbox, microvms_core::Error>,
        > {
            panic!("a local command opened a sandbox")
        }

        fn attach_session(
            &self,
            _region: microvms_core::Region,
            _attach: crate::seam::Attach,
        ) -> crate::seam::futures_util_shim::BoxFuture<
            '_,
            Result<microvms_core::session::Session, microvms_core::Error>,
        > {
            panic!("a local command attached a session")
        }

        fn put_artifact(
            &self,
            _uri: &str,
            _bytes: Vec<u8>,
        ) -> crate::seam::futures_util_shim::BoxFuture<'_, Result<(), microvms_core::Error>>
        {
            panic!("a local command uploaded an artifact")
        }
    }

    /// `ls` with nothing outstanding says so rather than printing an empty line.
    #[test]
    fn ls_with_an_empty_state_directory_says_nothing_outstanding() {
        let mut out = Output::new(Format::Plain, false, Vec::new(), Vec::new());
        let env = |_: &str| Some("/nonexistent-microvm-state".to_string());
        let seam = NoAws;
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
        let seam = NoAws;
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
    /// The keys are the coupling: `scripts/check-model-drift` looks them up by name, and a
    /// rename makes it report nothing disagreed — which is worse than a crash.
    #[test]
    fn constants_emits_the_object_the_drift_gate_reads() {
        let mut out = Output::new(Format::Plain, false, Vec::new(), Vec::new());
        let env = |_: &str| None;
        let seam = NoAws;
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
}

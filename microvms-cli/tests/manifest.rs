// SPDX-License-Identifier: Apache-2.0
//! The manifest cross-check, at the process boundary: what `microvm manifest` actually emits
//! matches what `microvm --help` actually accepts.
//!
//! # Why this is not the same test as the in-crate one
//!
//! `src/manifest.rs`'s tests compare the generated manifest against the clap tree — both in
//! process, both from the same `Cli::command()` call. That catches a generator bug and cannot
//! catch a *dispatch* bug: a command in the tree that no arm routes, or a parameter the manifest
//! publishes that the parser refuses.
//!
//! So this file spawns the binary. Every command the manifest lists is invoked, and every closed
//! domain it publishes is fed back to the parser — the value it names must parse, and a value
//! outside it must not. That is the difference between "the manifest describes the tree" and "the
//! manifest describes the binary".

mod support;

use support::{TempDir, run};

/// The manifest as the binary emits it.
fn manifest() -> serde_json::Value {
    let outcome = run(&["manifest", "--json"], &[]);
    assert_eq!(
        outcome.exit_code(),
        0,
        "manifest must always work, with nothing configured: {}",
        outcome.stderr
    );
    let envelope = outcome.envelope();
    assert_eq!(envelope["type"], "microvm.manifest");
    envelope["data"].clone()
}

/// **The cross-check.** Every command the manifest lists is one the binary routes.
///
/// Invoked with `--help`, which reaches the parser and the dispatcher's arm resolution without
/// needing an account or arguments. A command listed but unrouted exits non-zero here; a command
/// routed but unlisted is caught by the in-crate equality against the clap tree.
///
/// **Falsification** — add a `Command` variant with no `RESPONSE_TYPES` row and the response-type
/// assertion below goes red; add one the manifest omits and the in-crate test goes red. Between
/// them there is no way to ship a command the manifest does not describe, which is what "never
/// hand-maintained" has to mean.
#[test]
fn every_command_the_manifest_lists_is_one_the_binary_routes() {
    let manifest = manifest();
    let commands = manifest["commands"].as_array().expect("an array");
    assert_eq!(
        commands.len(),
        24,
        "the lifecycle seven (quickstart included), the attached ten (shell, sync, and \
         attach included), and the local seven"
    );

    for command in commands {
        let name = command["name"].as_str().expect("a name");
        let outcome = run(&[name, "--help"], &[]);
        assert_eq!(
            outcome.exit_code(),
            0,
            "the manifest lists `{name}` but the binary does not route it: {}",
            outcome.stderr
        );
        // Each declares a namespaced response type and its keys, which is what an agent branches
        // on after `status`.
        let kind = command["responseType"].as_str().unwrap_or_default();
        assert!(
            kind.starts_with("microvm."),
            "{name} publishes no namespaced response type: {kind:?}"
        );
        assert!(
            !command["responseKeys"]
                .as_array()
                .expect("an array")
                .is_empty(),
            "{name} publishes no response keys"
        );
        assert_eq!(command["supportsJson"], true, "{name}");
    }
}

/// **CLI-5, round-tripped through the parser.** Every published domain value parses, and a value
/// outside the domain does not.
///
/// This is the assertion that makes `choices` trustworthy rather than decorative. The in-crate test
/// compares the manifest's domain against the enum; this one feeds the domain back to the real
/// binary, so a domain that is published but not enforced — the exact shape of an S1 guard
/// downgraded to a convenience flag — fails here.
#[test]
fn every_published_domain_is_the_domain_the_parser_enforces() {
    let manifest = manifest();
    let mut round_tripped = 0;

    for command in manifest["commands"].as_array().expect("an array") {
        let name = command["name"].as_str().expect("a name");
        for param in command["parameters"].as_array().expect("an array") {
            let Some(choices) = param["choices"].as_array() else {
                continue;
            };
            let flag = format!("--{}", param["name"].as_str().expect("a name"));
            // A command that needs positional arguments cannot be probed this way; `cost` and
            // `logs` between them cover both S1 options and take at most one positional.
            let positional: Vec<&str> = match name {
                "cost" | "ls" | "manifest" | "constants" | "doctor" => vec![],
                "logs" => vec!["img"],
                // Skipped rather than fabricated: `run`, `build`, `exec`, and the lifecycle
                // commands need real identifiers, and inventing them would make this test about
                // argument shapes rather than about domains. Their `--memory`/`--region` are the
                // *same* clap types, asserted through `cost` and `logs`.
                _ => continue,
            };

            for value in choices {
                let value = value.as_str().expect("a domain value is a string");
                let mut argv = vec![name];
                argv.extend(positional.iter().copied());
                argv.extend([flag.as_str(), value, "--json"]);
                let outcome = run(&argv, &[]);
                // Not "it succeeded": what is asserted is that the value was not *rejected as
                // an argument*, which is the only thing a domain claim is about.
                assert_ne!(
                    outcome.exit_code(),
                    2,
                    "the manifest publishes {value:?} in {name}'s {flag} domain, but the parser \
                     refuses it: {}",
                    outcome.stdout
                );
                round_tripped += 1;
            }

            // And a value outside the domain is refused, so the domain is a constraint rather than
            // a list of suggestions.
            let mut argv = vec![name];
            argv.extend(positional.iter().copied());
            argv.extend([flag.as_str(), "definitely-not-in-the-domain", "--json"]);
            let refused = run(&argv, &[]);
            assert_eq!(
                refused.exit_code(),
                2,
                "{name}'s {flag} accepted a value outside its published domain — an S1 guard \
                 downgraded to a suggestion (CLI-5): {}",
                refused.stdout
            );
            assert_eq!(refused.envelope()["code"], "ERR_INVALID_ARG");
        }
    }
    assert!(
        round_tripped >= 10,
        "only {round_tripped} domain values were round-tripped; the S1 options must actually be \
         reachable or this passes vacuously"
    );
}

/// The exit table the manifest publishes is the one the binary exits with.
///
/// Both halves of the contract in one place: an agent reads `exitCodes` to build its own branch
/// table, and a row whose integer the binary never produces would be a branch that never fires.
/// Checked against two rows an invocation can actually reach.
#[test]
fn the_published_exit_table_agrees_with_what_the_binary_exits() {
    let manifest = manifest();
    let rows = manifest["exitCodes"].as_array().expect("an array");
    assert_eq!(rows.len(), 17);

    let code_for = |integer: i64| -> String {
        rows.iter()
            .find(|row| row["exit"].as_i64() == Some(integer))
            .and_then(|row| row["code"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(code_for(0), "", "row 0 has no code");
    assert_eq!(code_for(2), "ERR_INVALID_ARG");
    assert_eq!(code_for(12), "ERR_PRECONDITION");
    assert_eq!(code_for(13), "ERR_EXEC_FAILED");

    // And the binary really produces those two.
    let invalid = run(&["cost", "--memory", "1500", "--json"], &[]);
    assert_eq!(invalid.exit_code(), 2);
    assert_eq!(invalid.envelope()["code"], code_for(2));

    // `build` with a binary path that does not exist fails the local preflight — no AWS
    // is involved. (`logs` used to be the ERR_PRECONDITION probe here; it succeeds since
    // the 0.6.0 ruling on #79.)
    let precondition = run(
        &[
            "build",
            "/definitely/not/here/agentd",
            "--build-role-arn",
            "arn:aws:iam::123456789012:role/build",
            "--artifact-uri",
            "s3://bucket/img.zip",
            "--json",
        ],
        &[],
    );
    assert_eq!(precondition.exit_code(), 12);
    assert_eq!(precondition.envelope()["code"], code_for(12));
}

/// `microvm manifest` defaults to JSON, unlike every other command.
///
/// The only consumer that asks for a manifest is one that parses it, so a bare invocation is
/// already what it wants — which is `cli.py:2278`'s `json: bool = True` and the reason for it.
///
/// (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)
#[test]
fn a_bare_manifest_invocation_emits_json() {
    let outcome = run(&["manifest"], &[]);
    assert_eq!(outcome.exit_code(), 0);
    // `manifest` is always JSON, so this parses without the global flag.
    let parsed: serde_json::Value = serde_json::from_str(&outcome.stdout).unwrap_or_else(|error| {
        panic!(
            "a bare manifest must be parseable ({error}): {}",
            outcome.stdout
        )
    });
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["type"], "microvm.manifest");

    // And no other command does this: a bare `ls` is human text.
    let ledgers = TempDir::new("manifest-default");
    let ls = run(&["ls", "--state-dir", ledgers.path()], &[]);
    assert!(
        serde_json::from_str::<serde_json::Value>(&ls.stdout).is_err(),
        "manifest is the only command that defaults to JSON: {}",
        ls.stdout
    );
}

/// **`microvm constants --emit-json` emits the bare object the drift gate reads.**
///
/// Unwrapped by an envelope, deliberately: `scripts/check-model-drift.py` compares key-for-key against
/// the pinned botocore model, and an envelope would put every comparison behind
/// `["data"]["constants"]` for no gain. The keys are the coupling — the script looks them up by
/// name, so a rename makes it report that nothing disagreed, which is worse than a crash.
///
/// The choice is recorded rather than assumed: the command is *present in the manifest* (so an
/// agent can discover it) and the global `--json` wraps the same object (so both consumers are
/// served), which is why there is no second source of truth.
#[test]
fn constants_emit_json_writes_the_bare_object_the_drift_gate_reads() {
    let outcome = run(&["constants", "--emit-json"], &[]);
    assert_eq!(outcome.exit_code(), 0, "{}", outcome.stderr);
    let parsed: serde_json::Value = serde_json::from_str(&outcome.stdout)
        .unwrap_or_else(|error| panic!("not one JSON document ({error}): {}", outcome.stdout));

    // No envelope: the object is at the top level.
    assert!(
        parsed.get("status").is_none(),
        "an envelope wrapped it: {parsed}"
    );
    // Every key the gate reads, spelled as `sandbox.py` names them.
    for key in [
        "MODEL_API_VERSION",
        "MAX_RUN_HOOK_PAYLOAD_BYTES",
        "MAX_IMAGE_NAME_LEN",
        "IMAGE_NAME_PATTERN",
        "MAX_DURATION_SEC",
        "MAX_MICROVM_HOOK_TIMEOUT_SEC",
        "MAX_IMAGE_HOOK_TIMEOUT_SEC",
        "MAX_HOOK_PORT",
        "CAPABILITIES",
        "ARCHITECTURES",
        "MAX_NETWORK_CONNECTORS",
        "MAX_RESOURCES",
        "MAX_CLIENT_TOKEN_LEN",
        "MODEL_IMAGE_READY_STATES",
        "TOLERATED_IMAGE_READY_STATES",
        "TERMINAL_STATES",
        "DEAD_STATES",
        "MICROVM_REGIONS",
        "SIZE_CLASSES",
    ] {
        assert!(
            parsed.get(key).is_some(),
            "{key} is missing; scripts/check-model-drift.py looks it up by name and a rename makes \
             it report that nothing disagreed: {parsed}"
        );
    }
    assert_eq!(parsed["MODEL_API_VERSION"], "2025-09-09");
    assert_eq!(parsed["MAX_RUN_HOOK_PAYLOAD_BYTES"], 4096);
    assert_eq!(
        parsed["MICROVM_REGIONS"]
            .as_array()
            .expect("an array")
            .len(),
        5
    );
    assert_eq!(
        parsed["SIZE_CLASSES"].as_array().expect("an array").len(),
        5
    );

    // Under the global `--json` the identical object arrives inside the envelope, so an agent that
    // wants the envelope is served without a second source of truth.
    let wrapped = run(&["constants", "--json"], &[]);
    assert_eq!(wrapped.exit_code(), 0);
    let envelope = wrapped.envelope();
    assert_eq!(envelope["type"], "microvm.constants");
    assert_eq!(envelope["data"]["constants"], parsed);
}

/// `constants` is discoverable in the manifest, which is the point of not hiding it.
///
/// The packet allowed hiding it from `--help`; it is not hidden, and the reason is that its
/// consumer is a script that a human has to be able to find. A hidden command is one whose only
/// documentation is the source of the script that calls it.
#[test]
fn constants_is_listed_in_the_manifest_rather_than_hidden() {
    let manifest = manifest();
    let constants = manifest["commands"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|command| command["name"] == "constants")
        .unwrap_or_else(|| panic!("constants is missing from the manifest"))
        .clone();
    assert_eq!(constants["responseType"], "microvm.constants");
    assert!(
        constants["summary"]
            .as_str()
            .expect("a summary")
            .contains("drift gate"),
        "the summary must say what it is for: {constants}"
    );

    // And `--help` lists it too.
    let help = run(&["--help"], &[]);
    assert!(
        help.stdout.contains("constants"),
        "constants must be discoverable: {}",
        help.stdout
    );
}

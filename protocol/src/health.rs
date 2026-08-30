// SPDX-License-Identifier: Apache-2.0
//! The `GET /v1/health` response.

use std::borrow::Cow;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `GET /v1/health` response.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct Health {
    // The daemon's own version, distinct from `PROTOCOL_VERSION`. `Cow` so the
    // daemon reports its `CARGO_PKG_VERSION` borrowed while a client deserializes
    // into an owned string.
    //
    // Not a doc comment, deliberately: schemars publishes doc comments as
    // `description` and `docs/schema.json` is byte-compared, so adding one here is a
    // schema change. The field was undocumented before the extraction and stays so.
    pub version: Cow<'static, str>,
    pub bootstrapped: bool,
    /// Free space on the daemon's working filesystem, and the reserve it is judged
    /// against.
    ///
    /// Reported so disk pressure is something an orchestrator *watches* rather than
    /// something it discovers from a failed write. anthropics/claude-code#59856
    /// filled two 10 GB disks to 100% with never-collected session directories and
    /// the first symptom was `useradd: No space left on device` — by which point
    /// every writer in the sandbox was already broken. A number on a health endpoint
    /// is what makes that curve visible while there is still time to act.
    ///
    /// `None` when free space could not be measured, which is deliberately distinct
    /// from zero: unmeasurable is not full, and a monitor that conflated them would
    /// page on a missing `statvfs`.
    pub disk: Option<DiskHealth>,
    /// Whether any startup identity repair step failed. True means the VM is serving
    /// with a value from the shared image still in place — a duplicate machine-id or
    /// boot_id — which is a security-relevant condition an operator may want to
    /// drain the VM over, but is never a reason for the daemon to refuse to serve.
    pub identity_degraded: bool,
    /// False when identity repair was switched off by config. Distinguished from a
    /// repair that ran and found nothing so a monitor can tell "opted out" from
    /// "nothing to do".
    pub identity_repaired: bool,
    /// Whether any exec is still running right now.
    ///
    /// Here so that an orchestrator *outside* the VM can decide whether to keep the
    /// VM alive, and the "outside" is the whole design. The platform measures
    /// idleness by inbound traffic through the endpoint proxy, and that proxy
    /// terminates outside the guest and forwards over loopback (measured;
    /// `docs/PLATFORM.md`, "The platform's own hook arrives over loopback"). A
    /// request a guest process sends to the daemon's own port never reaches the
    /// proxy, so no amount of in-guest traffic can reset the idle timer. A route
    /// that promised otherwise would be a keepalive that does not keep anything
    /// alive, discovered when a multi-hour run auto-suspends mid-work.
    ///
    /// What does reset it is a poll from outside, and this field is what makes such
    /// a poll *informed* rather than unconditional: the orchestrator polls, which is
    /// itself the inbound traffic, and reads whether the workload is busy to decide
    /// whether to keep polling. The assertion is therefore repeated and explicitly
    /// the caller's, which is what the daemon self-keepaliving would not be — a hung
    /// process would then bill to the 8-hour ceiling with nobody asking.
    ///
    /// Computed from the exec registry rather than remembered: true iff at least one
    /// registered exec has not yet published a result. An exec that exited and is
    /// waiting to be acked is not busy — its output is being *held*, not produced —
    /// so an orchestrator does not keep a VM alive for a command that finished.
    ///
    /// `#[serde(default)]`, unlike every field above it, and the asymmetry is not an
    /// oversight. The daemon is baked into an image while the client is installed
    /// separately, so a current client routinely talks to a daemon from whenever that
    /// image was built — and a required field would make `health()` fail outright
    /// against a daemon that predates it, turning a missing signal into an
    /// unreachable VM. False is also the right absence: a daemon that cannot say
    /// whether it is busy has not asserted that it is.
    #[serde(default)]
    pub busy: bool,
    /// How many execs are registered, in any phase.
    ///
    /// Alongside `busy` because the two answer different questions and a monitor
    /// wants both: `busy: false, execs: 0` is a fresh or drained VM, while
    /// `busy: false, execs: 7` is a VM holding seven unacked results that somebody
    /// still has to collect. Terminating the second loses output nobody read.
    ///
    /// Defaulted for the same reason as `busy`: a client routinely talks to a daemon
    /// baked into an older image, and zero is the honest reading of a daemon that
    /// does not report a count.
    #[serde(default)]
    pub execs: usize,
    /// Every lifecycle-hook invocation the daemon observed, oldest first.
    ///
    /// The platform writes no CloudWatch logs for the validate hook, so "did my
    /// validate hook even run?" has no answer anywhere else. Each entry is the
    /// daemon's own observation — the hook path that was posted and the daemon's
    /// clock when it arrived — never anything the guest printed.
    ///
    /// One trust caveat travels with this field and is stated rather than implied:
    /// the hook routes are unauthenticated and reachable over loopback from inside
    /// the guest, so a hostile workload can *forge additional* entries by posting
    /// the hook paths itself. It cannot remove or alter real ones — the daemon
    /// records before it responds and the log is append-capped, keeping the
    /// earliest entries — so the platform's real firings are the front of the list
    /// and later spam is what the cap drops.
    ///
    /// Defaulted for `busy`'s reason: an older daemon omits the field, and an empty
    /// list is the honest reading of a daemon that reports no observations.
    #[serde(default)]
    pub hooks: Vec<HookObservation>,
    /// How many hook invocations were dropped once the log reached its cap.
    ///
    /// Non-zero means the list above is the *earliest* invocations only — the cap
    /// keeps first-N so a guest spamming the unauthenticated hook routes cannot
    /// grow daemon memory or push the platform's real firings out of the record.
    ///
    /// Defaulted like `hooks`: an older daemon omits it, and zero is the honest
    /// reading of a daemon that dropped nothing it could tell us about.
    #[serde(default)]
    pub hooks_dropped: u64,
}

/// One lifecycle-hook invocation, as the daemon observed it.
///
/// A daemon-reported fact in the same trust class as an exec's exit code: the
/// daemon's word about a request it served, carrying nothing the guest printed.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct HookObservation {
    /// Which hook, spelled the way the platform's routes spell it: `ready`,
    /// `validate`, `run`, `suspend`, `resume`, or `terminate`.
    pub hook: String,
    /// Seconds since the epoch on the daemon's clock when the invocation arrived.
    ///
    /// The daemon's clock rather than any caller's, and recorded before the handler
    /// does any work — a run hook that was invoked and refused still fired.
    pub fired_at: u64,
}

/// The disk half of [`Health`].
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct DiskHealth {
    /// Bytes available to an unprivileged writer, from `statvfs` `f_bavail`.
    pub available_bytes: u64,
    /// Bytes that must stay free before a write is refused. Zero means the guard is
    /// disabled.
    pub reserve_bytes: u64,
    /// Whether a write would be refused right now. Precomputed rather than left to
    /// the client, so every consumer applies the same comparison the write path does.
    pub under_pressure: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `disk: null` is not `disk` absent: unmeasurable free space is distinct from
    /// zero, and a client that read a missing key as zero would page on a missing
    /// `statvfs`.
    #[test]
    fn health_reports_an_unmeasurable_disk_as_an_explicit_null() {
        let written = serde_json::to_string(&Health {
            version: Cow::Borrowed("0.1.0"),
            bootstrapped: true,
            disk: None,
            identity_degraded: false,
            identity_repaired: true,
            busy: false,
            execs: 0,
            hooks: Vec::new(),
            hooks_dropped: 0,
        })
        .expect("serializes");
        assert!(written.contains(r#""disk":null"#), "{written}");

        let read: Health = serde_json::from_str(&written).expect("deserializes");
        assert_eq!(read.version, "0.1.0");
        assert!(read.disk.is_none());
    }

    /// `busy` and `execs` round-trip, and they are separate facts: a VM holding
    /// unacked results is not busy, and an orchestrator that conflated the two would
    /// either keep a finished VM alive or terminate one whose output nobody read.
    #[test]
    fn health_carries_busy_and_the_exec_count_as_two_separate_facts() {
        let written = serde_json::to_string(&Health {
            version: Cow::Borrowed("0.1.0"),
            bootstrapped: true,
            disk: None,
            identity_degraded: false,
            identity_repaired: true,
            busy: false,
            execs: 7,
            hooks: Vec::new(),
            hooks_dropped: 0,
        })
        .expect("serializes");
        assert!(written.contains(r#""busy":false"#), "{written}");
        assert!(written.contains(r#""execs":7"#), "{written}");

        let read: Health = serde_json::from_str(&written).expect("deserializes");
        assert!(!read.busy);
        assert_eq!(read.execs, 7);
    }

    /// The hook observations round-trip, spelled as the routes spell them, with the
    /// dropped count beside them.
    #[test]
    fn health_carries_hook_observations_and_the_dropped_count() {
        let written = serde_json::to_string(&Health {
            version: Cow::Borrowed("0.1.0"),
            bootstrapped: true,
            disk: None,
            identity_degraded: false,
            identity_repaired: true,
            busy: false,
            execs: 0,
            hooks: vec![
                HookObservation {
                    hook: "validate".to_string(),
                    fired_at: 1_756_500_000,
                },
                HookObservation {
                    hook: "run".to_string(),
                    fired_at: 1_756_500_100,
                },
            ],
            hooks_dropped: 3,
        })
        .expect("serializes");
        // The wire keys are pinned to the response's own convention — snake_case,
        // like `identity_degraded` beside them. The CLI's history line respells
        // `fired_at` as `firedAt` because the history file is camelCase; a spelling
        // only one side writes fails exactly when someone reads it.
        assert!(
            written.contains(r#""hooks":[{"hook":"validate","fired_at":1756500000}"#),
            "{written}"
        );
        assert!(written.contains(r#""hooks_dropped":3"#), "{written}");

        let read: Health = serde_json::from_str(&written).expect("deserializes");
        assert_eq!(read.hooks.len(), 2);
        assert_eq!(read.hooks[0].hook, "validate");
        assert_eq!(read.hooks[0].fired_at, 1_756_500_000);
        assert_eq!(read.hooks_dropped, 3);
    }

    /// An old daemon's envelope — no `hooks`, no `hooks_dropped` — still parses, with
    /// the empty defaults. The daemon is baked into an image while the client is
    /// installed separately, so this is the compatibility floor, not an edge case.
    #[test]
    fn an_old_daemons_envelope_parses_with_no_hook_fields() {
        let read: Health = serde_json::from_str(
            r#"{"version": "0.1.0", "bootstrapped": true, "disk": null,
                 "identity_degraded": false, "identity_repaired": true}"#,
        )
        .expect("deserializes");
        assert!(read.hooks.is_empty());
        assert_eq!(read.hooks_dropped, 0);
        assert!(!read.busy);
        assert_eq!(read.execs, 0);
    }
}

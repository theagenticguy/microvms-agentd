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
        })
        .expect("serializes");
        assert!(written.contains(r#""disk":null"#), "{written}");

        let read: Health = serde_json::from_str(&written).expect("deserializes");
        assert_eq!(read.version, "0.1.0");
        assert!(read.disk.is_none());
    }
}

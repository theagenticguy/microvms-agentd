// SPDX-License-Identifier: Apache-2.0
//! What one invocation created, on disk, so an interrupt can name what it could not delete.
//!
//! # On disk rather than in memory only
//!
//! The identifiers are worthless to the operator if the process that held them is the process
//! that died. An image wedged in `CREATING` cannot be deleted afterward *at all*, and a
//! service-created log group outlives `terraform destroy`, so for both of those the
//! identifier **is** the remedy — there is no second way to find them.
//!
//! # Leaked is recorded before the delete is attempted
//!
//! Not after. The other order — try, then record on failure — loses the identifier when the
//! process dies inside the call, which is exactly the interrupt case this file exists for.
//! So [`Ledger::mark_outstanding`] writes first and [`Ledger::mark_deleted`] narrows the list
//! afterwards.
//!
//! # A file is only removed when nothing is outstanding
//!
//! [`Ledger::clear`] refuses while `leaked` is non-empty, because a leftover file is how
//! `microvm ls` knows there is something to tell the operator about. Clearing one that still
//! names a live resource would hide the one case the file exists for.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One invocation's record.
///
/// `camelCase` on the wire, matching `cli.py:550`'s `as_dict`, so a ledger written by either
/// client is readable by both — which matters because a caller debugging a leak will reach
/// for whichever `microvm ls` is on their PATH.
///
/// (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub run_id: String,
    pub region: String,
    #[serde(default)]
    pub image_identifier: Option<String>,
    #[serde(default)]
    pub image_name: Option<String>,
    #[serde(default)]
    pub microvm_id: Option<String>,
    /// Identifiers teardown tried and failed to remove. The operator's to-do list.
    #[serde(default)]
    pub leaked: Vec<String>,
}

/// A record plus where it is written.
#[derive(Clone, Debug)]
pub struct Ledger {
    pub record: Record,
    path: Option<PathBuf>,
}

impl Ledger {
    /// A ledger keyed by a per-invocation id, written under `root`.
    ///
    /// The id is `<epoch>-<pid>`, matching the Python. Not from a CSPRNG: this names a local
    /// file rather than an idempotency token, and core is emphatic that token minting is its
    /// own job (TRAP-1).
    pub fn new(region: &str, root: &Path) -> Self {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();
        let run_id = format!("{epoch}-{}", std::process::id());
        let path = root.join(format!("{run_id}.json"));
        Self {
            record: Record {
                run_id,
                region: region.to_string(),
                ..Record::default()
            },
            path: Some(path),
        }
    }

    /// Where this ledger writes, or `None` for one that writes nowhere.
    ///
    /// `cfg(test)` because nothing in the shipped paths asks: a handler either flushes or does
    /// not, and a handler that branched on the path would be one that could skip the flush.
    #[cfg(test)]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Writes the record. Failures are swallowed.
    ///
    /// Deliberately: this runs on the teardown path, and a ledger write that raised would
    /// replace the caller's real failure with a filesystem one — the same reason
    /// `Sandbox::terminate` returns a report rather than a `Result`. An unwritable state
    /// directory costs the operator the `ls` entry, and the identifiers are still in the
    /// failure envelope.
    pub fn flush(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(&self.record) {
            let _ = std::fs::write(path, text);
        }
    }

    /// Records the image, and flushes.
    pub fn record_image(&mut self, identifier: &str, name: &str) {
        self.record.image_identifier = Some(identifier.to_string());
        self.record.image_name = Some(name.to_string());
        self.flush();
    }

    /// Records the VM, and flushes.
    pub fn record_microvm(&mut self, id: &str) {
        self.record.microvm_id = Some(id.to_string());
        self.flush();
    }

    /// Marks everything this invocation created as outstanding, **before** any delete.
    ///
    /// See the module docs: this order is what survives a process that dies inside the
    /// terminate call.
    pub fn mark_outstanding(&mut self) {
        self.record.leaked = [
            self.record.microvm_id.clone(),
            self.record.image_identifier.clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        self.flush();
    }

    /// Narrows the outstanding list to what a teardown did **not** report deleted.
    ///
    /// Takes the survivors rather than the deletions, because that is the shape
    /// [`microvms_core::sandbox::TeardownReport::undeleted`] hands over — and translating
    /// between the two is where an inversion lives.
    pub fn mark_deleted(&mut self, still_undeleted: &[String]) {
        self.record.leaked = still_undeleted.to_vec();
        self.flush();
    }

    /// Removes the file, but only when nothing is outstanding.
    pub fn clear(&self) {
        if !self.record.leaked.is_empty() {
            return;
        }
        if let Some(path) = &self.path {
            // Swallowed for the same reason `write` is (see the module doc): a
            // failure to delete a clean run's record costs one stale file in the
            // state dir, and raising here would replace the command's real
            // outcome with a housekeeping error.
            let _ = std::fs::remove_file(path);
        }
    }
}

// ── the name registry ────────────────────────────────────────────────────────

/// A VM name's shape: ASCII letters, digits, `-`, `_`, at most 128 bytes, and never the
/// service's own `mvm-` prefix.
///
/// The charset is the image-name pattern (`[a-zA-Z0-9-_]+`), reused deliberately: it is what
/// makes the prefix discrimination in [`resolve`] total — an identifier starting with `mvm-`
/// can only be a MicroVM id, because a legal name is refused that prefix here, and an ARN
/// cannot match because `:` is outside the set. It also makes every name a safe file name,
/// which is what the registry stores it as.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a VM name cannot be empty".to_string());
    }
    if name.len() > 128 {
        return Err(format!(
            "a VM name is at most 128 bytes; this one is {}",
            name.len()
        ));
    }
    // Both spellings: `microvm-` is the id prefix the real service answers (measured
    // 2026-08-28, first live run of this feature — the fakes' `mvm-` fixture shape let a
    // passthrough keyed on `mvm-` alone pass every scripted test and fail against AWS),
    // and `mvm-` stays refused because it is the fixture shape every scripted body uses.
    if name.starts_with("microvm-") || name.starts_with("mvm-") {
        return Err(format!(
            "{name:?} starts with a MicroVM id prefix — a name shaped like an id would make \
             `microvm suspend <identifier>` ambiguous about which VM it addresses"
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '-' && *c != '_')
    {
        return Err(format!(
            "{bad:?} is not a legal VM-name character: names take ASCII letters, digits, `-` \
             and `_`, the image-name pattern"
        ));
    }
    Ok(())
}

/// One kept VM's local name, and everything an attach needs to address it.
///
/// The endpoint, token, and region ride along with the id because a name that resolved to an
/// id alone would still make the caller paste the rest of the triple — and the triple is the
/// thing the name exists to replace. `camelCase` on the wire, the ledger's own convention.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NameRecord {
    pub name: String,
    pub microvm_id: String,
    pub endpoint: String,
    /// The launch's agent token. On disk so `exec --name` can attach without the caller
    /// re-pasting it; the file is written owner-only on Unix for exactly that reason.
    pub agent_token: String,
    pub region: String,
    /// Seconds since the epoch, the ledger's own clock.
    pub at: u64,
    /// The launching host's identity secret, base64, when `run --identity` generated one.
    ///
    /// In this file for the reason the agent token is: `tunnel --name --verify-identity`
    /// runs in a later process, and a secret that did not persist would make the flag work
    /// only in the launching shell. The file is already 0600 and already a credential store
    /// — this raises what a stolen record can do from "call the VM" to "call the VM and
    /// impersonate the launching host to it", which is the same trust domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_host_seed: Option<String>,
    /// The VM's public key, base64 — the pin `--verify-identity` checks the far end against.
    ///
    /// Deliberately the *public* half: the VM's secret was dropped at launch
    /// (`LaunchIdentity::keep`), so no record anywhere lets anyone impersonate the VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_vm_public_key: Option<String>,
}

/// The name→VM registry: one JSON file per live name, under `<root>/names/`.
///
/// A subdirectory rather than the state root, for history's reason: `read_all`'s `*.json`
/// glob must never read a name record as a run ledger. Separate from [`Ledger`] itself
/// because the two have opposite lifecycles — a ledger file is cleared the moment nothing is
/// outstanding, and a name must live exactly as long as its VM: registered when `run --keep`
/// succeeds, removed when a terminate is accepted, and *refused* for reuse in between. That
/// refusal is the registry's whole job, and it costs zero AWS calls by construction: the
/// collision check is a local file read.
#[derive(Clone, Debug)]
pub struct Names {
    root: PathBuf,
}

impl Names {
    /// The registry under `state_root/names`.
    pub fn new(state_root: &Path) -> Self {
        Self {
            root: state_root.join("names"),
        }
    }

    fn path_of(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.json"))
    }

    /// The record registered under `name`, or `None`.
    ///
    /// An unreadable file reads as registered — its name is taken by *something*, and
    /// treating a torn record as free would let a second VM claim a name whose first holder
    /// may still be billing. The caller sees the collision refusal and can inspect the file.
    pub fn lookup(&self, name: &str) -> Option<NameRecord> {
        let text = std::fs::read_to_string(self.path_of(name)).ok()?;
        match serde_json::from_str::<NameRecord>(&text) {
            Ok(record) => Some(record),
            Err(_) => Some(NameRecord {
                name: name.to_string(),
                microvm_id: String::new(),
                endpoint: String::new(),
                agent_token: String::new(),
                region: String::new(),
                at: 0,
                identity_host_seed: None,
                identity_vm_public_key: None,
            }),
        }
    }

    /// Writes `record` under its name. The one registry write that reports failure,
    /// because it runs on the success path: a name that silently failed to register would
    /// make every later `exec --name` fail with "no VM named" while the VM bills on.
    pub fn register(&self, record: &NameRecord) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.path_of(&record.name);
        let text = serde_json::to_string_pretty(record)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        std::fs::write(&path, text)?;
        // Owner-only, because the record carries the agent token — a bearer credential for
        // the VM. Unix-only mechanics; the state dir under other platforms keeps the
        // profile's own ACLs.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Releases whatever name is registered to `microvm_id`, returning it.
    ///
    /// By VM id rather than by name, because the terminate path resolves its identifier
    /// before this runs — the id is the one spelling it always holds, whichever the caller
    /// typed. Failures are swallowed (a registry error must not displace the teardown's
    /// real outcome); a record that survives costs one stale collision refusal, whose
    /// message names the file.
    pub fn release_by_vm(&self, microvm_id: &str) -> Option<String> {
        let entries = std::fs::read_dir(&self.root).ok()?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Some(record) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<NameRecord>(&text).ok())
            else {
                continue;
            };
            if record.microvm_id == microvm_id {
                let _ = std::fs::remove_file(&path);
                return Some(record.name);
            }
        }
        None
    }
}

/// Every ledger under `root`, oldest first.
///
/// An unreadable file becomes a record naming itself rather than being skipped: a truncated
/// ledger is what a process killed mid-write leaves, which is exactly the case `ls` exists
/// for. Skipping it silently would hide the one run most likely to have leaked.
pub fn read_all(root: &Path) -> Vec<serde_json::Value> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "runId": stem,
                        "unreadable": path.to_string_lossy(),
                    })
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary directory that cleans itself up.
    ///
    /// To be replaced by the `tempfile` dev-dependency in the dependency sweep, along with
    /// the seven other copies of this pattern across the crate.
    struct TempDir(PathBuf, #[allow(dead_code)] tempfile::TempDir);

    impl TempDir {
        fn new(label: &str) -> Self {
            let dir = tempfile::Builder::new()
                .prefix(&format!("microvm-cli-{label}-"))
                .tempdir()
                .expect("a temp dir");
            Self(dir.path().to_path_buf(), dir)
        }
    }

    /// **CLI-6's persistence half.** The ledger survives the process, so `ls` can report a
    /// leak the run that created it never got to print.
    ///
    /// Written and read through the real file, not a mock: the whole property is that the
    /// bytes outlive the process, and an in-memory round trip would assert nothing about it.
    #[test]
    fn a_leaked_identifier_survives_into_a_file_a_later_invocation_reads() {
        let dir = TempDir::new("survives");
        let mut ledger = Ledger::new("us-east-1", &dir.0);
        ledger.record_image("arn:image", "img");
        ledger.record_microvm("mvm-1");
        ledger.mark_outstanding();
        // The delete of the VM succeeded and the image's did not, which is the common shape:
        // an image in CREATING refuses deletion while the VM goes away fine.
        ledger.mark_deleted(&["arn:image".to_string()]);

        let read = read_all(&dir.0);
        assert_eq!(read.len(), 1, "{read:?}");
        assert_eq!(read[0]["microvmId"], "mvm-1");
        assert_eq!(read[0]["leaked"], serde_json::json!(["arn:image"]));
        assert_eq!(read[0]["region"], "us-east-1");
    }

    /// A clean run leaves no ledger behind.
    ///
    /// The other half of the same rule: a file that stayed would make `ls` report a leak that
    /// does not exist, and an operator who is told about a phantom leak stops reading `ls`.
    #[test]
    fn a_clean_run_leaves_nothing_for_ls_to_report() {
        let dir = TempDir::new("clean");
        let mut ledger = Ledger::new("us-east-1", &dir.0);
        ledger.record_microvm("mvm-1");
        ledger.mark_outstanding();
        assert_eq!(read_all(&dir.0).len(), 1, "the outstanding record is there");

        ledger.mark_deleted(&[]);
        ledger.clear();
        assert!(read_all(&dir.0).is_empty(), "a clean run clears its ledger");
    }

    /// `clear` refuses while something is outstanding.
    ///
    /// **Falsification** — drop the `is_empty` check from `clear` and this test loses the
    /// file, so `microvm ls` reports nothing about a VM that is still billing.
    #[test]
    fn clear_refuses_to_remove_a_ledger_that_still_names_a_live_resource() {
        let dir = TempDir::new("refuses");
        let mut ledger = Ledger::new("us-east-1", &dir.0);
        ledger.record_microvm("mvm-1");
        ledger.mark_outstanding();
        ledger.clear();
        let read = read_all(&dir.0);
        assert_eq!(read.len(), 1, "the leak must survive a clear attempt");
        assert_eq!(read[0]["leaked"], serde_json::json!(["mvm-1"]));
    }

    /// A truncated ledger is reported as unreadable rather than skipped.
    ///
    /// It is what a process killed mid-write leaves, and that process is the one most likely
    /// to have leaked — so silence here is the worst available answer.
    #[test]
    fn a_truncated_ledger_is_reported_rather_than_skipped() {
        let dir = TempDir::new("truncated");
        std::fs::write(dir.0.join("1754524800-999.json"), "{\"runId\": \"175").expect("writes");
        let read = read_all(&dir.0);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0]["runId"], "1754524800-999");
        assert!(read[0]["unreadable"].is_string(), "{read:?}");
    }

    /// Outstanding is recorded before a delete is attempted, so the order is observable.
    ///
    /// Asserted by reading the file *between* the two calls, which is the only way to
    /// distinguish this from the losing order — both end in the same final state.
    #[test]
    fn the_outstanding_list_is_on_disk_before_any_delete_is_attempted() {
        let dir = TempDir::new("order");
        let mut ledger = Ledger::new("us-east-1", &dir.0);
        ledger.record_image("arn:image", "img");
        ledger.record_microvm("mvm-1");

        ledger.mark_outstanding();
        // This is the instant a killed process would die at, and what it leaves behind.
        let mid_flight = read_all(&dir.0);
        assert_eq!(
            mid_flight[0]["leaked"],
            serde_json::json!(["mvm-1", "arn:image"]),
            "both identifiers must already be on disk: {mid_flight:?}"
        );
    }

    /// An unwritable directory does not raise.
    ///
    /// The teardown path calls this, and a ledger write that raised would replace the
    /// caller's real failure with a filesystem one.
    #[test]
    fn an_unwritable_state_directory_is_survivable() {
        let root = Path::new("/proc/definitely-not-writable");
        let ledger = Ledger::new("us-east-1", root);
        ledger.flush();
        ledger.clear();
        // The path really is under the unwritable root, so the flush above attempted a write that
        // could not succeed — rather than a ledger that quietly writes nowhere and would pass
        // this test for the wrong reason.
        assert_eq!(ledger.path().and_then(Path::parent), Some(root));
        assert!(
            !root.join(format!("{}.json", ledger.record.run_id)).exists(),
            "nothing was written, and nothing raised"
        );
    }

    /// Multiple ledgers read back oldest-first, so `ls` output is stable.
    #[test]
    fn ledgers_read_back_in_a_deterministic_order() {
        let dir = TempDir::new("order-many");
        for id in ["1754524800-1", "1754524801-2", "1754524802-3"] {
            std::fs::write(
                dir.0.join(format!("{id}.json")),
                serde_json::to_string(&Record {
                    run_id: id.to_string(),
                    region: "us-east-1".to_string(),
                    ..Record::default()
                })
                .expect("serializes"),
            )
            .expect("writes");
        }
        let ids: Vec<String> = read_all(&dir.0)
            .iter()
            .map(|value| value["runId"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(ids, ["1754524800-1", "1754524801-2", "1754524802-3"]);
    }

    fn a_record(name: &str, id: &str) -> NameRecord {
        NameRecord {
            name: name.to_string(),
            microvm_id: id.to_string(),
            endpoint: format!("https://{id}.example"),
            agent_token: "tok-1".to_string(),
            region: "us-east-1".to_string(),
            at: 1754524800,
            identity_host_seed: None,
            identity_vm_public_key: None,
        }
    }

    /// A registered name resolves to its record, from a different `Names` value.
    ///
    /// Two values rather than one reused, for history's reason: `run --keep` registers and a
    /// later `microvm exec --name` — a different process — looks up. The file is the handoff.
    #[test]
    fn a_registered_name_survives_into_a_later_lookup() {
        let dir = TempDir::new("names-roundtrip");
        Names::new(&dir.0)
            .register(&a_record("ci-runner", "mvm-1"))
            .expect("registers");

        let found = Names::new(&dir.0).lookup("ci-runner").expect("registered");
        assert_eq!(found, a_record("ci-runner", "mvm-1"));
        assert!(Names::new(&dir.0).lookup("other").is_none());
    }

    /// A release by VM id frees exactly that VM's name and no other.
    #[test]
    fn a_release_by_vm_id_frees_that_name_and_no_other() {
        let dir = TempDir::new("names-release");
        let names = Names::new(&dir.0);
        names.register(&a_record("a", "mvm-1")).expect("registers");
        names.register(&a_record("b", "mvm-2")).expect("registers");

        assert_eq!(names.release_by_vm("mvm-2").as_deref(), Some("b"));
        assert!(names.lookup("b").is_none());
        assert!(names.lookup("a").is_some(), "the other name is untouched");
        assert_eq!(names.release_by_vm("mvm-99"), None, "nothing to release");
    }

    /// A torn record reads as taken rather than as free.
    ///
    /// A process killed mid-register leaves exactly this file, and its VM may be billing —
    /// so letting a second VM claim the name would point every later command at the wrong
    /// one. The refusal names the file; the operator deletes it deliberately.
    #[test]
    fn a_torn_name_record_reads_as_taken_not_free() {
        let dir = TempDir::new("names-torn");
        let names = Names::new(&dir.0);
        std::fs::create_dir_all(dir.0.join("names")).expect("mkdir");
        std::fs::write(dir.0.join("names").join("wedged.json"), "{\"name\": \"we").expect("writes");
        assert!(
            names.lookup("wedged").is_some(),
            "a torn record is a taken name"
        );
    }

    /// The name grammar: the image-name charset, no `mvm-` prefix, bounded length.
    ///
    /// The `mvm-` refusal is the one that makes bare-identifier resolution total: with it,
    /// `mvm-*` can only be an id, anything else in the charset can only be a name, and the
    /// two sets cannot collide.
    #[test]
    fn the_name_grammar_refuses_the_id_prefix_and_foreign_characters() {
        assert!(validate_name("ci-runner_2").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name(&"x".repeat(129)).is_err());
        assert!(
            validate_name("mvm-lookalike").is_err(),
            "a name shaped like a MicroVM id would make resolution ambiguous"
        );
        assert!(
            validate_name("microvm-9536a532").is_err(),
            "the real service's id prefix is microvm-, measured on the first live run — \
             only the test fixtures spell it mvm-"
        );
        for bad in ["with space", "dot.name", "slash/name", "colon:name"] {
            assert!(validate_name(bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// The name record's wire shape is `camelCase`, like every other file in the state dir.
    #[test]
    fn the_name_record_serializes_camel_case_and_round_trips() {
        let value = serde_json::to_value(a_record("ci", "mvm-1")).expect("serializes");
        let mut keys: Vec<&String> = value.as_object().expect("object").keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "agentToken",
                "at",
                "endpoint",
                "microvmId",
                "name",
                "region"
            ]
        );
        let back: NameRecord = serde_json::from_value(value).expect("round trips");
        assert_eq!(back, a_record("ci", "mvm-1"));
    }

    /// The wire shape is `camelCase`, matching the Python's ledger key for key.
    ///
    /// A caller debugging a leak reaches for whichever `microvm ls` is on their PATH, and a
    /// ledger only one client can read is a ledger that fails exactly then.
    #[test]
    fn the_record_serializes_with_the_pythons_camel_case_keys() {
        let record = Record {
            run_id: "1-2".to_string(),
            region: "us-east-1".to_string(),
            image_identifier: Some("arn:image".to_string()),
            image_name: Some("img".to_string()),
            microvm_id: Some("mvm-1".to_string()),
            leaked: vec!["mvm-1".to_string()],
        };
        let value = serde_json::to_value(&record).expect("serializes");
        let mut keys: Vec<&String> = value.as_object().expect("object").keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "imageIdentifier",
                "imageName",
                "leaked",
                "microvmId",
                "region",
                "runId"
            ]
        );
        let back: Record = serde_json::from_value(value).expect("round trips");
        assert_eq!(back, record);
    }
}

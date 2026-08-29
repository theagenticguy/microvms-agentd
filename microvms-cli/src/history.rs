// SPDX-License-Identifier: Apache-2.0
//! What was asked of one MicroVM and what the platform reported back, as a record that
//! outlives the VM.
//!
//! # A second file, not a wider ledger
//!
//! The obvious home for this was [`crate::ledger`], which already owns the state directory
//! and the camelCase wire shape. It was assessed and refused: the ledger's `clear()`
//! **deletes the file** when nothing leaked, because leak accounting wants records gone the
//! moment they are resolved — a stale entry is a phantom leak `microvm ls` reports forever.
//! History's whole value is the opposite property: the record survives terminate, because a
//! caller attesting over a run needs it precisely after the VM is gone. One file serving
//! both masters would corrupt one semantic to serve the other, so the two live apart and
//! neither touches the other's files.
//!
//! # Append-only, and nothing ever deletes a history file
//!
//! There is no `clear()` here and no remove anywhere in this module. A terminated VM's
//! history is the point, not the leftover.
//!
//! # `seq` is derived by counting, and single-writer is the assumption
//!
//! Each append reads the file and counts its lines. That is racy under two concurrent
//! writers to the same VM's history — both would claim one `seq` — and it is accepted,
//! stated here rather than papered over with a lock file: the realistic shape is one
//! process driving one VM at a time, and a lock file in the state directory would be one
//! more thing an interrupted process leaves behind. A reader that sees a duplicated `seq`
//! is seeing two writers, which is itself information.
//!
//! # Every value is the platform's
//!
//! Events carry what the control plane and the daemon reported — identifiers, endpoints,
//! exit codes, teardown verdicts — and never anything the guest printed. A record built
//! from guest output would be a record the workload can forge.
//!
//! # Failures are swallowed, like the ledger's
//!
//! [`History::append`] runs on teardown paths, and a history write that raised would
//! replace the caller's real failure with a filesystem one. An unwritable state directory
//! costs the operator the record and nothing else.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One thing that happened to a VM, as the platform told it.
///
/// Internally tagged on `event` so a line reads `{"seq":0,"at":...,"event":"launched",...}` —
/// the discriminant is the field a consumer branches on, the same shape the envelope's `type`
/// takes. `camelCase` on the wire, matching the ledger.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    /// This invocation's own build produced the image the VM launched from.
    ///
    /// Only `run` writes this, at run time, because only then is there a VM to key the
    /// record by — a standalone `microvm build` has no VM id and therefore no history file
    /// to append to.
    ImageBuilt {
        image_identifier: String,
        image_name: String,
    },
    /// The launch was accepted. The identifier and endpoint are the service's own answer.
    Launched {
        image_identifier: String,
        endpoint: String,
        region: String,
    },
    /// One exec, as the daemon reported it.
    ///
    /// `exit_code` is `None` for a detached start (the outcome is not known yet) and for a
    /// stream that was cut before its terminal event — reporting 0 for either would turn an
    /// unfinished command into a passing one. `writers_may_be_alive` travels only when the
    /// daemon's outcome carried it, which is why it is an `Option` rather than a defaulted
    /// bool.
    Exec {
        exec_id: String,
        exit_code: Option<i32>,
        truncated: bool,
        writers_may_be_alive: Option<bool>,
    },
    /// The service reported SUSPENDED.
    Suspended,
    /// The service reported RUNNING again.
    Resumed,
    /// A teardown ran. The fields are the [`microvms_core::sandbox::TeardownReport`] shape:
    /// whether the terminate call was accepted, and what a delete was asked for and did not
    /// remove.
    Terminated {
        terminate_accepted: bool,
        undeleted: Vec<String>,
    },
}

/// One line on the wire: the counter, the clock, and the event flattened beside them.
///
/// `at` is seconds since the epoch, matching the ledger's `run_id` clock — the CLI's one
/// timestamp convention, so a reader correlating a history line against a ledger file does
/// no unit conversion.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    seq: u64,
    at: u64,
    #[serde(flatten)]
    event: Event,
}

/// Where one VM's history is appended.
#[derive(Clone, Debug)]
pub struct History {
    path: PathBuf,
}

impl History {
    /// The history file for `vm_id` under `root` — `<root>/history/<vm_id>.jsonl`.
    ///
    /// A subdirectory rather than the root itself, so `ledger::read_all`'s `*.json` glob and
    /// this module's files can never be confused for one another by a tool listing the
    /// state directory.
    pub fn for_vm(root: &Path, vm_id: &str) -> Self {
        Self {
            path: root.join("history").join(format!("{vm_id}.jsonl")),
        }
    }

    /// Appends one event. Failures are swallowed.
    ///
    /// Deliberately, and for the ledger's reason: this runs on teardown paths, and a history
    /// write that raised would replace the caller's real failure with a filesystem one. The
    /// values are still in the envelope; only the durable record is lost.
    ///
    /// `seq` is the current line count — see the module docs on the single-writer
    /// assumption that makes counting sufficient.
    pub fn append(&self, event: Event) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let seq = std::fs::read_to_string(&self.path)
            .map(|text| text.lines().count() as u64)
            .unwrap_or(0);
        let record = Record {
            seq,
            at: epoch_secs(),
            event,
        };
        let Ok(line) = serde_json::to_string(&record) else {
            return;
        };
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

/// Every event recorded for `vm_id` under `root`, oldest first.
///
/// A missing file is an empty result, not an error: asking about a VM this state dir never
/// saw is a question, not a mistake — the caller may hold an id from another machine's
/// state directory, and "no record here" is the honest answer.
///
/// An unparseable line becomes a record naming itself rather than being skipped, mirroring
/// the ledger's unreadable convention: a truncated last line is what a process killed
/// mid-append leaves, and that process is the one whose history most needs reading.
pub fn read_events(root: &Path, vm_id: &str) -> Vec<serde_json::Value> {
    let path = root.join("history").join(format!("{vm_id}.jsonl"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .filter(serde_json::Value::is_object)
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "line": index + 1,
                        "unreadable": line,
                    })
                })
        })
        .collect()
}

/// Seconds since the epoch, the ledger's own clock.
fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary directory that cleans itself up. To be replaced by the `tempfile`
    /// dev-dependency in the dependency sweep, with the crate's other copies.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "microvm-cli-history-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn launched() -> Event {
        Event::Launched {
            image_identifier: "arn:image".to_string(),
            endpoint: "https://mvm-1.example".to_string(),
            region: "us-east-1".to_string(),
        }
    }

    /// An append round-trips through the real file into what a later invocation reads.
    ///
    /// Through the filesystem rather than a mock, for the ledger's reason: the whole
    /// property is that the bytes outlive the process, and an in-memory round trip would
    /// assert nothing about it.
    #[test]
    fn an_appended_event_round_trips_through_the_real_file() {
        let dir = TempDir::new("roundtrip");
        History::for_vm(&dir.0, "mvm-1").append(launched());

        let read = read_events(&dir.0, "mvm-1");
        assert_eq!(read.len(), 1, "{read:?}");
        assert_eq!(read[0]["event"], "launched");
        assert_eq!(read[0]["imageIdentifier"], "arn:image");
        assert_eq!(read[0]["endpoint"], "https://mvm-1.example");
        assert_eq!(read[0]["region"], "us-east-1");
        assert_eq!(read[0]["seq"], 0);
        assert!(read[0]["at"].as_u64().is_some_and(|at| at > 0), "{read:?}");
    }

    /// `seq` is monotonic across appends, including appends from two `History` values.
    ///
    /// Two values rather than one reused, because that is the real shape: `run` writes
    /// `launched` and a later `microvm terminate` — a different process entirely — writes
    /// `terminated`. A counter held in memory would restart at zero there; counting the
    /// file is what makes the sequence one sequence.
    #[test]
    fn seq_is_monotonic_across_two_appends_from_two_writers() {
        let dir = TempDir::new("seq");
        History::for_vm(&dir.0, "mvm-1").append(launched());
        History::for_vm(&dir.0, "mvm-1").append(Event::Suspended);

        let read = read_events(&dir.0, "mvm-1");
        assert_eq!(read.len(), 2);
        assert_eq!(read[0]["seq"], 0);
        assert_eq!(read[1]["seq"], 1);
        assert_eq!(read[1]["event"], "suspended");
    }

    /// The record survives a terminate: the terminated event appends, and the file stays.
    ///
    /// **Falsification** — add a ledger-style `clear()` after the `Terminated` append (the
    /// tidy-up this module's docs refuse) and both assertions go red: the whole value of
    /// history is being readable after the VM is gone. Broken exactly so on 2026-08-25,
    /// failed as stated, restored.
    #[test]
    fn a_terminated_event_appends_and_the_file_survives_it() {
        let dir = TempDir::new("survives-terminate");
        let history = History::for_vm(&dir.0, "mvm-1");
        history.append(launched());
        history.append(Event::Terminated {
            terminate_accepted: true,
            undeleted: vec!["arn:image".to_string()],
        });

        let read = read_events(&dir.0, "mvm-1");
        assert_eq!(read.len(), 2, "the record must outlive the VM: {read:?}");
        assert_eq!(read[1]["event"], "terminated");
        assert_eq!(read[1]["terminateAccepted"], true);
        assert_eq!(read[1]["undeleted"], serde_json::json!(["arn:image"]));
        assert!(
            dir.0.join("history").join("mvm-1.jsonl").exists(),
            "nothing ever deletes a history file"
        );
    }

    /// An unwritable directory does not raise.
    ///
    /// The teardown path calls this, and a history write that raised would replace the
    /// caller's real failure with a filesystem one.
    ///
    /// The unwritable root is a regular *file*, not `/proc/...`: a path component that is a
    /// file cannot become a directory on any platform, whereas `/proc/definitely-not-writable`
    /// resolves to a perfectly creatable `<drive>:\proc\...` on Windows — measured as exactly
    /// this test writing there and failing on the windows-latest CI tier.
    #[test]
    fn an_unwritable_state_directory_is_survivable() {
        let dir = TempDir::new("unwritable");
        let root = dir.0.join("a-file-not-a-directory");
        std::fs::write(&root, "occupied").expect("writes");
        // The append really is attempted against the unwritable root — rather than a
        // history that quietly writes nowhere and would pass for the wrong reason.
        History::for_vm(&root, "mvm-1").append(launched());
        assert!(
            read_events(&root, "mvm-1").is_empty(),
            "nothing was written, and nothing raised"
        );
        assert!(root.is_file(), "the root is still the file it was");
    }

    /// A truncated last line is reported as unreadable rather than skipped.
    ///
    /// It is what a process killed mid-append leaves, and that process is the one whose
    /// history most needs reading — so silence is the worst available answer. The intact
    /// lines before it still parse, because one bad line must not cost the record.
    #[test]
    fn a_truncated_last_line_is_reported_rather_than_skipped() {
        let dir = TempDir::new("truncated");
        History::for_vm(&dir.0, "mvm-1").append(launched());
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(dir.0.join("history").join("mvm-1.jsonl"))
                .expect("opens");
            write!(file, "{{\"seq\":1,\"at\":175").expect("writes the torn tail");
        }

        let read = read_events(&dir.0, "mvm-1");
        assert_eq!(
            read.len(),
            2,
            "the torn line is a record, not a skip: {read:?}"
        );
        assert_eq!(read[0]["event"], "launched");
        assert!(read[1]["unreadable"].is_string(), "{read:?}");
        assert_eq!(read[1]["line"], 2);
    }

    /// The wire shape is camelCase, key for key.
    ///
    /// Pinned the way the ledger pins its own: the file is read by whichever `microvm` is
    /// on a caller's PATH, and a record only one build can read fails exactly when someone
    /// reaches for it.
    #[test]
    fn every_event_serializes_with_camel_case_keys() {
        let dir = TempDir::new("camelcase");
        let history = History::for_vm(&dir.0, "mvm-1");
        history.append(Event::ImageBuilt {
            image_identifier: "arn:image".to_string(),
            image_name: "img".to_string(),
        });
        history.append(launched());
        history.append(Event::Exec {
            exec_id: "x-1".to_string(),
            exit_code: Some(0),
            truncated: false,
            writers_may_be_alive: Some(false),
        });
        history.append(Event::Terminated {
            terminate_accepted: false,
            undeleted: vec!["mvm-1".to_string()],
        });

        let read = read_events(&dir.0, "mvm-1");
        let keys_of = |value: &serde_json::Value| -> Vec<String> {
            let mut keys: Vec<String> = value
                .as_object()
                .expect("an object")
                .keys()
                .cloned()
                .collect();
            keys.sort();
            keys
        };
        assert_eq!(
            keys_of(&read[0]),
            ["at", "event", "imageIdentifier", "imageName", "seq"]
        );
        assert_eq!(
            keys_of(&read[1]),
            [
                "at",
                "endpoint",
                "event",
                "imageIdentifier",
                "region",
                "seq"
            ]
        );
        assert_eq!(
            keys_of(&read[2]),
            [
                "at",
                "event",
                "execId",
                "exitCode",
                "seq",
                "truncated",
                "writersMayBeAlive"
            ]
        );
        assert_eq!(
            keys_of(&read[3]),
            ["at", "event", "seq", "terminateAccepted", "undeleted"]
        );

        // And the enum round-trips, so the reader above is the writer's own shape.
        let line = serde_json::to_string(&Record {
            seq: 9,
            at: 1,
            event: Event::Suspended,
        })
        .expect("serializes");
        let back: Record = serde_json::from_str(&line).expect("round trips");
        assert_eq!(back.event, Event::Suspended);
        assert_eq!(back.seq, 9);
    }

    /// A VM this state dir never saw reads back empty, not as an error.
    ///
    /// Asking is a question, not a mistake: the id may be real and the record may live in
    /// another machine's state directory.
    #[test]
    fn an_unknown_vm_reads_back_as_a_clean_empty_record() {
        let dir = TempDir::new("unknown");
        assert!(read_events(&dir.0, "mvm-never-seen").is_empty());
    }
}

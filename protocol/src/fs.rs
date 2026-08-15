// SPDX-License-Identifier: Apache-2.0
//! File-transfer wire types: the one query string every `/v1/fs/*` route takes.
//!
//! There are no body types here. The fs bodies are opaque byte streams — a file's
//! contents or an uncompressed tar — so the query is the entire typed surface, and
//! its failures answer `text/plain` rather than [`crate::exec::ErrorBody`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Query string for every route in this module.
///
/// `path` is required. A request missing it is 400, never 404: clients map 404
/// onto `FileNotFoundError`, so answering 404 for a protocol typo made a missing
/// query key look like an absent artifact — that is how one defect hid for a full
/// review round.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct FsQuery {
    pub path: String,
    /// Octal mode for a written file, carried as a string so `0644` and `644`
    /// both parse and neither is read as decimal 644.
    #[serde(default)]
    pub mode: Option<String>,
}

/// Query string for `GET /v1/fs/file`, which is [`FsQuery`] plus an optional line
/// range.
///
/// A separate type rather than two more fields on [`FsQuery`], because a line range
/// means nothing on `PUT /v1/fs/file` or on either tar route and a shared type would
/// publish it on all four. `mode` is absent here for the same reason pointing the
/// other way: it is a property of a write.
///
/// The semantics are the AI SDK harness contract's, verbatim, because that is the
/// consumer: **1-based and inclusive on both ends**, and an `end_line` past the
/// file's last line reads through EOF without an error. A caller asking for lines
/// 1..1000 of a 12-line file gets the 12 lines and a 200, not a 416.
#[derive(Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct FileReadQuery {
    pub path: String,
    /// First line to return, 1-based inclusive. Absent means 1.
    ///
    /// Zero is refused with 400 rather than treated as 1. A caller who sends 0 is
    /// working from a 0-based mental model, and silently reinterpreting it would
    /// hand back a window one line off from the one they will compute offsets
    /// against.
    #[serde(default)]
    pub start_line: Option<u64>,
    /// Last line to return, 1-based inclusive. Absent means through EOF.
    ///
    /// Past the last line is **not** an error: the read returns through EOF. A range
    /// ending before `start_line` is refused with 400, because it can only be a
    /// caller who computed one of the two wrong — there is no file for which it is
    /// the right question.
    #[serde(default)]
    pub end_line: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mode` is a string on the wire, and the reason is that `0644` in JSON is
    /// either a syntax error or decimal 644 depending on the parser.
    #[test]
    fn an_fs_query_without_a_mode_deserializes_and_keeps_the_mode_a_string() {
        let query: FsQuery = serde_json::from_str(r#"{"path":"/tmp/f"}"#).expect("deserializes");
        assert_eq!(query.path, "/tmp/f");
        assert!(query.mode.is_none());

        let with_mode: FsQuery =
            serde_json::from_str(r#"{"path":"/tmp/f","mode":"0644"}"#).expect("deserializes");
        assert_eq!(with_mode.mode.as_deref(), Some("0644"));
    }

    /// A read query with no range deserializes to two `None`s, which is what keeps
    /// the un-ranged read byte-identical to what it always was.
    #[test]
    fn a_file_read_query_without_a_range_leaves_both_bounds_absent() {
        let query: FileReadQuery =
            serde_json::from_str(r#"{"path":"/tmp/f"}"#).expect("deserializes");
        assert_eq!(query.path, "/tmp/f");
        assert!(query.start_line.is_none());
        assert!(query.end_line.is_none());

        let ranged: FileReadQuery =
            serde_json::from_str(r#"{"path":"/tmp/f","start_line":3,"end_line":9}"#)
                .expect("deserializes");
        assert_eq!(ranged.start_line, Some(3));
        assert_eq!(ranged.end_line, Some(9));
    }
}

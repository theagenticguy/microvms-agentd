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
}

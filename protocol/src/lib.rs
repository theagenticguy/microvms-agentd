//! The `microvms-agentd` wire contract as types, shared by the daemon and every
//! Rust client of it.
//!
//! # Why this is a crate rather than a copy on each side
//!
//! The daemon serializes these shapes and a client deserializes them, and for as
//! long as each side owned its own definitions the only thing keeping them in step
//! was review. That is the failure mode `docs/schema.json` was built to catch after
//! the fact; this crate makes the class of error unavailable in the first place. A
//! field renamed here breaks compilation on whichever side has not caught up, which
//! is the earliest and cheapest place a protocol change can fail.
//!
//! # What lives here and what does not
//!
//! Pure data only: serde plus schemars, no tokio, no axum, no base64. The rule is
//! whether the type is *what travels* or *machinery for making it travel*. So the
//! SSE event payloads are here and the stream that emits them is not; the base64
//! field is here as a `String` and the encoding is in the daemon's handler; the
//! error body is here and the `fail(status, slug, detail)` helper that pairs it with
//! an axum `StatusCode` is not.
//!
//! Every type derives both `Serialize` and `Deserialize` even where one side needs
//! only one of them, because the missing half is exactly what a client would have to
//! hand-write. `docs/schema.json` is generated from these attributes under both
//! contracts and byte-compared in CI, so a shape that reads differently than it
//! writes is a test failure rather than a consumer's problem.

pub mod exec;
pub mod fs;
pub mod health;
pub mod hook;

/// Protocol version, distinct from the daemon version.
///
/// It tracks the `/v1/` path namespace rather than the crate version: a patch to
/// the daemon does not change the wire format, and a client that pinned a daemon
/// version would refuse an upgrade it is compatible with.
///
/// What a client should do on a mismatch, stated here because it is the question a
/// version number exists to answer:
///
/// * **Same `protocol_version`, different `daemon_version`** — proceed. The path
///   namespace and every shape in `$defs` are unchanged; only the implementation
///   moved. This is the normal case and must not be treated as an error.
/// * **Different `protocol_version`** — do not proceed against `/v1/`. A new
///   protocol version means a shape or a status code changed meaning, and a client
///   built for `1` cannot tell a changed meaning from a bug. Fetch `/v1/schema`,
///   which stays unauthenticated precisely so this is diagnosable, and fail with
///   the two versions named.
/// * **The `microvms-agentd-version` response header disagrees with the schema
///   document's `daemon_version`** — you are talking to two daemons through one
///   endpoint, or to a proxy that rewrote a response. Treat it as a transport
///   fault, not a version negotiation.
///
/// There is deliberately no negotiation *request*: the daemon serves exactly one
/// protocol version, and a client that has read this document already knows which.
pub const PROTOCOL_VERSION: &str = "1";

/// The response header carrying the daemon version, on every response including
/// errors.
///
/// Lowercase because HTTP/2 requires it and hyper normalizes anyway; naming it
/// once here keeps the layer, the schema document, and any test asserting on it
/// from drifting into three spellings of the same header.
pub const VERSION_HEADER: &str = "microvms-agentd-version";

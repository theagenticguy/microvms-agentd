// SPDX-License-Identifier: Apache-2.0
//! The committed `docs/schema.json` is the published contract, so this tier asks
//! the two questions a consumer's trust rests on: is it what the code says, and
//! does it describe the surface the daemon actually serves?
//!
//! Both are checked by construction rather than by reading. The staleness check
//! regenerates and compares; the coverage check drives every documented route
//! through the real router and asserts the router answers. A route added without a
//! doc entry cannot be served at all — `routes::app` is assembled from the same
//! list — so what remains to catch is the reverse: a documented route that no
//! longer exists, and a status code the document promises that the daemon has
//! stopped producing.

use std::path::{Path, PathBuf};

use agentd::{AppState, Config, routes, schema};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt as _;

fn artifact_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace parent")
        .join("docs/schema.json")
}

fn generated() -> String {
    let document = schema::document(&Config::default(), &routes::surface_docs());
    let mut rendered = serde_json::to_string_pretty(&document).expect("the document serializes");
    rendered.push('\n');
    rendered
}

/// The guard that keeps a generated artifact from rotting. Everything else here
/// tests the generator; this tests the file in the repository.
#[test]
fn the_committed_artifact_matches_what_the_code_generates() {
    let path = artifact_path();
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));

    assert_eq!(
        committed,
        generated(),
        "{} is stale. Regenerate it with: cargo run -p agentd --bin schema",
        path.display(),
    );
}

/// Proves the check above can fail, rather than trusting that it would.
///
/// A staleness guard nobody has watched fail is a guard that might be comparing a
/// string to itself. The mutation is the smallest one that a semantic comparison
/// would wave through and a byte comparison must not: one whitespace character.
#[test]
fn the_staleness_check_rejects_an_edited_artifact() {
    let mut edited = generated();
    edited.push('\n');
    assert_ne!(
        edited,
        generated(),
        "the comparison is insensitive to an edit, so it cannot detect staleness"
    );

    // And a real content edit, which is what actually happens: a field renamed in
    // the source and the artifact left behind.
    let tampered = generated().replace("\"exec_id\"", "\"execId\"");
    assert_ne!(
        tampered,
        generated(),
        "the artifact has no exec_id to rename"
    );
}

#[test]
fn the_artifact_is_valid_json_and_carries_the_expected_top_level_keys() {
    let committed: Value = serde_json::from_str(
        &std::fs::read_to_string(artifact_path()).expect("the artifact is readable"),
    )
    .expect("the artifact is valid JSON");

    for key in [
        "$schema",
        "$defs",
        "routes",
        "limits",
        "daemon_version",
        "protocol_version",
        "version_header",
        "hook_prefix",
        "auth",
    ] {
        assert!(
            committed.get(key).is_some(),
            "the document is missing {key}"
        );
    }

    assert_eq!(
        committed["$schema"],
        "https://json-schema.org/draft/2020-12/schema",
    );
    assert_eq!(committed["definition_collisions"], serde_json::json!([]));
}

const PROBE_TOKEN: &str = "probe-token";

/// Sends `method path` through the real router with a valid bearer token.
///
/// Authenticated on purpose, and it took a failed mutation to learn why. An
/// unauthenticated probe cannot see a missing route at all: `/v1/exec/{id}` is a
/// wildcard, so it *matches* `/v1/exec/start`, and the auth middleware sitting on
/// that match answers 503 before axum ever decides the method is wrong. Every probe
/// under `/v1/exec/` therefore came back 503 whether or not the route existed, and
/// the coverage assertion below was passing vacuously. With a token the request
/// reaches method dispatch and a stale doc entry surfaces as the 404 or 405 it is.
async fn probe(endpoint: &schema::Endpoint) -> StatusCode {
    // Fresh state per probe: the run hook mutates bootstrap state, so a shared one
    // would make the outcome depend on iteration order.
    let state = AppState::new(Config::default());
    state.bootstrap(PROBE_TOKEN.as_bytes(), std::collections::HashMap::new());
    let app = routes::app(state);

    // `{id}` is axum's capture syntax, not a literal segment.
    let concrete = endpoint.path.replace("{id}", "probe");
    let response = app
        .oneshot(
            Request::builder()
                .method(endpoint.method)
                .uri(&concrete)
                .header("authorization", format!("Bearer {PROBE_TOKEN}"))
                .body(Body::empty())
                .expect("a well-formed probe request"),
        )
        .await
        .expect("the router answered");
    response.status()
}

/// Every documented route is reachable on the real router.
///
/// The router is built from the same list, so this cannot catch an undocumented
/// route — it catches the opposite and equally bad case, a document that promises a
/// path the daemon no longer serves. A 404 on a path with no `{id}`, or any 405,
/// means the entry is a lie.
#[tokio::test]
async fn every_documented_route_is_served_by_the_router() {
    for endpoint in routes::surface_docs() {
        let status = probe(&endpoint).await;

        assert_ne!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{} {} is documented but the router does not accept that method",
            endpoint.method,
            endpoint.path,
        );

        // A 404 on an `{id}` route is the handler saying the id is unknown, which
        // is correct and expected. On a fixed path it can only mean the router has
        // no such route.
        if !endpoint.path.contains('{') {
            assert_ne!(
                status,
                StatusCode::NOT_FOUND,
                "{} {} is documented but the router has no such path",
                endpoint.method,
                endpoint.path,
            );
        }
    }
}

/// The status a probe actually receives has to be one the document lists.
///
/// This is the half that catches drift in the direction a reader cares about: the
/// codes are the part of this protocol a client gets wrong, and each one was bought
/// with a defect. An empty body against a route expecting JSON is a documented 400,
/// a missing `?path=` is a documented 400, and an unknown exec id is a documented
/// 404 — so a route that answers something the table omits is a table that is wrong.
#[tokio::test]
async fn an_authenticated_probe_gets_a_status_the_document_promises() {
    for endpoint in routes::surface_docs() {
        let observed = probe(&endpoint).await;
        let promised: Vec<u16> = endpoint
            .statuses
            .iter()
            .map(|status| status.code.as_u16())
            .collect();
        assert!(
            promised.contains(&observed.as_u16()),
            "{} {} answered {} but the document promises only {promised:?}",
            endpoint.method,
            endpoint.path,
            observed.as_u16(),
        );
    }
}

/// The 503-before-bootstrap rule, on every bearer route, checked against the same
/// table that publishes it. Not 404 and not a dropped connection: a client mapping
/// 404 onto "not found" reports a phantom absent artifact, which is how one defect
/// hid for a full review round.
#[tokio::test]
async fn every_bearer_route_answers_503_before_bootstrap() {
    for endpoint in routes::surface_docs() {
        if endpoint.auth != schema::Auth::Bearer {
            continue;
        }
        let app = routes::app(AppState::new(Config::default()));
        let concrete = endpoint.path.replace("{id}", "probe");
        let response = app
            .oneshot(
                Request::builder()
                    .method(endpoint.method)
                    .uri(&concrete)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("the router answered");

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{} {} does not answer 503 before bootstrap",
            endpoint.method,
            endpoint.path,
        );
        assert!(
            endpoint
                .statuses
                .iter()
                .any(|status| status.code == StatusCode::SERVICE_UNAVAILABLE),
            "{} {} answers 503 but the document does not say so",
            endpoint.method,
            endpoint.path,
        );
    }
}

/// `PROTOCOL.md` has promised this header since the first commit. It is asserted on
/// the 404 fallback and on an auth rejection as well as on a success, because those
/// are the responses that bypass every handler — and a header only present on the
/// happy path is one a client cannot use as a precondition.
#[tokio::test]
async fn the_version_header_is_on_every_response_including_errors() {
    let cases: Vec<(&str, &str, StatusCode)> = vec![
        ("GET", "/v1/health", StatusCode::OK),
        ("GET", "/v1/schema", StatusCode::OK),
        // Before bootstrap, so the auth middleware answers without reaching a
        // handler.
        ("GET", "/v1/exec/nope", StatusCode::SERVICE_UNAVAILABLE),
        // The fallback, which is not a route at all.
        ("GET", "/no/such/path", StatusCode::NOT_FOUND),
    ];

    for (method, path, expected) in cases {
        let app = routes::app(AppState::new(Config::default()));
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("the router answered");

        assert_eq!(response.status(), expected, "{method} {path}");
        let header = response
            .headers()
            .get(routes::VERSION_HEADER)
            .unwrap_or_else(|| {
                panic!(
                    "{method} {path} answered {} with no {} header",
                    expected.as_u16(),
                    routes::VERSION_HEADER,
                )
            });
        assert_eq!(header.to_str().expect("ascii header"), routes::VERSION);
    }
}

/// The unauthenticated schema route is the whole version-negotiation story: a client
/// has to be able to read the contract during the window before a token exists.
#[tokio::test]
async fn the_schema_route_answers_before_bootstrap_and_agrees_with_the_committed_file() {
    let app = routes::app(AppState::new(Config::default()));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/schema")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("the router answered");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the schema must be readable before a token is installed, or a client \
         cannot negotiate a version during the only window in which it needs to"
    );

    let bytes = axum::body::to_bytes(response.into_body(), 4 << 20)
        .await
        .expect("body");
    let served: Value = serde_json::from_slice(&bytes).expect("the served document is JSON");
    let committed: Value =
        serde_json::from_str(&generated()).expect("the generated document is JSON");

    // Compared as parsed JSON, not as bytes: the route serves compact JSON and the
    // file is pretty-printed, and the contract is the content.
    assert_eq!(
        served, committed,
        "the live route and the committed artifact describe different protocols"
    );
}

/// A limit a client cannot read is a limit it discovers by tripping, and two of
/// these are indistinguishable from a transport failure when they fire.
#[test]
fn the_document_publishes_every_operative_limit() {
    let document = schema::document(&Config::default(), &routes::surface_docs());
    let limits = &document["limits"];
    let defaults = Config::default();

    assert_eq!(limits["max_body_bytes"], defaults.max_body_bytes);
    assert_eq!(limits["max_output_bytes"], defaults.max_output_bytes);
    assert_eq!(
        limits["max_stdin_write_bytes"],
        defaults.max_stdin_write_bytes
    );
    assert_eq!(limits["stream_replay_bytes"], defaults.stream_buffer_bytes);
    assert_eq!(
        limits["sse_keepalive_secs"],
        defaults.sse_keepalive.as_secs_f64()
    );
}

/// The load-bearing status codes, asserted individually because each one was bought
/// with a defect and prose alone has already failed to keep them straight once.
#[test]
fn the_non_obvious_status_codes_are_in_the_machine_readable_artifact() {
    let document = schema::document(&Config::default(), &routes::surface_docs());
    let routes_doc = document["routes"].as_array().expect("routes is an array");

    let codes_for = |method: &str, path: &str| -> Vec<u64> {
        routes_doc
            .iter()
            .find(|route| route["method"] == method && route["path"] == path)
            .unwrap_or_else(|| panic!("{method} {path} is not in the document"))["statuses"]
            .as_array()
            .expect("statuses is an array")
            .iter()
            .map(|status| status["code"].as_u64().expect("a numeric code"))
            .collect()
    };

    // 503 before bootstrap and 401 for a wrong token, on every bearer route, and
    // never 404 in either case — a client mapping 404 onto "not found" turns a
    // protocol state into a phantom absent artifact.
    for (method, path) in [
        ("POST", "/v1/exec/start"),
        ("GET", "/v1/exec/{id}"),
        ("GET", "/v1/exec/{id}/stream"),
        ("POST", "/v1/exec/{id}/stdin"),
        ("POST", "/v1/exec/{id}/ack"),
        ("POST", "/v1/exec/{id}/kill"),
        ("GET", "/v1/fs/file"),
        ("PUT", "/v1/fs/file"),
        ("GET", "/v1/fs/tar"),
        ("PUT", "/v1/fs/tar"),
    ] {
        let codes = codes_for(method, path);
        assert!(codes.contains(&503), "{method} {path} omits 503");
        assert!(codes.contains(&401), "{method} {path} omits 401");
    }

    // 409 on a hijack attempt at the hook, which is what makes bootstrap one-shot
    // observable to a caller.
    let run = codes_for("POST", &format!("{}/run", routes::HOOK_PREFIX));
    assert!(run.contains(&409), "the run hook omits the conflict case");
    assert!(run.contains(&400), "the run hook omits the malformed case");

    // 409 versus 410 on stdin: "you did not ask for stdin" is fixable at start
    // time, "stdin is closed" never is, and a client retries one and not the other.
    let stdin = codes_for("POST", "/v1/exec/{id}/stdin");
    assert!(stdin.contains(&409), "stdin omits stdin_not_requested");
    assert!(stdin.contains(&410), "stdin omits stdin_closed");
    assert!(stdin.contains(&413), "stdin omits the write cap");

    // 409 twice on ack, for two different reasons, both of which would otherwise
    // read as "the command produced no output".
    let ack = codes_for("POST", "/v1/exec/{id}/ack");
    assert!(ack.contains(&409), "ack omits its conflict cases");

    // 404 on fs read means genuinely absent — the one place the mapping is right.
    let read = codes_for("GET", "/v1/fs/file");
    assert!(
        read.contains(&404),
        "fs read omits the genuinely-absent case"
    );
    assert!(read.contains(&400), "fs read omits the protocol-error case");
}

/// The three SSE events are typed, and the typing is the reason the stream is SSE
/// rather than a chunked byte body. A consumer that does not know `exit` exists
/// cannot tell a finished command from a dropped connection.
#[test]
fn the_stream_route_documents_all_three_typed_events() {
    let document = schema::document(&Config::default(), &routes::surface_docs());
    let stream = document["routes"]
        .as_array()
        .expect("routes")
        .iter()
        .find(|route| route["path"] == "/v1/exec/{id}/stream")
        .expect("the stream route is documented");

    let names: Vec<&str> = stream["sse_events"]
        .as_array()
        .expect("sse_events is an array")
        .iter()
        .map(|event| event["event"].as_str().expect("an event name"))
        .collect();
    assert_eq!(names, ["output", "gap", "exit"]);
}

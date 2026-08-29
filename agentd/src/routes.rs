// SPDX-License-Identifier: Apache-2.0
//! Router assembly: lifecycle hooks, the control API, and health.

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;

use crate::state::{AppState, Bootstrap};
use crate::{auth, exec, fs, schema};

/// The hook and health wire types and the header constant, re-exported from their
/// original paths so every call site and doc reference here stays valid.
pub use protocol::VERSION_HEADER;
pub use protocol::health::{DiskHealth, Health};
pub use protocol::hook::{HOOK_PREFIX, RunHook, RunHookEnvelope, RunHookError};

/// Version reported by `/v1/health` and the `microvms-agentd-version` header.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn hook_path(hook: &str) -> String {
    format!("{HOOK_PREFIX}/{hook}")
}

/// Builds the full application.
///
/// Assembled by walking [`surface_docs`], the same list `/v1/schema` publishes.
/// That is the point: a route cannot be served unless it appears in the list, and
/// a listed route with no handler here panics at startup rather than serving an
/// undocumented surface. The alternative — two independent lists kept in step by
/// discipline — is how generated docs rot into being worse than none.
pub fn app(state: AppState) -> Router {
    let max_body = state.config().max_body_bytes;

    // Lifecycle hooks are unauthenticated because the platform has no token to
    // present. The paths are fixed by the service: the platform calls
    // `/aws/lambda-microvms/runtime/v1/<hook>`, so they are not under our own
    // `/v1` namespace and cannot be renamed.
    //
    // `ready` and `validate` are image-build hooks rather than instance hooks —
    // the build calls them to decide whether the snapshot it just produced is
    // usable. A daemon that omits them fails the build, not the run, which is a
    // confusing place to discover the omission.
    let mut open = Router::new();
    let mut control = Router::new();

    for endpoint in surface_docs() {
        let handler = handler_for(&endpoint);
        match endpoint.auth {
            schema::Auth::Bearer => control = control.route(&endpoint.path, handler),
            schema::Auth::Open | schema::Auth::PlatformHook => {
                open = open.route(&endpoint.path, handler);
            }
        }
    }

    // Every control route sits behind the token guard. `route_layer` applies the
    // middleware only to matched routes, so an unmatched path still falls through
    // to the 404 fallback instead of being answered 401 — telling an
    // unauthenticated caller which paths exist is not a secret worth keeping, but
    // answering 401 for a typo sends a client chasing credentials.
    let control = control
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ))
        // The extractor-level default (2 MiB) does not apply to bodies consumed
        // as a stream, so it is disabled and the wire-level layer below is the
        // real cap. Keeping both would silently truncate JSON control bodies at
        // 2 MiB while leaving tar uploads unbounded.
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(max_body));

    open.merge(control)
        .fallback(not_found)
        // Applied last, and outside `route_layer`, so it covers every response the
        // application can produce: handler bodies, the 401/503 the auth middleware
        // returns before a handler runs, the 413 the body-limit layer injects, and
        // the 404 fallback. A version header a client only sometimes receives is
        // one it cannot use as a precondition, which is the whole reason to send it.
        .layer(middleware::from_fn(stamp_version))
        // Outermost, so it wraps every other layer including the version stamp and
        // the auth middleware — a panic inside one of those is exactly as fatal as a
        // panic in a handler, and a layer inside them could not catch it.
        //
        // What this buys is narrow and worth stating precisely. Without it a
        // panicking handler drops the connection, and the client sees a transport
        // error it cannot distinguish from a dead VM. With it the client gets a 500
        // and the connection survives. It does *not* undo the panic: whatever the
        // handler was doing is abandoned, and any `std::sync::Mutex` it held is now
        // poisoned — which is why this pairs with the poison recovery in `state.rs`.
        // Either half alone leaves the VM wedged in a different way.
        //
        // Requires `panic = "unwind"`; see the workspace release profile, which sets
        // it back from `abort` for exactly this reason.
        .layer(CatchPanicLayer::new())
        .with_state(state)
}

/// Maps a documented endpoint onto the handler that serves it.
///
/// Exhaustive by construction: an unrecognised entry panics at startup, which a
/// test exercises. A silently-skipped route would be a documented path that
/// answers 404, and a client trusting the document would read that as "the
/// artifact is absent" rather than "this daemon does not serve it".
fn handler_for(endpoint: &schema::Endpoint) -> axum::routing::MethodRouter<AppState> {
    let hooks = HOOK_PREFIX;
    match (endpoint.method, endpoint.path.as_str()) {
        ("POST", path) if path == format!("{hooks}/ready") => post(ready_hook),
        ("POST", path) if path == format!("{hooks}/validate") => post(validate_hook),
        ("POST", path) if path == format!("{hooks}/run") => post(run_hook),
        ("POST", path) if path == format!("{hooks}/suspend") => post(suspend_hook),
        ("POST", path) if path == format!("{hooks}/resume") => post(resume_hook),
        ("POST", path) if path == format!("{hooks}/terminate") => post(terminate_hook),
        ("POST", "/v1/exec/start") => post(exec::start),
        ("GET", "/v1/exec/{id}") => get(exec::poll),
        // The streaming attach is registered as a sibling of the poll route rather
        // than replacing it: poll is what the conformance suite and every existing
        // client use, and streaming is an additional view onto the same server-side
        // object. Detaching from one does not affect the other.
        ("GET", "/v1/exec/{id}/stream") => get(exec::stream),
        ("POST", "/v1/exec/{id}/stdin") => post(exec::write_stdin),
        ("POST", "/v1/exec/{id}/ack") => post(exec::ack),
        ("POST", "/v1/exec/{id}/kill") => post(exec::kill),
        // Two entries share each fs path, one per method. axum merges method
        // routers registered against the same path, so GET and PUT arrive here
        // separately and end up on one route.
        ("GET", "/v1/fs/file") => get(fs::read_file),
        ("PUT", "/v1/fs/file") => axum::routing::put(fs::write_file),
        ("GET", "/v1/fs/tar") => get(fs::read_tar),
        ("PUT", "/v1/fs/tar") => axum::routing::put(fs::write_tar),
        ("GET", "/v1/tcp") => get(crate::tunnel::open),
        ("GET", "/v1/health") => get(health),
        ("GET", "/v1/schema") => get(schema_route),
        (method, path) => panic!("{method} {path} is documented but has no handler"),
    }
}

/// Stamps [`VERSION_HEADER`] onto every response.
///
/// `PROTOCOL.md` has promised this header since the first commit and nothing was
/// emitting it — the constant existed and was referenced only from a doc comment.
/// It is set rather than inserted-if-absent because nothing downstream sets it, and
/// a handler that did would be describing a different daemon than the one running.
async fn stamp_version(request: axum::extract::Request, next: middleware::Next) -> Response {
    let mut response = next.run(request).await;
    // `VERSION` comes from `CARGO_PKG_VERSION`, which cargo will not let contain a
    // character invalid in a header value, so this cannot fail. Handled rather
    // than unwrapped anyway: a panic here would take down every response, and a
    // missing header is a far better failure than a dead listener.
    match axum::http::HeaderValue::from_str(VERSION) {
        Ok(value) => {
            response.headers_mut().insert(
                axum::http::header::HeaderName::from_static(VERSION_HEADER),
                value,
            );
        }
        Err(err) => tracing::error!(%err, VERSION, "version is not a valid header value"),
    }
    response
}

/// One-shot token bootstrap, and the launch-environment channel beside it.
///
/// Deliberately unauthenticated: the platform has no credential to present, and
/// its request arrives over loopback indistinguishably from an in-VM process
/// (measured; see `docs/PLATFORM.md`). The defense is that this route can only
/// succeed once.
///
/// The payload may also carry `env`, a map applied as the base environment of every
/// subsequent exec. It rides here because this is the only per-VM channel the
/// platform offers, and it shares the token's 4096-byte budget. The token itself
/// never becomes part of that base environment — [`AppState::bootstrap`] takes the
/// two as separate arguments precisely so no code path can move one into the other.
async fn run_hook(
    State(state): State<AppState>,
    body: Result<Json<RunHookEnvelope>, JsonRejection>,
) -> Response {
    let Ok(Json(envelope)) = body else {
        // A malformed hook body is 400. It is never 404: a client that maps 404
        // onto "missing file" would report a phantom absent artifact for what is
        // really a protocol error.
        tracing::warn!("run hook body is not JSON");
        return StatusCode::BAD_REQUEST.into_response();
    };

    let Some(raw) = envelope.run_hook_payload else {
        tracing::warn!("run hook envelope carries no runHookPayload");
        return StatusCode::BAD_REQUEST.into_response();
    };

    // The token is never logged, and neither is the payload that carries it. The
    // refusal is safe to log for the same reason: `RunHookError` names a key or a
    // shape and never a value, which is why it is a typed error rather than serde's
    // own message.
    let hook = match RunHook::parse(&raw) {
        Ok(hook) => hook,
        Err(err) => {
            tracing::warn!(%err, "runHookPayload rejected");
            // The body names the problem too. This is the one route whose failure
            // is invisible from outside the VM — the platform terminates it with
            // "Run lifecycle hook returned HTTP status 400" before forwarding any
            // traffic — so a caller debugging a launch that died young has only
            // `stateReason` and the guest logs, and a bare 400 tells them nothing
            // about which key they got wrong.
            return (StatusCode::BAD_REQUEST, err.to_string()).into_response();
        }
    };

    // The tunnel's identity material, when the payload carries it. Read before the
    // bootstrap so a self-contradictory payload — one half of a mutual proof — is a
    // 400 rather than a VM that launches and then refuses every handshake. Absence
    // is not an error: a launch must never fail because a caller did not ask for a
    // feature, and this route's 400 makes the platform terminate the VM.
    let tunnel_identity = match crate::tunnel_identity::Material::from_payload(&hook) {
        Ok(material) => material.map(std::sync::Arc::new),
        Err(err) => {
            // Safe to log for the same reason `RunHookError` is: every variant names
            // a key or a shape, never the key material itself.
            tracing::warn!(%err, "tunnel identity material rejected");
            return (StatusCode::BAD_REQUEST, err.to_string()).into_response();
        }
    };

    let identity_delivered = tunnel_identity.is_some();
    match state.bootstrap_with_identity(hook.agent_token.as_bytes(), hook.env, tunnel_identity) {
        Bootstrap::Installed => {
            // The count and the keys, never the values: a launch env is where a
            // caller puts a credential, and the whole reason it travels in this
            // payload is that the payload is not logged. `tunnel_identity` is a
            // boolean for the same reason and one step further: there is no
            // rendering of a private key that belongs in a log line.
            tracing::info!(
                launch_env_vars = state.launch_env().len(),
                tunnel_identity = identity_delivered,
                "agent token installed"
            );
            StatusCode::OK.into_response()
        }
        Bootstrap::AlreadyIdentical => {
            // The platform may retry its own hook. Answering 409 here would fail
            // a launch that is fine.
            tracing::info!("identical bootstrap replay accepted");
            StatusCode::OK.into_response()
        }
        Bootstrap::Conflict => {
            tracing::warn!("bootstrap refused: a different token is already installed");
            StatusCode::CONFLICT.into_response()
        }
    }
}

/// Image-build readiness probe.
///
/// Called during the build, before any instance exists and therefore before any
/// token has been delivered. Answering 200 here is correct even though the
/// control API is closed: the question is whether the daemon started, not
/// whether it is bootstrapped. Gating this on bootstrap state would fail every
/// build.
async fn ready_hook() -> StatusCode {
    tracing::info!("image ready hook");
    StatusCode::OK
}

/// Image-build validation probe. Same reasoning as `ready`.
async fn validate_hook() -> StatusCode {
    tracing::info!("image validate hook");
    StatusCode::OK
}

async fn suspend_hook() -> StatusCode {
    tracing::info!("suspend hook");
    StatusCode::OK
}

/// Resume acknowledgement.
///
/// Suspend is a freeze and restore, not a stop and start: measured 2026-08-05 in
/// us-east-1, the in-memory agent token, the filesystem, exec records, and even
/// backgrounded processes all survive a suspend/resume cycle, and the endpoint URL
/// is unchanged. So the normal case here is a VM that is already bootstrapped and
/// needs nothing.
///
/// An earlier version of this docstring claimed the opposite — that in-memory
/// bootstrap state made a resumed VM unable to serve the control API. That was
/// inferred from where the state lives rather than measured, and it was wrong. See
/// `docs/PLATFORM.md`.
///
/// The one thing that does change across a suspend is the guest's view of time: it
/// observes the whole suspension as a single jump, so any timeout, lease, or
/// session held by a running command expires at once on resume.
async fn resume_hook(State(state): State<AppState>) -> StatusCode {
    if state.is_bootstrapped() {
        tracing::info!("resumed with bootstrap state intact");
    } else {
        // Unexpected given the measurement above, and worth saying loudly rather
        // than treating as routine: it would mean this resume behaved like a cold
        // start, and every in-flight exec record is gone with it.
        tracing::warn!(
            "resumed WITHOUT an installed token — this contradicts the measured \
             suspend/resume behavior; the control API stays closed until a fresh \
             run hook arrives"
        );
    }
    StatusCode::OK
}

async fn terminate_hook() -> StatusCode {
    tracing::info!("terminate hook");
    StatusCode::OK
}

/// `GET /v1/health` — liveness, and the exec-activity signal an orchestrator polls.
///
/// `busy` is here rather than on a route the workload calls itself, and the reason is
/// measured. The platform measures idleness by inbound traffic through the endpoint
/// proxy; that proxy terminates *outside* the guest and forwards over loopback
/// (`docs/PLATFORM.md`, "The platform's own hook arrives over loopback"), so a
/// request an in-VM process sends to this port never reaches the thing counting
/// traffic. An in-guest keepalive route would therefore keep nothing alive, and would
/// be discovered as broken by a multi-hour run auto-suspending mid-work.
///
/// So the honest shape is the other way round: an orchestrator outside the VM polls
/// this endpoint, which *is* real inbound traffic, and reads `busy` to decide whether
/// to keep polling. The assertion of liveness is repeated and is the caller's, which
/// is what the daemon self-keepaliving would not be — a hung process would then bill
/// to the 8-hour `maximumDurationInSeconds` ceiling with nobody having asked for it.
/// Nothing here is opt-in because nothing here perpetuates anything: two fields on a
/// response the route already returned.
async fn health(State(state): State<AppState>) -> Json<Health> {
    let identity = state.identity_report();
    let (busy, execs) = exec::activity(&state).await;
    Json(Health {
        // `Cow` on the wire type so a client deserializes into an owned string; the
        // daemon's own version is a `&'static str` and stays borrowed.
        version: std::borrow::Cow::Borrowed(VERSION),
        bootstrapped: state.is_bootstrapped(),
        // Measured against the daemon's own working directory, which is the image
        // WORKDIR and the filesystem a caller's writes land on by default. A write
        // to some other mount is checked against *that* mount at write time; this
        // is the fleet-wide number worth graphing, not a promise about every path.
        disk: state
            .disk_guard()
            .read(&std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")))
            .ok()
            .map(|reading| DiskHealth {
                available_bytes: reading.available,
                reserve_bytes: reading.reserve,
                under_pressure: reading.under_pressure(),
            }),
        identity_degraded: identity.degraded(),
        identity_repaired: identity.attempted,
        busy,
        execs,
    })
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// `GET /v1/schema` — the machine-readable wire contract.
///
/// Unauthenticated, like `/v1/health`, and the reason is not convenience. A client
/// needs the contract *before* it holds a token: the bootstrap token arrives at the
/// platform's `/run` hook, so between VM launch and bootstrap there is a window in
/// which the only thing a caller can do is ask what this daemon speaks. Gating the
/// document behind the token would make version negotiation impossible during
/// exactly the window it matters, and would answer 503 — which a client reads as
/// "the daemon is broken" rather than "not yet bootstrapped".
///
/// Nothing here is secret. Every path, shape, and status code is in the published
/// repository, and the limits are the operator's own configuration rather than
/// anything about the workload. The one thing that would be sensitive — whether a
/// token is installed — is deliberately *not* here; that lives on `/v1/health`,
/// which is equally open and already says so.
async fn schema_route(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(schema::document(state.config(), &surface_docs()))
}

/// The complete route surface, as both the router and `/v1/schema` see it.
///
/// One list, walked twice. Built at call time rather than held in a `static`
/// because the hook paths are formatted from [`HOOK_PREFIX`] and the status tables
/// are already `const` — the cost is a few dozen allocations on a path taken once
/// at startup and once per schema request.
pub fn surface_docs() -> Vec<schema::Endpoint> {
    use schema::{Auth, json_body, no_body, octet_stream, query, sse_stream, tar_stream};

    /// Keeps each row below to the fields that differ between routes.
    fn row(
        method: &'static str,
        path: String,
        auth: Auth,
        summary: &'static str,
        statuses: &'static [schema::Status],
    ) -> schema::Endpoint {
        schema::Endpoint {
            method,
            path,
            auth,
            summary,
            query: None,
            request: no_body(),
            response: no_body(),
            statuses,
            sse_events: &[],
        }
    }

    let hook = |name: &str| hook_path(name);

    vec![
        // The lifecycle hooks. A consumer must never call these: they are the
        // platform's, the prefix is fixed by the service, and `/run` in particular
        // is the one route whose success is not repeatable.
        row(
            "POST",
            hook("ready"),
            Auth::PlatformHook,
            "image-build readiness probe; answers 200 even before bootstrap, \
             because the question is whether the daemon started",
            schema::HOOK_ACK,
        ),
        row(
            "POST",
            hook("validate"),
            Auth::PlatformHook,
            "image-build validation probe; same reasoning as ready",
            schema::HOOK_ACK,
        ),
        schema::Endpoint {
            request: json_body::<RunHookEnvelope>(),
            ..row(
                "POST",
                hook("run"),
                Auth::PlatformHook,
                "one-shot token bootstrap, plus the optional launch environment. \
                 The platform wraps the caller's string, so the payload's own \
                 fields are one JSON parse deeper than the request body: \
                 {\"runHookPayload\": \"{\\\"agent_token\\\": \\\"...\\\", \
                 \\\"env\\\": {\\\"KEY\\\": \\\"VALUE\\\"}}\"}. `agent_token` is \
                 required and non-empty; `env` is optional and becomes the BASE \
                 environment of every later exec, with each request's own `env` \
                 overlaid on top of it. Values must be strings. Unknown keys are \
                 ignored, so a newer client can still bootstrap this daemon. The \
                 whole payload is capped by the platform at 4096 bytes inclusive, \
                 measured in UTF-8 bytes — the token and the env share that \
                 budget. `env` is installed only by the first successful \
                 bootstrap: a replay cannot edit it and a conflicting token \
                 cannot either.",
                schema::HOOK_RUN,
            )
        },
        row(
            "POST",
            hook("suspend"),
            Auth::PlatformHook,
            "acknowledged and logged",
            schema::HOOK_ACK,
        ),
        row(
            "POST",
            hook("resume"),
            Auth::PlatformHook,
            "acknowledged. Measured: the token, filesystem, exec records, and even \
             backgrounded processes survive a suspend/resume cycle. What does not \
             survive is the guest's view of time, which jumps — so any timeout or \
             lease held by a running command expires at once on resume.",
            schema::HOOK_ACK,
        ),
        row(
            "POST",
            hook("terminate"),
            Auth::PlatformHook,
            "acknowledged; begins graceful shutdown with in-flight requests draining",
            schema::HOOK_ACK,
        ),
        schema::Endpoint {
            request: json_body::<exec::StartRequest>(),
            response: json_body::<exec::StartResponse>(),
            ..row(
                "POST",
                "/v1/exec/start".into(),
                Auth::Bearer,
                "start a command under a caller-minted exec_id. Idempotent on that \
                 id: a retry returns success without spawning a second child.",
                schema::EXEC_START,
            )
        },
        schema::Endpoint {
            response: json_body::<exec::PollResponse>(),
            ..row(
                "GET",
                "/v1/exec/{id}".into(),
                Auth::Bearer,
                "poll status and output. Read-only: polling never mutates the entry, \
                 and output survives until an explicit ack.",
                schema::EXEC_POLL,
            )
        },
        schema::Endpoint {
            query: Some(query::<exec::StreamQuery>()),
            response: sse_stream(
                "resume with ?offset=N to receive exactly the bytes after N. The \
                 next offset to resume from is a chunk's offset plus the length of \
                 its decoded bytes. A body that ends without an exit event means \
                 the connection failed, not the command — that distinction is the \
                 reason this is SSE and not a chunked byte stream.",
            ),
            sse_events: SSE_EVENTS,
            ..row(
                "GET",
                "/v1/exec/{id}/stream".into(),
                Auth::Bearer,
                "follow output as Server-Sent Events from a byte offset",
                schema::EXEC_STREAM,
            )
        },
        schema::Endpoint {
            request: json_body::<exec::StdinRequest>(),
            response: json_body::<exec::StdinResponse>(),
            ..row(
                "POST",
                "/v1/exec/{id}/stdin".into(),
                Auth::Bearer,
                "write to a child's stdin, or signal EOF. A separate request from \
                 the output stream on purpose: a dropped attach must not cost the \
                 ability to feed the process. EOF is explicit rather than inferred, \
                 because a child reading stdin cannot exit until the daemon drops \
                 its own handle.",
                schema::EXEC_STDIN,
            )
        },
        schema::Endpoint {
            response: json_body::<exec::PollResponse>(),
            ..row(
                "POST",
                "/v1/exec/{id}/ack".into(),
                Auth::Bearer,
                "release output and enter TTL collection. Only acked entries are \
                 ever collected, so output nobody read is never destroyed.",
                schema::EXEC_ACK,
            )
        },
        schema::Endpoint {
            response: json_body::<exec::KillResponse>(),
            ..row(
                "POST",
                "/v1/exec/{id}/kill".into(),
                Auth::Bearer,
                "SIGTERM then SIGKILL to the whole process group, not just the \
                 direct child — a shell that backgrounded a server leaves the \
                 interesting process outside the child pid",
                schema::EXEC_KILL,
            )
        },
        schema::Endpoint {
            query: Some(query::<crate::tunnel::TunnelQuery>()),
            response: no_body(),
            ..row(
                "GET",
                "/v1/tcp".into(),
                Auth::Bearer,
                "upgrade to a WebSocket relayed to 127.0.0.1:<port> in the guest, so a \
                 caller outside the VM can speak an arbitrary TCP protocol to a server \
                 inside it. Binary frames carry bytes in both directions and a frame is a \
                 byte range rather than a message — TCP has no message boundaries and \
                 neither does this. The dial is loopback-only and never resolves a name: a \
                 relay that could reach another host would be an open proxy inside the VM \
                 reachable with the agent token. One connection per WebSocket, deliberately: \
                 multiplexing would reimplement per-stream ids, flow control, and close \
                 handshakes that the platform already provides per connection. Outcomes \
                 arrive as close codes — 4502 nothing listening, 4400 port 0, 4500 a \
                 mid-relay failure — because a WebSocket route leaves HTTP behind after its \
                 101. Rests on a measured platform property: binary frames survive a \
                 port-scoped token byte-exact (docs/PLATFORM.md, 2026-08-29). With \
                 `identity=1` the relay first completes a Noise KK handshake against the \
                 per-VM key delivered in the launch payload, which proves the far end is \
                 this VM without trusting the endpoint proxy and makes every later frame \
                 ciphertext; 4401 means this VM was launched without a seed and 4403 means \
                 the handshake was refused. The handshake runs before the dial, so a refused \
                 caller never reaches a guest service.",
                schema::TUNNEL_OPEN,
            )
        },
        schema::Endpoint {
            query: Some(query::<fs::FileReadQuery>()),
            response: octet_stream("the file's bytes, streamed"),
            ..row(
                "GET",
                "/v1/fs/file".into(),
                Auth::Bearer,
                "read one file, or a 1-based inclusive line range of it. \
                 ?start_line=&end_line= are the AI SDK harness's readTextFile \
                 semantics: an end_line past the last line reads through EOF \
                 without error, and omitting both returns the whole file \
                 byte-identically. Still streamed — the range never buffers the \
                 file to slice it.",
                schema::FS_READ_FILE,
            )
        },
        schema::Endpoint {
            query: Some(query::<fs::FsQuery>()),
            request: octet_stream("the file's bytes, streamed to disk rather than buffered"),
            ..row(
                "PUT",
                "/v1/fs/file".into(),
                Auth::Bearer,
                "write one file. Deliberately not confined to a root: the same token \
                 authorizes exec, so a root prefix would add no security while \
                 breaking harnesses that write to home directories and /etc.",
                schema::FS_WRITE_FILE,
            )
        },
        schema::Endpoint {
            query: Some(query::<fs::FsQuery>()),
            response: tar_stream("uncompressed, streamed from a spool file"),
            ..row(
                "GET",
                "/v1/fs/tar".into(),
                Auth::Bearer,
                "download a tree as tar. Symlinks are packed as symlinks, which is \
                 the producing half of what extraction accepts.",
                schema::FS_READ_TAR,
            )
        },
        schema::Endpoint {
            query: Some(query::<fs::FsQuery>()),
            request: tar_stream("uncompressed; spooled to an unlinked temp file, never buffered"),
            ..row(
                "PUT",
                "/v1/fs/tar".into(),
                Auth::Bearer,
                "upload and extract a tar under ?path=, confined to that root — the \
                 one confined write path, because member paths come from the archive \
                 rather than from the caller. Mirrors the CPython tarfile `data` \
                 filter: in-tree symlinks preserved, absolute link targets refused, \
                 relative targets resolved lexically so a symlink written earlier in \
                 the same archive cannot redirect a later member.",
                schema::FS_WRITE_TAR,
            )
        },
        schema::Endpoint {
            response: json_body::<Health>(),
            ..row(
                "GET",
                "/v1/health".into(),
                Auth::Open,
                "liveness, daemon version, whether bootstrap has completed, and \
                 whether any exec is still running. `busy` exists so an \
                 orchestrator OUTSIDE the VM can hold it alive: the platform \
                 measures idleness by inbound traffic through the endpoint proxy, \
                 which terminates outside the guest, so a request from inside the \
                 guest cannot reset the idle timer. Polling this from outside is \
                 both the traffic and the decision.",
                schema::HEALTH,
            )
        },
        schema::Endpoint {
            response: octet_stream("this document"),
            ..row(
                "GET",
                "/v1/schema".into(),
                Auth::Open,
                "this document: every route, shape, status code, and operative limit",
                schema::SCHEMA,
            )
        },
    ]
}

/// The three typed events on `GET /v1/exec/{id}/stream`.
///
/// The typing is the point of using SSE at all. A raw chunked byte stream cannot
/// distinguish a finished command from a dropped connection — the bytes are
/// identical — so a client would have to guess, and guessing wrong on a build that
/// emits nothing for two minutes means reporting a failure that did not happen.
static SSE_EVENTS: &[schema::SseEvent] = &[
    schema::sse_event::<exec::OutputEvent>(
        "output",
        "a run of bytes at a known offset. `output` is base64 because output is \
         arbitrary bytes and a JSON string cannot carry non-UTF-8 — and because a \
         lossy decode would split a multi-byte character at a chunk boundary. \
         Resume from offset + len(decoded).",
    ),
    schema::sse_event::<exec::GapEvent>(
        "gap",
        "bytes in [from, to) are gone: the request resumed before the replay window, \
         or this subscriber fell behind the live channel. Reported rather than \
         hidden, because a client that cannot tell missing output from no output \
         will read a truncated log as a complete one.",
    ),
    schema::sse_event::<exec::ExitEvent>(
        "exit",
        "the terminal event, emitted before the body ends. A body that closes \
         without it means the connection failed, not the command. `offset` is the \
         total bytes published, so a client can assert it saw all of them.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// The platform's own body shape: the caller's payload as a JSON *string* inside
    /// `{"runHookPayload": ...}`. Built with `serde_json` rather than by formatting,
    /// so the inner escaping is the platform's and not a hand-rolled approximation —
    /// getting that wrong is what made a whole tier pass against a daemon the
    /// platform could not bootstrap.
    fn envelope(payload: serde_json::Value) -> RunHookEnvelope {
        RunHookEnvelope {
            run_hook_payload: Some(payload.to_string()),
        }
    }

    /// Posts a run-hook envelope through the handler and returns the status.
    async fn post_hook(state: &AppState, envelope: RunHookEnvelope) -> StatusCode {
        run_hook(State(state.clone()), Ok(Json(envelope)))
            .await
            .status()
    }

    /// A launch env in the payload installs, and the response is the same 200 a
    /// token-only bootstrap has always been.
    #[tokio::test]
    async fn a_run_hook_carrying_an_env_installs_both_halves() {
        let state = AppState::new(Config::default());
        let status = post_hook(
            &state,
            envelope(serde_json::json!({
                "agent_token": "tok",
                "env": {"FROM_LAUNCH": "yes", "EMPTY": ""},
            })),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(state.is_bootstrapped());
        let installed = state.launch_env();
        assert_eq!(
            installed.get("FROM_LAUNCH").map(String::as_str),
            Some("yes")
        );
        assert_eq!(installed.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(installed.len(), 2);
    }

    /// A payload with no `env` bootstraps exactly as it always did. This is the
    /// compatibility floor: every launch that predates the feature sends this.
    #[tokio::test]
    async fn a_run_hook_with_no_env_still_bootstraps_with_an_empty_one() {
        let state = AppState::new(Config::default());
        let status = post_hook(&state, envelope(serde_json::json!({"agent_token": "tok"}))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(state.is_bootstrapped());
        assert!(state.launch_env().is_empty());
    }

    /// An unknown key is ignored and the launch still succeeds. A 400 here makes the
    /// platform terminate the VM before forwarding any traffic, so refusing a field
    /// this daemon has never heard of would turn a newer client into a dead launch.
    #[tokio::test]
    async fn an_unknown_payload_key_does_not_fail_the_launch() {
        let state = AppState::new(Config::default());
        let status = post_hook(
            &state,
            envelope(serde_json::json!({
                "agent_token": "tok",
                "env": {"A": "1"},
                "some_future_field": {"nested": [1, 2, 3]},
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(state.launch_env().len(), 1);
    }

    /// Each malformed shape is a 400 that names the problem, and nothing is
    /// bootstrapped. The body is asserted on because this route's failure is
    /// invisible from outside the VM — the platform terminates it before forwarding
    /// traffic — so the guest log and the body are the only evidence.
    #[tokio::test]
    async fn a_malformed_env_is_a_400_that_names_the_problem_and_installs_nothing() {
        for (payload, expected) in [
            (
                serde_json::json!({"agent_token": "tok", "env": "A=1"}),
                "env",
            ),
            (
                serde_json::json!({"agent_token": "tok", "env": {"PORT": 8080}}),
                "PORT",
            ),
            (serde_json::json!({"agent_token": ""}), "empty"),
            (serde_json::json!({"env": {"A": "1"}}), "agent_token"),
        ] {
            let state = AppState::new(Config::default());
            let response =
                run_hook(State(state.clone()), Ok(Json(envelope(payload.clone())))).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{payload}");
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .expect("body");
            let detail = String::from_utf8(body.to_vec()).expect("utf-8");
            assert!(
                detail.contains(expected),
                "the refusal must name {expected}: {detail}"
            );
            assert!(
                !state.is_bootstrapped(),
                "a refused payload must install nothing: {payload}"
            );
            assert!(state.launch_env().is_empty());
        }
    }

    /// The refusal body never carries the token. The route's whole reason for a typed
    /// error rather than serde's message is that serde quotes the value it rejected,
    /// and the value next to a bad `env` is a credential.
    #[tokio::test]
    async fn a_refusal_body_does_not_echo_the_agent_token() {
        let state = AppState::new(Config::default());
        let response = run_hook(
            State(state),
            Ok(Json(envelope(serde_json::json!({
                "agent_token": "super-secret-agent-token",
                "env": {"PORT": 8080},
            })))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let detail = String::from_utf8(body.to_vec()).expect("utf-8");
        assert!(
            !detail.contains("super-secret-agent-token"),
            "the token reached a response body: {detail}"
        );
    }

    /// `busy` and `execs` are on the health response, and an idle daemon reports
    /// false and zero rather than omitting them.
    #[tokio::test]
    async fn health_reports_an_idle_daemon_as_not_busy_with_no_execs() {
        let state = AppState::new(Config::default());
        let Json(report) = health(State(state)).await;
        assert!(!report.busy);
        assert_eq!(report.execs, 0);
    }
}

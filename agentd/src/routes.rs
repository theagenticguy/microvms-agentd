//! Router assembly: lifecycle hooks, the control API, and health.

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;

use crate::state::{AppState, Bootstrap};
use crate::{auth, exec, fs, schema};

/// Version reported by `/v1/health` and the `microvms-agentd-version` header.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The response header carrying [`VERSION`], on every response including errors.
///
/// Lowercase because HTTP/2 requires it and hyper normalizes anyway; naming it
/// once here keeps the layer, the schema document, and any test asserting on it
/// from drifting into three spellings of the same header.
pub const VERSION_HEADER: &str = "microvms-agentd-version";

/// Prefix the platform uses for every lifecycle hook. Fixed by the service.
pub const HOOK_PREFIX: &str = "/aws/lambda-microvms/runtime/v1";

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

/// The envelope the platform posts to the run hook.
///
/// The `runHookPayload` string given to `RunMicrovm` is not delivered as the
/// request body: the platform wraps it, so the body is
/// `{"runHookPayload": "<the caller's string>"}` and the caller's own JSON is one
/// `serde_json` parse deeper. Measured 2026-08-05 — a daemon that reads
/// `agent_token` from the top level answers 400, and the platform then terminates
/// the VM with "Run lifecycle hook returned HTTP status 400" before any traffic is
/// forwarded, so the mistake is invisible from the outside.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunHookEnvelope {
    #[serde(rename = "runHookPayload")]
    pub run_hook_payload: Option<String>,
}

/// The caller's own payload, carrying the per-VM secret.
///
/// Passing the token at launch is what keeps it out of the shared image snapshot.
/// It is safe because the platform forwards no external traffic until this hook
/// returns 200.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunHook {
    pub agent_token: String,
}

/// `GET /v1/health` response.
#[derive(Debug, JsonSchema, Serialize)]
pub struct Health {
    version: &'static str,
    bootstrapped: bool,
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
    disk: Option<DiskHealth>,
    /// Whether any startup identity repair step failed. True means the VM is serving
    /// with a value from the shared image still in place — a duplicate machine-id or
    /// boot_id — which is a security-relevant condition an operator may want to
    /// drain the VM over, but is never a reason for the daemon to refuse to serve.
    identity_degraded: bool,
    /// False when identity repair was switched off by config. Distinguished from a
    /// repair that ran and found nothing so a monitor can tell "opted out" from
    /// "nothing to do".
    identity_repaired: bool,
}

/// The disk half of [`Health`].
#[derive(Debug, JsonSchema, Serialize)]
pub struct DiskHealth {
    /// Bytes available to an unprivileged writer, from `statvfs` `f_bavail`.
    available_bytes: u64,
    /// Bytes that must stay free before a write is refused. Zero means the guard is
    /// disabled.
    reserve_bytes: u64,
    /// Whether a write would be refused right now. Precomputed rather than left to
    /// the client, so every consumer applies the same comparison the write path does.
    under_pressure: bool,
}

/// One-shot token bootstrap.
///
/// Deliberately unauthenticated: the platform has no credential to present, and
/// its request arrives over loopback indistinguishably from an in-VM process
/// (measured; see `docs/PLATFORM.md`). The defense is that this route can only
/// succeed once.
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

    // The token is never logged, and neither is the payload that carries it.
    let Ok(hook) = serde_json::from_str::<RunHook>(&raw) else {
        tracing::warn!("runHookPayload is not a JSON object with agent_token");
        return StatusCode::BAD_REQUEST.into_response();
    };

    if hook.agent_token.is_empty() {
        tracing::warn!("agent_token is empty");
        return StatusCode::BAD_REQUEST.into_response();
    }

    match state.bootstrap(hook.agent_token.as_bytes()) {
        Bootstrap::Installed => {
            tracing::info!("agent token installed");
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

async fn health(State(state): State<AppState>) -> Json<Health> {
    let identity = state.identity_report();
    Json(Health {
        version: VERSION,
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
                "one-shot token bootstrap. The platform wraps the caller's string, \
                 so agent_token is one JSON parse deeper than the request body: \
                 {\"runHookPayload\": \"{\\\"agent_token\\\": \\\"...\\\"}\"}.",
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
            query: Some(query::<fs::FsQuery>()),
            response: octet_stream("the file's bytes, streamed"),
            ..row(
                "GET",
                "/v1/fs/file".into(),
                Auth::Bearer,
                "read one file",
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
                "liveness, daemon version, and whether bootstrap has completed",
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

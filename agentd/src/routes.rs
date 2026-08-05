//! Router assembly: lifecycle hooks, the control API, and health.

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde::{Deserialize, Serialize};
use tower_http::limit::RequestBodyLimitLayer;

use crate::state::{AppState, Bootstrap};
use crate::{auth, exec, fs};

/// Version reported by `/v1/health` and the `microvms-agentd-version` header.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Prefix the platform uses for every lifecycle hook. Fixed by the service.
pub const HOOK_PREFIX: &str = "/aws/lambda-microvms/runtime/v1";

fn hook_path(hook: &str) -> String {
    format!("{HOOK_PREFIX}/{hook}")
}

/// Builds the full application.
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
    let hooks = Router::new()
        .route(&hook_path("ready"), post(ready_hook))
        .route(&hook_path("validate"), post(validate_hook))
        .route(&hook_path("run"), post(run_hook))
        .route(&hook_path("suspend"), post(suspend_hook))
        .route(&hook_path("resume"), post(resume_hook))
        .route(&hook_path("terminate"), post(terminate_hook));

    // Every control route sits behind the token guard. `route_layer` applies the
    // middleware only to matched routes, so an unmatched path still falls through
    // to the 404 fallback instead of being answered 401 — telling an
    // unauthenticated caller which paths exist is not a secret worth keeping, but
    // answering 401 for a typo sends a client chasing credentials.
    let control = Router::new()
        .route("/v1/exec/start", post(exec::start))
        .route("/v1/exec/{id}", get(exec::poll))
        .route("/v1/exec/{id}/ack", post(exec::ack))
        .route("/v1/exec/{id}/kill", post(exec::kill))
        .route("/v1/fs/file", get(fs::read_file).put(fs::write_file))
        .route("/v1/fs/tar", get(fs::read_tar).put(fs::write_tar))
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

    hooks
        .merge(control)
        .route("/v1/health", get(health))
        .fallback(not_found)
        .with_state(state)
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
#[derive(Debug, Deserialize)]
pub struct RunHookEnvelope {
    #[serde(rename = "runHookPayload")]
    pub run_hook_payload: Option<String>,
}

/// The caller's own payload, carrying the per-VM secret.
///
/// Passing the token at launch is what keeps it out of the shared image snapshot.
/// It is safe because the platform forwards no external traffic until this hook
/// returns 200.
#[derive(Debug, Deserialize)]
pub struct RunHook {
    pub agent_token: String,
}

#[derive(Debug, Serialize)]
struct Health {
    version: &'static str,
    bootstrapped: bool,
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
/// Bootstrap state is held in memory, so a resumed VM has no token and cannot
/// serve the control API. The hook still answers 200 because refusing it would
/// not restore the token, and the honest signal is in the log plus the
/// `bootstrapped: false` that `/v1/health` will report.
async fn resume_hook(State(state): State<AppState>) -> StatusCode {
    if !state.is_bootstrapped() {
        tracing::warn!(
            "resumed without an installed token: bootstrap state is in memory, so the \
             control API is unavailable until a fresh /run arrives"
        );
    }
    StatusCode::OK
}

async fn terminate_hook() -> StatusCode {
    tracing::info!("terminate hook");
    StatusCode::OK
}

async fn health(State(state): State<AppState>) -> Json<Health> {
    Json(Health {
        version: VERSION,
        bootstrapped: state.is_bootstrapped(),
    })
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

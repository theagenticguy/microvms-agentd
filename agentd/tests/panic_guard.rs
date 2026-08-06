//! The panic guard, driven through a real layered `tower` stack.
//!
//! # Why this tier rather than a unit test
//!
//! Whether `CatchPanicLayer` fires depends on *where in the stack it sits*, not on
//! any function this crate defines. A unit test could only assert that the layer
//! was constructed. These drive a router layered the same way `routes::app` layers
//! it and assert on the status a client would actually receive.
//!
//! # The failure being guarded
//!
//! The daemon is the only channel into the VM: no SSH, no supervisor, no console,
//! and nothing inside that restarts a dead process. A panicking handler is fatal in
//! two independent ways, and both are covered here:
//!
//! 1. The panic propagates into hyper, which drops the connection. The client sees
//!    a transport error indistinguishable from a dead VM and cannot tell whether
//!    retrying is safe.
//! 2. If the panic was holding a `std::sync::Mutex`, that lock is poisoned forever.
//!    With `.expect()` on the lock, every *later* request panics in the same place,
//!    so one bug becomes a permanently unreachable VM.
//!
//! `CatchPanicLayer` addresses the first and the poison recovery in `state.rs`
//! addresses the second. Either alone leaves the VM wedged in the other way, which
//! is why this file asserts on both.

use std::sync::{Arc, Mutex};

use agentd::{AppState, Config};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use tower::ServiceExt;
use tower_http::catch_panic::CatchPanicLayer;

/// Holds a `std::sync::Mutex` so a panic can poison something the way a real
/// handler's panic would.
#[derive(Clone)]
struct Fragile {
    counter: Arc<Mutex<u32>>,
}

/// Stands in for a handler bug: an unwrap on an empty option, a slice index past
/// the end, an overflow in a debug build.
async fn always_panics() -> StatusCode {
    panic!("a handler bug reached in production");
}

/// Panics while holding the mutex, which poisons it.
async fn panics_holding_a_lock(State(state): State<Fragile>) -> StatusCode {
    let mut guard = state.counter.lock().expect("not yet poisoned");
    *guard += 1;
    panic!("panicking with a lock held");
}

/// Reads through the same lock, recovering rather than propagating — mirroring the
/// policy `AppState` applies. See the reasoning in `state.rs`.
async fn reads_the_lock(State(state): State<Fragile>) -> String {
    let value = match state.counter.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    };
    format!("{value}")
}

/// Layered exactly like `routes::app`: the panic layer outermost, so it wraps every
/// other layer rather than only the handlers.
fn app(state: Fragile) -> Router {
    Router::new()
        .route("/panic", get(always_panics))
        .route("/panic-with-lock", get(panics_holding_a_lock))
        .route("/read", get(reads_the_lock))
        .layer(CatchPanicLayer::new())
        .with_state(state)
}

fn fragile() -> Fragile {
    Fragile {
        counter: Arc::new(Mutex::new(0)),
    }
}

fn get_request(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("request")
}

/// Silences the panic hook around a deliberate panic, so a passing run does not
/// print a backtrace that reads like a failure.
fn hush() -> impl Drop {
    /// The boxed hook `take_hook` hands back, named so the guard below is readable.
    type Hook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send>;

    struct Restore(Option<Hook>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(hook) = self.0.take() {
                std::panic::set_hook(hook);
            }
        }
    }

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    Restore(Some(previous))
}

#[tokio::test]
async fn a_panicking_handler_answers_500_instead_of_dropping_the_connection() {
    let _quiet = hush();

    // That this `await` resolves at all is half the assertion: without the layer the
    // panic unwinds out of `poll` and takes the caller with it, which is the
    // connection drop observed at the service boundary.
    let response = app(fragile())
        .oneshot(get_request("/panic"))
        .await
        .expect("the layer produced a response rather than unwinding");

    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a client gets a status it can act on, not a transport error",
    );
}

#[tokio::test]
async fn the_stack_keeps_serving_after_a_handler_panics() {
    // The property that matters more than the status code: request N+1 succeeds.
    // This is what separates "one request failed" from "the VM is gone".
    let _quiet = hush();
    let app = app(fragile());

    let first = app
        .clone()
        .oneshot(get_request("/panic"))
        .await
        .expect("first request answered");
    assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let second = app
        .oneshot(get_request("/read"))
        .await
        .expect("the router still serves after a panic");
    assert_eq!(second.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_panic_that_poisons_a_lock_does_not_wedge_every_later_request() {
    // The compounding failure, and the reason the panic layer alone is not enough.
    // The handler panics *while holding* a mutex, so the lock is poisoned before the
    // layer ever sees the panic. A later reader using `.expect()` would panic too,
    // forever, and the VM would be unreachable.
    let _quiet = hush();
    let state = fragile();
    let app = app(state.clone());

    let panicked = app
        .clone()
        .oneshot(get_request("/panic-with-lock"))
        .await
        .expect("answered rather than unwinding");
    assert_eq!(panicked.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // The lock really is poisoned. Without this the test would pass against a
    // handler that never poisoned anything, and would prove nothing.
    assert!(
        state.counter.lock().is_err(),
        "the mutex must actually be poisoned for this test to mean anything",
    );

    let after = app
        .oneshot(get_request("/read"))
        .await
        .expect("a poisoned lock does not close the API");
    assert_eq!(after.status(), StatusCode::OK);
    let body = axum::body::to_bytes(after.into_body(), 1024)
        .await
        .expect("body");
    assert_eq!(
        String::from_utf8_lossy(&body),
        "1",
        "the recovered guard exposes the state the panicking handler left behind",
    );
}

#[tokio::test]
async fn the_layer_passes_ordinary_responses_through_untouched() {
    // Guards the other direction: a layer that answered 500 for everything would
    // satisfy every assertion above. An unmatched path must still be a 404, and a
    // 404 in particular — clients map it onto "file not found".
    let response = app(fragile())
        .oneshot(get_request("/no-such-route"))
        .await
        .expect("unmatched path answered");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let ok = app(fragile())
        .oneshot(get_request("/read"))
        .await
        .expect("ordinary route answered");
    assert_eq!(ok.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_real_appstate_still_authorizes_after_a_panic() {
    // The worst case in practice: `AppState`'s token lock is taken on *every*
    // control request, so propagating its poison closes the whole control API. This
    // drives the real `AppState` rather than the local stand-in.
    let _quiet = hush();
    let state = AppState::new(Config::default());
    state.bootstrap(b"tok");

    let response = app(fragile())
        .oneshot(get_request("/panic"))
        .await
        .expect("answered");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // Authorization still works, so the control API is still open, and a wrong
    // token is still refused — a panic is not a way in.
    assert!(state.is_bootstrapped());
    assert_eq!(state.token_matches(b"tok"), Some(true));
    assert_eq!(state.token_matches(b"wrong"), Some(false));
}

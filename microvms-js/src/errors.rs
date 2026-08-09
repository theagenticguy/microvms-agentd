// SPDX-License-Identifier: Apache-2.0
//! One conversion from a core [`Error`] out to JS, and the `ERR_*` code contract.
//!
//! # What a JS caller branches on
//!
//! The core's `ERR_*` code — byte-identical to the Python binding's `.code`, to `cli.py`'s
//! JSON envelope, and to the Rust `Error::code()`. Nobody parses a message, and there is no
//! fourth spelling of the taxonomy.
//!
//! # Where the code lands, and why it is in two places rather than one
//!
//! This is the one place napi's model does not let a binding be as clean as its Python twin,
//! and the reason is worth stating exactly because it decides the contract.
//!
//! A `napi::Error<S>` becomes a JS `Error` whose `.code` is `S` rendered as a string.
//! `S = String` gives a real `ERR_*` code, and a **synchronous** `#[napi]` function can
//! return one: the generated code calls `JsError::from(err).into_value(env)`, which is
//! generic over `S`. An **async** one cannot. `execute_tokio_future_with_finalize_callback`
//! is typed `Future<Output = Result<Data, impl Into<Error>>>`, and that bare `Error` is
//! `Error<Status>` — napi's own closed enum of napi conditions. So any code string is
//! collapsed to a `Status` on the way through a Promise rejection.
//!
//! Measured rather than reasoned about, with a probe addon built through the real CLI:
//!
//! ```text
//! SYNC   code="ERR_INVALID_ARG"  message="sync message"
//! ASYNC  code="GenericFailure"   message="async message"
//! ```
//!
//! Nearly every method on this surface is async, because the core is. So the contract cannot
//! be "read `.code`" — that would be true on `Duration.measured()` and false on
//! `sandbox.resume()`, which is the worst possible split: it works in the first test someone
//! writes and fails in production.
//!
//! **The rule is `err.cause.message`.** It is the `ERR_*` code and nothing else, on every
//! path, sync and async alike. `.code` carries the same string on the sync paths as a free
//! bonus, and a napi status on the async ones — so it is documented as *not* the thing to
//! branch on.
//!
//! # The daemon-status class is one level deeper
//!
//! The distinction between a 400 and a 404, which the conformance oracle asserts on, is the
//! cause's own cause: `err.cause.cause.message` is the `WireKind` name. Absent entirely for
//! a local reject, because nothing reached the daemon and inventing a status would be a claim
//! nobody made.
//!
//! One string per level, each level's message being exactly that string — so reading it is a
//! field access rather than parsing.
//!
//! # No validation lives here
//!
//! Every error crossing this boundary originated in `microvms-core`. This file translates; it
//! never decides (BIND-2).
//!
//! # Why every signature spells `napi::Result<..>` rather than an alias
//!
//! `napi-derive` reads the return type **syntactically**: it looks for a last path segment
//! literally named `Result` and takes its first generic argument as the JS value. A type
//! alias is invisible to that, so `JsResult<Duration>` was treated as the returned class and
//! produced `the trait bound Result<Duration, napi::Error<String>>: ObjectFinalize is not
//! satisfied` at every signature that used it. Measured, not guessed. Each signature says the
//! type out loud.

use microvms_core::Error;

/// The chain that carries the code, and the wire kind under it.
///
/// Built once here so the sync and async paths differ only in the outer error's status — the
/// one thing napi forces them to differ in.
fn code_chain(error: &Error) -> napi::Error {
    // The cause's message is *exactly* the code, so reading it is a field access.
    let mut cause = napi::Error::new(napi::Status::GenericFailure, error.code());
    if let Some(wire) = error.wire_kind() {
        cause.set_cause(napi::Error::new(
            napi::Status::GenericFailure,
            wire.as_str(),
        ));
    }
    cause
}

/// A core error for a **synchronous** `#[napi]` function.
///
/// `Error<String>` so `.code` really is the `ERR_*` string on this path. The cause chain is
/// attached regardless, so the uniform rule holds here too.
pub(crate) fn to_napi(error: &Error) -> napi::Error<String> {
    let mut mapped = napi::Error::new(error.code().to_string(), error.to_string());
    mapped.set_cause(code_chain(error));
    mapped
}

/// A core error on its way out of a synchronous function, for `.map_err(js)`.
///
/// `From<Error> for napi::Error` is impossible — both types are foreign to this crate — so
/// the conversion is a named function applied at each boundary. One visible call per method
/// reads as the translation it is.
pub(crate) fn js(error: Error) -> napi::Error<String> {
    to_napi(&error)
}

/// A core error on its way out of an **async** `#[napi]` function.
///
/// A local newtype, because that is the only way a foreign error type can reach napi's async
/// path: the macro wants `impl Into<napi::Error>`, and neither `microvms_core::Error` nor
/// `napi::Error` is ours to write that impl between. The `From` below is where the code is
/// necessarily demoted from `.code` into the cause — see the module docs for the measurement
/// that forced it.
///
/// `pub` rather than `pub(crate)` because it appears in the return type of every `pub async
/// fn` on this surface, and Rust rightly warns about a private type reachable at `pub`
/// visibility. It exports nothing to JS — napi reads only the `Ok` side of the `Result` — so
/// the visibility is a Rust-side formality, not a widening of the JS surface.
pub struct AsyncError(Error);

impl From<Error> for AsyncError {
    fn from(error: Error) -> Self {
        AsyncError(error)
    }
}

impl From<AsyncError> for napi::Error {
    fn from(error: AsyncError) -> napi::Error {
        let AsyncError(inner) = error;
        // `GenericFailure` rather than a guessed napi condition: mapping `ERR_INVALID_ARG` to
        // `Status::InvalidArg` would look tidier and would be a lie, because napi's
        // `InvalidArg` is about a napi argument conversion and not about this library's
        // taxonomy. A caller reads the cause.
        let mut mapped = napi::Error::new(napi::Status::GenericFailure, inner.to_string());
        mapped.set_cause(code_chain(&inner));
        mapped
    }
}

/// A core error on its way out of an async function, for `.map_err(js_async)`.
pub(crate) fn js_async(error: Error) -> AsyncError {
    AsyncError(error)
}

/// Every `ERR_*` code this library can raise, for a caller building an exhaustive switch.
///
/// Enumerated from the core's own `ErrorKind::ALL` rather than transcribed, so a kind added
/// there appears here without an edit — a hand-written list would agree with a typo.
#[napi_derive::napi]
pub fn error_codes() -> Vec<String> {
    microvms_core::ErrorKind::ALL
        .iter()
        .map(|kind| kind.code().to_string())
        .collect()
}

/// Every daemon-status class, as `err.cause.cause.message` names them.
#[napi_derive::napi]
pub fn wire_kinds() -> Vec<String> {
    microvms_core::WireKind::ALL
        .iter()
        .map(|wire| wire.as_str().to_string())
        .collect()
}

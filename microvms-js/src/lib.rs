// SPDX-License-Identifier: Apache-2.0
//! Node bindings over `microvms-core`: a thin wrapper that cannot reopen a trap.
//!
//! # What this crate is, and what it deliberately is not
//!
//! A total, thin mapping. Every public core constructor gets one binding constructor, every
//! accessor one getter, and **no arithmetic or coercion surface the core does not have**. That
//! last clause is the whole design constraint: `microvms-core` spends most of its length
//! making mistakes *unavailable* rather than rejected, and a binding is exactly where that
//! gets given back for free.
//!
//! It is not a place where validation lives. There is no local range check, no state check, no
//! region check, no size check anywhere in these files. Every rejection a JS caller sees came
//! from the core, with the core's message naming the `docs/PLATFORM.md` finding that measured
//! the behaviour. Where a gap in the core would let a mistake through, the rule this crate was
//! built under is to note the gap and leave it — a guard added here is the copy every JS
//! caller hits and the copy nothing else tests.
//!
//! # `#[napi]` classes, never `#[napi(object)]`, for anything that carries a closure
//!
//! This is the single most important line in the crate. `#[napi(object)]` converts to and from
//! a plain JS object *by structure*, so `{ seconds: 3600 }` would satisfy a
//! `RunHookTimeout` parameter and `{ amount: 1.5 }` an `EstimatedUsd` — which is precisely
//! the coercion those types exist to prevent. A `#[napi]` class is nominal: napi v3 emits a
//! real TypeScript class, `tsc` rejects an object literal, and at runtime the argument
//! conversion rejects a non-instance *before* any Rust runs.
//!
//! `#[napi(object)]` appears only where the shape carries no closure and is either a pure
//! result ([`exec::ExecResult`], [`session::Health`], [`sandbox::TeardownReport`]) or an
//! options bag. Two flavours of bag, and the difference is forced rather than chosen:
//! [`cost::RunUsageOptions`] keeps its guarded values as `ClassInstance` **fields**, which
//! works because `runReport` is synchronous; [`sandbox::BuildImageOptions`] cannot, because
//! `ClassInstance` holds raw napi pointers and is not `Send` while napi's async path requires
//! `Future: Send` — so `buildImage` takes its `SizeClass` and its two hook timeouts as
//! separate reference parameters. Either way the guarded types stay classes.
//!
//! # The four closures a binding could have given away, and what stops each
//!
//! * **A dollar amount as a number.** JS coerces far more eagerly than Python, so this needed
//!   the most care: [`cost::EstimatedUsd`] has no `valueOf`, no `toJSON`, no
//!   `Symbol.toPrimitive`, and no `add`. `Number(usd)` is `NaN`, `usd * 2` is `NaN`,
//!   `JSON.stringify(usd)` is `{}` — the figure comes out only through `.amount`, a string.
//! * **An unlabelled duration.** [`cost::Duration`] has no `#[napi(constructor)]` —
//!   `Duration.measured(s)` and `Duration.projected(s)` are the only doors, so
//!   `new Duration(3600)` throws rather than producing an unlabelled span.
//! * **A region that does not carry MicroVMs.** [`region::Region`] has factory methods for the
//!   five names, a `parse` that refuses everything else, and an `unlisted` that says at the
//!   call site that someone opted into the null-message trap (TRAP-6). No method takes a
//!   region string.
//! * **Two hook timeouts whose ceilings are 60x apart, transposed.**
//!   [`hooks::RunHookTimeout`] and [`hooks::BuildHookTimeout`] are separate classes, so
//!   passing one where the other belongs is rejected by napi's conversion.
//!
//! And the one that needed no work: there is **no `clientToken` field** anywhere, because
//! there is no such field on the core's request types. TRAP-1 is closed by absence at both
//! levels.
//!
//! # Async maps straight through
//!
//! No `block_on` bridge and no GIL, unlike the Python side. An exported `#[napi] pub async fn`
//! runs on napi's managed tokio runtime and returns a `Promise`; an `Err` rejects it.
//!
//! **Branch on `err.cause.message`, not on `err.code`.** That is the one place napi's model
//! forces this binding to differ from its Python twin, and [`errors`] records the measurement
//! that decided it: napi's async rejection path is typed over its own closed `Status` enum, so
//! a custom code string survives a synchronous throw and is collapsed on a Promise rejection.
//! The cause's message is the `ERR_*` code on every path.
//!
//! # Layout
//!
//! [`errors`] is the one conversion out. [`region`], [`hooks`], and [`cost`] are the value
//! types. [`session`] and [`exec`] are the in-VM surface; [`process`] is the same exec seen as
//! two byte streams, for a consumer shaped like the AI SDK's `SandboxProcess`; [`sandbox`] is
//! the lifecycle.

// `pub` rather than private: the `#[napi]` macro registers each item at module-init time
// through a link-section constructor rather than through a Rust path, so with private modules
// every exported function is `dead_code` as far as rustc can see. Making the modules public is
// what lets `-D warnings` stay on without a blanket allow — and it exports nothing extra,
// because this crate is a `cdylib` with no Rust consumers.
pub mod cost;
pub mod errors;
pub mod exec;
pub mod hooks;
pub mod process;
pub mod region;
pub mod sandbox;
pub mod session;

use napi_derive::napi;

/// The core crate's version, for a `doctor` or `manifest` command to report.
///
/// The **core's** version and not this crate's: what a caller needs to know is which client
/// they are talking through, and a binding version that drifted from it would be a second
/// number nobody can act on.
#[napi]
pub fn core_version() -> String {
    microvms_core::VERSION.to_string()
}

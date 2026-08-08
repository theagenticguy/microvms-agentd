//! The MicroVMs client: the control plane, the in-VM daemon, the cost engine, and
//! every trap closure, in one library crate (ARCH-1).
//!
//! # What this crate is for
//!
//! `docs/PLATFORM.md` records seventeen measured findings about AWS Lambda MicroVMs,
//! fifteen of which a client can act on. Most of them are traps in the specific sense
//! that the platform's answer points away from the cause: an unsupported region
//! answers `AccessDeniedException` with a null message, a `clientToken` replay wedges
//! an image in `CREATING` for fifteen hours with no error at all, a
//! `minimumMemoryInMiB` of 512 produces a guest reporting 2 GB. Each finding cost a
//! measurement, and this crate is where that measurement is spent once so no caller
//! has to make it again.
//!
//! The Python client at `clients/python/` closes the same traps and stays in the tree
//! as the conformance oracle and the API reference. This crate is the port, and the
//! reason to port is that Rust can make several of those closures *unavailable*
//! rather than merely rejected — see the strength ladder below.
//!
//! # How strongly a trap is closed
//!
//! The spec ranks each closure, strongest first:
//!
//! * **S1, inexpressible** — the mistake cannot be written down. [`region::Region`] is
//!   an enum over the five regions that carry MicroVMs, so a typo'd region is a
//!   compile error rather than a runtime check; [`sizing::SizeClass`] is closed over
//!   the five documented baselines; [`hooks::RunHookTimeout`] and
//!   [`hooks::BuildHookTimeout`] are separate types with no conversion between them,
//!   so a 3600-second build timeout cannot reach a field that caps at 60.
//! * **S2, expressible but rejected** — the mistake can be written and the client
//!   refuses it locally, before any control-plane call, with an error naming the
//!   `docs/PLATFORM.md` finding. Weaker because the guard is code that can regress,
//!   but it costs seconds rather than a build cycle. Every boundary where a bare
//!   integer or string still has to be judged lands here:
//!   [`sizing::SizeClass::from_baseline_mib`], `Region::from_str`.
//! * **S3, correct by default and overridable** — weakest, because it protects the
//!   caller who does nothing and abandons the one who overrides. An S3 closure must
//!   say what the override costs: [`region::Region::unlisted`] says it costs you the
//!   diagnostic.
//!
//! # Every error message names its finding
//!
//! A local reject explains itself by naming the `docs/PLATFORM.md` section that
//! measured the behaviour, because the codes and the guards exist precisely so a
//! reader can go to the measurement rather than to a constraint. "region
//! 'eu-central-1' is invalid" sends someone to check their spelling; the message
//! [`region::Region`] actually raises sends them to the null-message finding.
//!
//! # A guard that cannot fail is worse than no guard
//!
//! Carried over from the Python era unchanged: every guard here has a falsification —
//! a specific plausible edit that must turn a specific test red. "Delete the feature
//! and the test fails" does not count. The clearest case is TRAP-13 in
//! [`sizing`]: every documented peak is exactly four times its baseline, so a test
//! against the shipped table cannot tell a table lookup from `baseline * 4`, and the
//! guard has to drive the lookup over a table where the pattern does not hold.
//!
//! # Layout
//!
//! [`error`], [`region`], [`sizing`], [`hooks`], and [`constants`] are the foundation
//! every other module builds on. [`cost`], [`control`], [`session`], and [`sandbox`]
//! are the product surface.

pub mod constants;
pub mod control;
pub mod cost;
pub mod error;
pub mod hooks;
pub mod region;
pub mod sandbox;
pub mod session;
pub mod sizing;

// Re-exported so consumers name wire types through this crate rather than
// depending on `protocol` directly — the CLI's thinness guard counts on that.
pub use protocol;

pub use error::{Error, ErrorKind, WireKind};
pub use hooks::{BuildHookTimeout, RunHookTimeout};
pub use region::Region;
pub use sizing::SizeClass;

/// The crate's own version, for a `doctor` or `manifest` command to report.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

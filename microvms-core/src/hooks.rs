//! The two hook-timeout ceilings, as two types that cannot be swapped.
//!
//! Two families of lifecycle hook with ceilings 60x apart:
//!
//! * `run`, `resume`, `suspend`, `terminate` — max **60 seconds**. These wait on a
//!   daemon that is already booted, so 60s is generous and a daemon slower than that
//!   fails the launch with no way to ask for more time.
//! * `ready`, `validate` — max **3600 seconds**. These are image-*build* hooks; a
//!   build hook waits on a Dockerfile.
//!
//! Both from the pinned service model (`MicrovmHooks*TimeoutInSecondsInteger` and
//! `MicrovmImageHooks*TimeoutInSecondsInteger`, API version 2025-09-09), checked
//! against it by the build gate (TRAP-12).
//!
//! # Why two types rather than one with a comment
//!
//! Because the trap is the confusion, not the number. A caller who picks one value
//! large enough for a build hook — 300, say — passes image validation and is rejected
//! on the run family, *after* the artifact upload, reported as a constraint on a
//! field they did not know had two different ceilings. The Python client closes this
//! by validating a dict of hooks against a table of families (`sandbox.py:271`
//! `require_hook_timeouts_in_range`); that is S2, and it can only fire once a whole
//! hook block exists.
//!
//! Here each ceiling is its own type, so a build timeout in a run-hook field is a
//! compile error. There is deliberately **no conversion between them** — no `From`,
//! no `as_run()`, no shared trait with a `secs()` method that a generic function
//! could use to launder one into the other. A conversion is the whole trap with an
//! extra step: `BuildHookTimeout::try_new(300)?` is valid, and if it could become a
//! `RunHookTimeout` the 60s ceiling would be enforced nowhere.
//!
//! The only thing both types offer is a way *out* to a plain integer, at the wire
//! boundary where the request body is built and the field name already says which
//! family it is.

use std::fmt;

use crate::constants::{
    MAX_IMAGE_HOOK_TIMEOUT_SEC, MAX_MICROVM_HOOK_TIMEOUT_SEC, MODEL_API_VERSION,
};
use crate::error::Error;

/// A timeout for the `run`, `resume`, `suspend`, or `terminate` hook: 1..=60 seconds.
///
/// See the module docs for why this cannot be built from a [`BuildHookTimeout`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunHookTimeout(u32);

/// A timeout for the `ready` or `validate` image-build hook: 1..=3600 seconds.
///
/// See the module docs for why this cannot be built from a [`RunHookTimeout`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BuildHookTimeout(u32);

impl RunHookTimeout {
    /// The service ceiling for the MicroVM hook family.
    pub const MAX_SECS: u32 = MAX_MICROVM_HOOK_TIMEOUT_SEC;

    /// A run-family timeout, or a local reject naming both ceilings.
    ///
    /// The error names the *other* family's ceiling too, because the caller who hits
    /// this is nearly always someone who picked a build-hook number: telling them 60
    /// is the limit answers a question they did not ask, and telling them the two
    /// families differ by 60x answers the one they did.
    pub fn try_new(secs: u32) -> Result<Self, Error> {
        if (1..=Self::MAX_SECS).contains(&secs) {
            return Ok(Self(secs));
        }
        Err(Error::invalid_arg(out_of_range_message(
            "microvmHooks",
            secs,
            Self::MAX_SECS,
            BuildHookTimeout::MAX_SECS,
        )))
    }

    /// The value to put on the wire. The only way out of the newtype.
    pub fn as_secs(self) -> u32 {
        self.0
    }
}

impl BuildHookTimeout {
    /// The service ceiling for the image-hook family.
    pub const MAX_SECS: u32 = MAX_IMAGE_HOOK_TIMEOUT_SEC;

    /// A build-family timeout, or a local reject naming both ceilings.
    pub fn try_new(secs: u32) -> Result<Self, Error> {
        if (1..=Self::MAX_SECS).contains(&secs) {
            return Ok(Self(secs));
        }
        Err(Error::invalid_arg(out_of_range_message(
            "microvmImageHooks",
            secs,
            Self::MAX_SECS,
            RunHookTimeout::MAX_SECS,
        )))
    }

    /// The value to put on the wire. The only way out of the newtype.
    pub fn as_secs(self) -> u32 {
        self.0
    }
}

impl fmt::Display for RunHookTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.0)
    }
}

impl fmt::Display for BuildHookTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.0)
    }
}

/// The refusal both constructors raise, naming the family, the ceiling, and the
/// other family's ceiling.
///
/// Shared so the two messages cannot drift apart, and worded from
/// `sandbox.py:271`'s so a reader who has seen one recognises the other.
fn out_of_range_message(block: &str, secs: u32, ceiling: u32, other: u32) -> String {
    format!(
        "{block} timeout of {secs}s is outside the accepted range 1..{ceiling} (service model \
         {MODEL_API_VERSION}). The two hook families have ceilings 60x apart — {block} caps at \
         {ceiling}s while the other family caps at {other}s — because a build hook waits on a \
         Dockerfile and a run hook waits on a daemon that is already booted \
         (docs/PLATFORM.md, 'The `runHookPayload` ceiling is 4096 bytes, and the service model \
         states it')."
    )
}

/// A hook port: 1..=65535 (`HooksPortInteger`).
///
/// Here rather than in a caller because it is the same shape of constraint from the
/// same table, and because the daemon's port is a field a caller sets by hand.
/// Returned as a `u16` since that is what a port is; the guard exists for the two
/// values a `u16` still gets wrong — 0, and a figure that arrived as a wider integer.
pub fn require_hook_port(port: u32) -> Result<u16, Error> {
    if (1..=u32::from(u16::MAX)).contains(&port) {
        return Ok(port as u16);
    }
    Err(Error::invalid_arg(format!(
        "hooks.port={port} is outside 1..{} (service model {MODEL_API_VERSION}).",
        u16::MAX
    )))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::error::ErrorKind;

    /// The two ceilings, as literals. The 60x gap is the whole reason there are two
    /// types, so a change to either number is a change to the design.
    #[test]
    fn the_two_families_cap_sixty_seconds_apart_by_a_factor_of_sixty() {
        assert_eq!(RunHookTimeout::MAX_SECS, 60);
        assert_eq!(BuildHookTimeout::MAX_SECS, 3600);
        assert_eq!(
            BuildHookTimeout::MAX_SECS,
            RunHookTimeout::MAX_SECS * 60,
            "the 60x gap is the finding"
        );
    }

    /// The inclusive boundaries on both sides, for both families. `0` is refused
    /// because the model's minimum is 1, and a zero timeout is a hook that cannot
    /// succeed.
    #[test]
    fn each_family_accepts_its_ceiling_and_refuses_one_past_it() {
        assert_eq!(RunHookTimeout::try_new(1).expect("1s fits").as_secs(), 1);
        assert_eq!(RunHookTimeout::try_new(60).expect("60s fits").as_secs(), 60);
        assert!(RunHookTimeout::try_new(0).is_err());
        assert!(RunHookTimeout::try_new(61).is_err());

        assert_eq!(BuildHookTimeout::try_new(1).expect("1s fits").as_secs(), 1);
        assert_eq!(
            BuildHookTimeout::try_new(3600)
                .expect("3600s fits")
                .as_secs(),
            3600
        );
        assert!(BuildHookTimeout::try_new(0).is_err());
        assert!(BuildHookTimeout::try_new(3601).is_err());
    }

    /// The trap, at the value that triggers it. 300 is a plausible build-hook timeout
    /// and is legal for the image family; the run family refuses it, and the message
    /// has to explain *why the same number was fine elsewhere* or the caller reads it
    /// as arbitrary.
    #[test]
    fn a_build_sized_timeout_is_refused_by_the_run_family_naming_both_ceilings() {
        let build = BuildHookTimeout::try_new(300).expect("300s is legal for a build hook");
        assert_eq!(build.as_secs(), 300);

        let err = RunHookTimeout::try_new(300).expect_err("300s is not legal for a run hook");
        assert_eq!(err.kind(), ErrorKind::InvalidArg);
        let message = err.to_string();
        assert!(message.contains("microvmHooks"), "{message}");
        assert!(message.contains("1..60"), "{message}");
        assert!(
            message.contains("3600s"),
            "must name the other ceiling: {message}"
        );
        assert!(message.contains("60x apart"), "{message}");
        assert!(
            message.contains("2025-09-09"),
            "must name the model: {message}"
        );
    }

    /// The image family's refusal names the run family's ceiling, symmetrically. A
    /// one-directional message would leave the caller who over-shot 3600 without the
    /// fact that a smaller number is a different family's limit.
    #[test]
    fn the_build_family_refusal_names_the_run_ceiling() {
        let err = BuildHookTimeout::try_new(7200).expect_err("7200s is past the image ceiling");
        let message = err.to_string();
        assert!(message.contains("microvmImageHooks"), "{message}");
        assert!(message.contains("1..3600"), "{message}");
        assert!(
            message.contains("60s"),
            "must name the other ceiling: {message}"
        );
    }

    /// A port outside the model's range is refused; the boundaries are inclusive.
    #[test]
    fn a_hook_port_outside_the_model_range_is_refused() {
        assert_eq!(require_hook_port(1).expect("1 fits"), 1);
        assert_eq!(require_hook_port(9000).expect("9000 fits"), 9000);
        assert_eq!(require_hook_port(65535).expect("65535 fits"), 65535);
        for bad in [0, 65536, u32::MAX] {
            let err = require_hook_port(bad).expect_err("outside 1..65535");
            assert_eq!(err.kind(), ErrorKind::InvalidArg, "{bad}");
            assert!(err.to_string().contains("1..65535"), "{bad}");
        }
    }

    proptest! {
        /// The verdict over the whole domain, for both families at once: accepted iff
        /// in `1..=ceiling`, and a value legal for one family is refused by the other
        /// for every figure in the 61..=3600 band. That band is the trap's whole
        /// surface, and a property that only checked each type separately would not
        /// assert the two disagree.
        #[test]
        fn the_two_ceilings_disagree_across_the_whole_sixty_to_thirty_six_hundred_band(secs: u32) {
            let run = RunHookTimeout::try_new(secs);
            let build = BuildHookTimeout::try_new(secs);

            prop_assert_eq!(run.is_ok(), (1..=60).contains(&secs));
            prop_assert_eq!(build.is_ok(), (1..=3600).contains(&secs));

            if (61..=3600).contains(&secs) {
                prop_assert!(build.is_ok(), "{} is a legal build timeout", secs);
                let err = run.expect_err("but not a legal run timeout");
                prop_assert_eq!(err.kind(), ErrorKind::InvalidArg);
            }
            if let Ok(run) = RunHookTimeout::try_new(secs) {
                prop_assert_eq!(run.as_secs(), secs);
            }
            if let Ok(build) = BuildHookTimeout::try_new(secs) {
                prop_assert_eq!(build.as_secs(), secs);
            }
        }
    }
}

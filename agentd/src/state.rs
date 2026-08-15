// SPDX-License-Identifier: Apache-2.0
//! Bootstrap state and the exec registry.
//!
//! The one-shot bootstrap lives here. Its behavior is specified by the
//! `agentd-model` crate, which checks over every interleaving that a losing racer
//! never replaces the winner's token.
//!
//! # Why both locks recover from poisoning
//!
//! Every lock here is taken with [`recover`] rather than `.expect()`. The reasoning
//! is per-lock and the answer happens to be the same for both, but it is not the
//! same argument, and neither is "poisoning is unlikely".
//!
//! The stakes first. A poisoned `Mutex` stays poisoned forever, and `.expect()` on
//! it panics. So one panic anywhere under one of these locks makes *every
//! subsequent request* panic in the same place — the token check runs on the
//! authorization path, so a poisoned token lock closes the whole control API. The
//! daemon is the only channel into the VM: no SSH, no supervisor, no console.
//! Propagating the poison therefore converts a single handler bug into a
//! permanently unreachable VM with whatever work was in it lost. That is a strictly
//! worse outcome than any state this module can be left holding.
//!
//! What poisoning actually risks is a *torn invariant*: the guard was dropped
//! mid-update, so the data may be in a state no correct code path produces. That
//! has to be judged for each lock.
//!
//! **`token`** holds one `Option<Vec<u8>>` and every write is a single whole-value
//! assignment (`*slot = Some(...)`). There is no multi-step update, so there is no
//! intermediate state to be caught in: the value is either the old one or the new
//! one. A `Vec` cannot be observed half-assigned, because the panic would have to
//! occur inside `Vec`'s own move, and a move is not a user-visible sequence of
//! steps. Recovery is therefore sound in the strong sense — the invariant is
//! whole-value assignment and it cannot be broken. The security property also
//! survives: recovering hands back the same `Option`, so a poisoned lock cannot
//! *install* a token, and the one-shot check is re-run from whatever is there.
//! Poisoning is not a way to bypass bootstrap.
//!
//! **`launch_env`** is the same argument as `token`, one step weaker in the same
//! direction and stronger in another. Every write is a single whole-value
//! assignment of a `HashMap`, so there is no intermediate state to be caught in;
//! and unlike the token it carries nothing that authorizes anything, so a recovered
//! guard cannot hand back a credential. What it *can* be is stale or absent, which
//! means a child spawned afterwards runs without a variable the caller set. That is
//! visible in the child's own failure rather than silent, and it is a far better
//! outcome than a VM nothing can reach.
//!
//! **`execs`** is weaker and worth being explicit about, because it is a
//! `HashMap` mutated through arbitrary caller closures in [`AppState::with_execs`].
//! A panic inside one of those closures can leave the map missing an insert, or
//! holding an entry whose `acked_at` was set while a matching update elsewhere did
//! not happen. So recovery here is *not* "the data is definitely fine". It is
//! sound for a narrower reason: `std::collections::HashMap` is not left internally
//! corrupt by a panic in the caller's code — the panic unwinds out of the closure,
//! not out of the map's own rehash — so reads and writes afterwards are
//! memory-safe and well-defined. What can be wrong is *semantic*: one exec entry
//! may be inconsistent. The blast radius of that is one exec id, whose worst case
//! is a stale entry that TTL collection eventually reaps or a poll that reports the
//! wrong thing for one command. Set against a dead VM, one wrong exec record is the
//! right trade, and it is the trade this module makes deliberately rather than by
//! omission.
//!
//! The `Mutex`es inside `exec::Shared` are `tokio::sync::Mutex`, which has no
//! poisoning at all, so they need nothing here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::config::Config;
use crate::disk;
use crate::exec::ExecEntry;
use crate::identity;

/// Takes a lock, recovering the guard if a previous holder panicked.
///
/// See the module docs for why recovering beats propagating for both locks here.
/// The recovery is logged at `warn`: a poisoned lock means a handler panicked
/// somewhere, and that is a defect to go find even though the daemon kept serving.
fn recover<'a, T>(lock: &'a Mutex<T>, name: &str) -> MutexGuard<'a, T> {
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(
                lock = name,
                "recovering a poisoned lock: a previous holder panicked, so this \
                 state may be inconsistent. Serving on is deliberate — the daemon \
                 is the only channel into the VM and propagating the poison would \
                 make every later request panic too.",
            );
            poisoned.into_inner()
        }
    }
}

/// What happened when a caller tried to install a token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bootstrap {
    /// The token was installed. This caller was first.
    Installed,
    /// A token identical to the installed one was presented again. The platform
    /// may retry its own hook, and telling it the VM is broken would fail a
    /// launch that is actually fine, so this is success.
    AlreadyIdentical,
    /// A token different from the installed one was presented. Refused with 409;
    /// the installed token is unchanged.
    Conflict,
}

/// Shared daemon state. Cloning is cheap: everything is behind an `Arc`.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    /// `None` until bootstrap completes. Held as bytes because the token is
    /// compared in constant time on bytes: comparing `str` values raises on
    /// non-ASCII input in some runtimes, and any caller controls that header.
    token: Mutex<Option<Vec<u8>>>,
    /// The base environment for every exec, delivered in the run-hook payload
    /// alongside the token.
    ///
    /// Empty until a run hook carrying `env` arrives, and empty forever if none
    /// does — which is why it is a plain map rather than an `Option`. There is no
    /// behavior "no launch env was delivered" has that "an empty one was" does
    /// not, and collapsing them means no call site has to decide.
    ///
    /// Deliberately **not** the same slot as the token, and deliberately never
    /// holding it. The token's whole security property is that it stays out of
    /// child environments (`docs/TRUST.md`), and the way to keep that true while
    /// adding an environment channel is to have no code path that can put the
    /// token in this map: [`AppState::bootstrap`] takes the token as bytes and the
    /// env as a separate argument, so there is no struct field a future refactor
    /// could accidentally forward.
    launch_env: Mutex<HashMap<String, String>>,
    execs: Mutex<HashMap<String, ExecEntry>>,
    /// How free space is measured. Held here rather than called directly from the
    /// write paths so a test can inject a filesystem that is full, or one that
    /// fills mid-upload, without depending on the host's actual free space.
    space_probe: disk::SpaceProbe,
    /// What startup identity repair did. Immutable after construction: repair runs
    /// once, before serving.
    identity: identity::Report,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self::with_probe(config, disk::available_bytes, identity::Report::skipped())
    }

    /// Construction with the seams exposed, for `main` (which has a real repair
    /// report to hand in) and for tests (which inject a fake probe).
    pub fn with_probe(
        config: Config,
        space_probe: disk::SpaceProbe,
        identity: identity::Report,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                token: Mutex::new(None),
                launch_env: Mutex::new(HashMap::new()),
                execs: Mutex::new(HashMap::new()),
                space_probe,
                identity,
            }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// The disk guard, bound to the configured reserve. Returned by value so a
    /// handler can hold it across an await without borrowing shared state.
    pub fn disk_guard(&self) -> disk::Guard {
        disk::Guard {
            probe: self.inner.space_probe,
            reserve: self.inner.config.disk_reserve_bytes,
        }
    }

    pub fn identity_report(&self) -> &identity::Report {
        &self.inner.identity
    }

    /// Installs the agent token and the launch environment, once. Returns which of
    /// the three outcomes occurred so the route can map it to a status code.
    ///
    /// The two arrive together because they arrive in one payload, and they are
    /// separate parameters because they must never be one value: the token is bytes
    /// that authorize, the env is a map that reaches children, and the whole point
    /// of the split is that no code path can move a byte from the first into the
    /// second. See [`Inner::launch_env`].
    ///
    /// `launch_env` is installed only on [`Bootstrap::Installed`] — the first
    /// caller's. A replay of the identical token is the platform retrying its own
    /// hook, so its env is the same map; and a *different* env under the same token
    /// would be a second caller editing the first's environment without ever
    /// holding a different credential, which is precisely the one-shot property
    /// stated for the other half of the payload.
    pub fn bootstrap(&self, presented: &[u8], launch_env: HashMap<String, String>) -> Bootstrap {
        let mut slot = recover(&self.inner.token, "token");
        match slot.as_deref() {
            None => {
                *slot = Some(presented.to_vec());
                // Under the token lock, so a racer that loses the token cannot win
                // the environment: the winner's map is in place before any other
                // caller can observe a token installed.
                *recover(&self.inner.launch_env, "launch_env") = launch_env;
                Bootstrap::Installed
            }
            Some(installed) => {
                if crate::auth::constant_time_eq(installed, presented) {
                    Bootstrap::AlreadyIdentical
                } else {
                    Bootstrap::Conflict
                }
            }
        }
    }

    /// The launch environment, cloned for a caller about to build a child command.
    ///
    /// Cloned rather than handed out behind the guard because the caller merges it
    /// with the per-request map and then hands the result to `Command::envs`, and
    /// holding a `std::sync::Mutex` guard across that would put a non-`Send` value
    /// on an async path.
    ///
    /// Empty when no launch env was delivered, which is the same answer as an empty
    /// one having been delivered. Nothing branches on the difference.
    pub fn launch_env(&self) -> HashMap<String, String> {
        recover(&self.inner.launch_env, "launch_env").clone()
    }

    /// Whether bootstrap has completed. Control routes answer 503 until it has.
    pub fn is_bootstrapped(&self) -> bool {
        recover(&self.inner.token, "token").is_some()
    }

    /// Compares a presented token against the installed one in constant time.
    /// Returns `None` when no token is installed, which the caller must map to
    /// 503 rather than 401 — an unbootstrapped daemon is not the same as a bad
    /// credential, and the client acts differently on each.
    pub fn token_matches(&self, presented: &[u8]) -> Option<bool> {
        let slot = recover(&self.inner.token, "token");
        let installed = slot.as_deref()?;
        Some(crate::auth::constant_time_eq(installed, presented))
    }

    /// Runs a closure against the exec registry.
    ///
    /// A panic inside `f` poisons this lock. The next call recovers the guard
    /// rather than propagating, which is the narrower of the two soundness
    /// arguments in the module docs — read it before assuming the map is
    /// necessarily consistent.
    pub fn with_execs<T>(&self, f: impl FnOnce(&mut HashMap<String, ExecEntry>) -> T) -> T {
        let mut execs = recover(&self.inner.execs, "execs");
        f(&mut execs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::new(Config::default())
    }

    /// A launch env from pairs, so the tests below read as the payload does.
    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn first_bootstrap_installs() {
        let state = state();
        assert!(!state.is_bootstrapped());
        assert_eq!(
            state.bootstrap(b"tok-a", HashMap::new()),
            Bootstrap::Installed
        );
        assert!(state.is_bootstrapped());
    }

    #[test]
    fn identical_replay_succeeds_and_a_different_token_conflicts() {
        let state = state();
        state.bootstrap(b"tok-a", HashMap::new());

        // The platform retrying its own hook must not be told the VM is broken.
        assert_eq!(
            state.bootstrap(b"tok-a", HashMap::new()),
            Bootstrap::AlreadyIdentical
        );
        // A hijack attempt is refused and changes nothing.
        assert_eq!(
            state.bootstrap(b"tok-b", HashMap::new()),
            Bootstrap::Conflict
        );
        assert_eq!(state.token_matches(b"tok-a"), Some(true));
        assert_eq!(state.token_matches(b"tok-b"), Some(false));
    }

    /// The winner's launch env is installed and no later hook can edit it, whether
    /// it replays the same token or presents a different one. The second case is
    /// the interesting one: without this, a caller who cannot win the token could
    /// still rewrite the environment every subsequent child runs in.
    #[test]
    fn only_the_first_bootstrap_sets_the_launch_environment() {
        let state = state();
        assert!(state.launch_env().is_empty(), "empty before bootstrap");

        assert_eq!(
            state.bootstrap(b"tok-a", env(&[("WANTED", "yes")])),
            Bootstrap::Installed
        );
        assert_eq!(state.launch_env(), env(&[("WANTED", "yes")]));

        // The platform's own retry carries the same payload, so accepting its env
        // would be a no-op — but a *changed* env under the same token would not be,
        // and this is where that is refused.
        assert_eq!(
            state.bootstrap(b"tok-a", env(&[("WANTED", "hijacked")])),
            Bootstrap::AlreadyIdentical
        );
        assert_eq!(state.launch_env(), env(&[("WANTED", "yes")]));

        assert_eq!(
            state.bootstrap(b"tok-b", env(&[("WANTED", "hijacked")])),
            Bootstrap::Conflict
        );
        assert_eq!(state.launch_env(), env(&[("WANTED", "yes")]));
    }

    #[test]
    fn token_check_before_bootstrap_is_distinguishable_from_a_bad_token() {
        let state = state();
        // None means "not bootstrapped" -> 503. Some(false) means "wrong token"
        // -> 401. Collapsing these two was a real defect class.
        assert_eq!(state.token_matches(b"anything"), None);
    }

    /// Panics inside `f` on purpose to poison the lock, returning once the poison
    /// is confirmed. Catching the unwind is what keeps the test process alive.
    fn poison(state: &AppState, f: impl FnOnce() + std::panic::UnwindSafe) {
        let state = state.clone();
        let previous = std::panic::take_hook();
        // The default hook would print a backtrace for a panic the test is causing
        // deliberately, which makes a passing run look like a failing one.
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(move || {
            state.with_execs(|_| f());
        });
        std::panic::set_hook(previous);
        assert!(result.is_err(), "the deliberate panic must have unwound");
    }

    #[test]
    fn a_panic_while_the_exec_lock_is_held_does_not_wedge_the_registry() {
        // The hazard this replaces: `.expect("exec lock poisoned")` made every
        // request after the first panic panic too, in a VM with no other way in.
        let state = state();
        state.with_execs(|execs| {
            assert!(execs.is_empty());
        });

        poison(&state, || {
            panic!("a handler bug, while the registry lock is held")
        });

        // The lock really is poisoned — otherwise this test would pass against the
        // old `.expect()` code and prove nothing.
        assert!(
            state.inner.execs.lock().is_err(),
            "the registry lock must actually be poisoned for this test to mean anything",
        );

        // And the registry is still usable, both for reads and for writes.
        let len = state.with_execs(|execs| execs.len());
        assert_eq!(len, 0);
    }

    #[test]
    fn a_poisoned_token_lock_still_answers_the_authorization_path() {
        // The worst case of the two: the token check runs on every control
        // request, so propagating this poison closes the whole control API
        // permanently.
        let state = state();
        state.bootstrap(b"tok-a", env(&[("WANTED", "yes")]));

        // Poisoned directly, since no closure runs under the token lock.
        let inner = state.inner.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(move || {
            let _guard = inner.token.lock().expect("held");
            panic!("panic with the token lock held");
        });
        std::panic::set_hook(previous);
        assert!(result.is_err());
        assert!(
            state.inner.token.lock().is_err(),
            "the token lock must actually be poisoned",
        );

        // Recovery hands back the same Option, so the installed token is intact and
        // the security property survives: poisoning is not a bootstrap bypass.
        assert!(state.is_bootstrapped());
        assert_eq!(state.token_matches(b"tok-a"), Some(true));
        assert_eq!(state.token_matches(b"tok-b"), Some(false));
        // A different token is still refused, so the one-shot check is re-run
        // against what is really there rather than against an empty slot.
        assert_eq!(
            state.bootstrap(b"tok-b", env(&[("WANTED", "hijacked")])),
            Bootstrap::Conflict
        );
        assert_eq!(state.token_matches(b"tok-a"), Some(true));
        // And the env the winner installed is still what a child would get, which
        // is the launch-env half of "poisoning is not a bootstrap bypass".
        assert_eq!(state.launch_env(), env(&[("WANTED", "yes")]));
    }
}

//! Bootstrap state and the exec registry.
//!
//! The one-shot bootstrap lives here. Its behavior is specified by the
//! `agentd-model` crate, which checks over every interleaving that a losing racer
//! never replaces the winner's token.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::exec::ExecEntry;

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
    execs: Mutex<HashMap<String, ExecEntry>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                token: Mutex::new(None),
                execs: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Installs the agent token, once. Returns which of the three outcomes
    /// occurred so the route can map it to a status code.
    pub fn bootstrap(&self, presented: &[u8]) -> Bootstrap {
        let mut slot = self.inner.token.lock().expect("token lock poisoned");
        match slot.as_deref() {
            None => {
                *slot = Some(presented.to_vec());
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

    /// Whether bootstrap has completed. Control routes answer 503 until it has.
    pub fn is_bootstrapped(&self) -> bool {
        self.inner
            .token
            .lock()
            .expect("token lock poisoned")
            .is_some()
    }

    /// Compares a presented token against the installed one in constant time.
    /// Returns `None` when no token is installed, which the caller must map to
    /// 503 rather than 401 — an unbootstrapped daemon is not the same as a bad
    /// credential, and the client acts differently on each.
    pub fn token_matches(&self, presented: &[u8]) -> Option<bool> {
        let slot = self.inner.token.lock().expect("token lock poisoned");
        let installed = slot.as_deref()?;
        Some(crate::auth::constant_time_eq(installed, presented))
    }

    /// Runs a closure against the exec registry.
    pub fn with_execs<T>(&self, f: impl FnOnce(&mut HashMap<String, ExecEntry>) -> T) -> T {
        let mut execs = self.inner.execs.lock().expect("exec lock poisoned");
        f(&mut execs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::new(Config::default())
    }

    #[test]
    fn first_bootstrap_installs() {
        let state = state();
        assert!(!state.is_bootstrapped());
        assert_eq!(state.bootstrap(b"tok-a"), Bootstrap::Installed);
        assert!(state.is_bootstrapped());
    }

    #[test]
    fn identical_replay_succeeds_and_a_different_token_conflicts() {
        let state = state();
        state.bootstrap(b"tok-a");

        // The platform retrying its own hook must not be told the VM is broken.
        assert_eq!(state.bootstrap(b"tok-a"), Bootstrap::AlreadyIdentical);
        // A hijack attempt is refused and changes nothing.
        assert_eq!(state.bootstrap(b"tok-b"), Bootstrap::Conflict);
        assert_eq!(state.token_matches(b"tok-a"), Some(true));
        assert_eq!(state.token_matches(b"tok-b"), Some(false));
    }

    #[test]
    fn token_check_before_bootstrap_is_distinguishable_from_a_bad_token() {
        let state = state();
        // None means "not bootstrapped" -> 503. Some(false) means "wrong token"
        // -> 401. Collapsing these two was a real defect class.
        assert_eq!(state.token_matches(b"anything"), None);
    }
}

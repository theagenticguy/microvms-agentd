//! A checked model of the `microvms-agentd` bootstrap and exec lifecycle.
//!
//! This crate contains no daemon code. It is an executable specification: a state
//! machine whose reachable states stateright enumerates exhaustively, plus the
//! safety properties the real daemon must uphold. Its purpose is to settle
//! questions that were argued in prose during Harbor PR #2469 — above all whether
//! an in-VM process can hijack the unauthenticated `/run` bootstrap hook — with a
//! proof over every interleaving rather than a handful of example tests.
//!
//! # Why an in-VM attacker is modeled at all
//!
//! Measured against the real service on 2026-08-04 in us-east-1: the platform's
//! own `/run` hook arrives from `127.0.0.1`, so it is indistinguishable at the
//! socket level from a request sent by a process inside the MicroVM. Filtering by
//! source address is therefore not merely unverified, it is wrong — it would
//! reject the platform's legitimate bootstrap. The one-shot bootstrap is the only
//! available defense, and this model is where we check whether it suffices.
//!
//! # The invariant under test
//!
//! The daemon is the container `CMD` (`ENTRYPOINT []` plus
//! `CMD ["microvms-agentd"]`), and the harness issues its first `exec` only after
//! readiness succeeds, which requires the token to already be installed. So in a
//! correct deployment no in-VM workload runs before bootstrap completes. That is
//! an *unenforced* invariant: nothing in the daemon can stop a base image from
//! starting its own background process. [`Config::attacker_before_bootstrap`]
//! toggles exactly that assumption, so the model reports both facts:
//!
//! * with the invariant held, the attacker never obtains authority, and
//! * with it broken, stateright produces the concrete path by which it does.
//!
//! The second half is what prose could not deliver: it prices the invariant
//! instead of asserting that the invariant is fine.

pub mod client;

use stateright::{Model, Property};

/// Who is sending a request. The daemon cannot tell these apart from the socket;
/// the distinction exists only so properties can talk about the attacker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Principal {
    /// The Lambda MicroVMs control plane delivering a lifecycle hook.
    Platform,
    /// The harness (Harbor and friends) driving the control API.
    Client,
    /// A process inside the MicroVM that nobody asked to start.
    Attacker,
}

/// A bearer token. Values are symbolic: what matters is only whether two tokens
/// are equal, never their bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Token {
    /// Minted by the client and delivered through `runHookPayload`.
    Harness,
    /// Minted by an in-VM attacker hoping to install it first.
    Attacker,
}

/// Bootstrap is one-shot: the token can be installed once and never replaced.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Boot {
    /// No token installed. Control routes answer 503.
    Uninitialized,
    /// A token is installed, along with who installed it.
    Ready { token: Token, by: Principal },
}

/// Where a single exec sits in its lifecycle. Output lives until the caller acks,
/// which is what makes a retried poll safe.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExecPhase {
    /// Child spawned, still running.
    Running,
    /// Child exited; output is buffered and readable.
    Exited,
    /// Caller acked; output has been released and the entry awaits collection.
    Acked,
}

/// One exec slot, keyed by the caller-minted idempotency key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Exec {
    pub id: u8,
    pub phase: ExecPhase,
    /// True while the daemon still holds this exec's captured output.
    pub output_held: bool,
    /// How many children this id has spawned. Must never exceed one, however
    /// many times the caller retries `/exec/start`.
    pub spawns: u8,
    /// How many times `/exec/start` was called with this id.
    pub starts: u8,
}

/// The response the daemon produced for the action just taken. Recording it in
/// the state lets safety properties assert on the request/response mapping, not
/// only on internal state.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Response {
    /// Request accepted.
    Ok,
    /// A second `/run` carrying a different token, or an ack out of phase.
    Conflict,
    /// A control request whose token does not match the installed one.
    Unauthorized,
    /// A control request arriving before bootstrap.
    Unavailable,
}

/// What a caller can ask of the control API.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ControlOp {
    /// `POST /v1/exec/start` with a caller-minted id.
    ExecStart(u8),
    /// `GET /v1/exec/{id}` — must never mutate.
    ExecPoll(u8),
    /// `POST /v1/exec/{id}/ack` — releases output.
    ExecAck(u8),
    /// TTL garbage collection of acked entries.
    Collect,
}

/// A request against the daemon.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Action {
    /// The unauthenticated `/run` lifecycle hook.
    RunHook { from: Principal, token: Token },
    /// An authenticated control request.
    Control {
        from: Principal,
        token: Token,
        op: ControlOp,
    },
    /// A spawned child finishing on its own.
    ChildExit(u8),
}

/// Everything the daemon knows, plus the audit counters properties assert on.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct State {
    pub boot: Boot,
    pub execs: Vec<Exec>,
    pub last: Option<(Action, Response)>,
    /// Incremented if the installed token is ever replaced. Must stay 0.
    pub token_replacements: u8,
    /// Set if any request bearing the attacker's token was served by the control
    /// API. Must stay false when the deployment invariant holds.
    pub attacker_authorized: bool,
    /// Set if an exec's output was destroyed without the caller acking it.
    pub released_without_ack: bool,
    /// Bound on hook traffic, to keep the state space finite.
    pub hooks_seen: u8,
}

/// Model parameters. The defaults describe a correct deployment.
#[derive(Clone, Debug)]
pub struct Config {
    /// Distinct exec ids available to callers.
    pub exec_ids: u8,
    /// How many `/run` hooks may arrive in total.
    pub max_hooks: u8,
    /// How many times a caller may retry `/exec/start` for one id.
    pub max_starts_per_exec: u8,
    /// Whether an in-VM process may act *before* bootstrap completes. False
    /// models the real harness, where the daemon is `CMD` and no workload runs
    /// until after readiness. True models a base image that starts its own
    /// background process, which is the case the PR's review round raised.
    pub attacker_before_bootstrap: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            exec_ids: 2,
            max_hooks: 3,
            max_starts_per_exec: 2,
            attacker_before_bootstrap: false,
        }
    }
}

impl Config {
    /// The correct deployment: the platform's hook lands before any in-VM
    /// workload can act.
    pub fn deployment_invariant_held() -> Self {
        Self::default()
    }

    /// The deployment whose base image starts a process before bootstrap.
    pub fn deployment_invariant_broken() -> Self {
        Self {
            attacker_before_bootstrap: true,
            ..Self::default()
        }
    }
}

impl State {
    fn exec(&self, id: u8) -> Option<&Exec> {
        self.execs.iter().find(|e| e.id == id)
    }

    fn exec_mut(&mut self, id: u8) -> Option<&mut Exec> {
        self.execs.iter_mut().find(|e| e.id == id)
    }

    fn installed_token(&self) -> Option<Token> {
        match self.boot {
            Boot::Uninitialized => None,
            Boot::Ready { token, .. } => Some(token),
        }
    }
}

/// The daemon model.
#[derive(Clone, Debug)]
pub struct Agentd {
    pub cfg: Config,
}

impl Agentd {
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    /// Whether an in-VM process is running and therefore able to send requests.
    fn attacker_active(&self, state: &State) -> bool {
        self.cfg.attacker_before_bootstrap || state.boot != Boot::Uninitialized
    }
}

impl Model for Agentd {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![State {
            boot: Boot::Uninitialized,
            execs: Vec::new(),
            last: None,
            token_replacements: 0,
            attacker_authorized: false,
            released_without_ack: false,
            hooks_seen: 0,
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        // Lifecycle hooks. The platform sends the harness token; an in-VM
        // attacker sends its own. Both arrive over loopback and are
        // indistinguishable to the daemon.
        if state.hooks_seen < self.cfg.max_hooks {
            actions.push(Action::RunHook {
                from: Principal::Platform,
                token: Token::Harness,
            });
            if self.attacker_active(state) {
                actions.push(Action::RunHook {
                    from: Principal::Attacker,
                    token: Token::Attacker,
                });
            }
        }

        // Control traffic. The client holds the harness token; the attacker can
        // only present its own.
        let mut callers = vec![(Principal::Client, Token::Harness)];
        if self.attacker_active(state) {
            callers.push((Principal::Attacker, Token::Attacker));
        }

        for (from, token) in callers {
            for id in 0..self.cfg.exec_ids {
                let starts = state.exec(id).map_or(0, |e| e.starts);
                if starts < self.cfg.max_starts_per_exec {
                    actions.push(Action::Control {
                        from,
                        token,
                        op: ControlOp::ExecStart(id),
                    });
                }
                if state.exec(id).is_some() {
                    actions.push(Action::Control {
                        from,
                        token,
                        op: ControlOp::ExecPoll(id),
                    });
                    actions.push(Action::Control {
                        from,
                        token,
                        op: ControlOp::ExecAck(id),
                    });
                }
            }
            actions.push(Action::Control {
                from,
                token,
                op: ControlOp::Collect,
            });
        }

        // Children finish on their own schedule.
        for exec in &state.execs {
            if exec.phase == ExecPhase::Running {
                actions.push(Action::ChildExit(exec.id));
            }
        }
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut next = last.clone();

        let response = match action {
            Action::RunHook { token, from } => {
                next.hooks_seen += 1;
                match last.boot {
                    // First writer installs the token. Nothing else can.
                    Boot::Uninitialized => {
                        next.boot = Boot::Ready { token, by: from };
                        Response::Ok
                    }
                    // An identical replay is idempotent: the platform may retry
                    // its own hook and must not be told the VM is broken.
                    Boot::Ready {
                        token: installed, ..
                    } if installed == token => Response::Ok,
                    // A different token after bootstrap is a hijack attempt.
                    Boot::Ready { .. } => Response::Conflict,
                }
            }

            Action::Control { from, token, op } => match last.installed_token() {
                // The control API is closed until bootstrap completes. Answering
                // 503 rather than 404 matters: clients map 404 onto "missing
                // file", so the wrong code turns a protocol error into a phantom
                // absent artifact.
                None => Response::Unavailable,

                // Authorization is decided before any body is read, so an
                // unauthorized caller can never make the daemon allocate.
                Some(installed) if installed != token => Response::Unauthorized,

                Some(_) => {
                    if from == Principal::Attacker {
                        next.attacker_authorized = true;
                    }
                    match op {
                        ControlOp::ExecStart(id) => {
                            match next.exec_mut(id) {
                                // Retry of a known id: count the call, spawn
                                // nothing, touch no other field. This is the
                                // idempotency contract the ack protocol exists
                                // to provide.
                                Some(exec) => exec.starts += 1,
                                None => next.execs.push(Exec {
                                    id,
                                    phase: ExecPhase::Running,
                                    output_held: true,
                                    spawns: 1,
                                    starts: 1,
                                }),
                            }
                            Response::Ok
                        }
                        // Polling is read-only by construction: no field of the
                        // exec is touched here.
                        ControlOp::ExecPoll(_) => Response::Ok,
                        ControlOp::ExecAck(id) => match next.exec_mut(id) {
                            Some(exec) if exec.phase == ExecPhase::Exited => {
                                exec.phase = ExecPhase::Acked;
                                exec.output_held = false;
                                Response::Ok
                            }
                            // Acking a live exec is a conflict, not a silent
                            // success that would drop output still being written.
                            _ => Response::Conflict,
                        },
                        ControlOp::Collect => {
                            // Only acked entries may be collected. Collecting
                            // anything else destroys output the caller never
                            // read, which is the defect the Python daemon
                            // shipped by unlinking on exit.
                            if next
                                .execs
                                .iter()
                                .any(|e| e.phase != ExecPhase::Acked && e.output_held)
                                && next.execs.iter().all(|e| e.phase == ExecPhase::Acked)
                            {
                                next.released_without_ack = true;
                            }
                            next.execs.retain(|e| e.phase != ExecPhase::Acked);
                            Response::Ok
                        }
                    }
                }
            },

            Action::ChildExit(id) => {
                match next.exec_mut(id) {
                    Some(exec) if exec.phase == ExecPhase::Running => {
                        exec.phase = ExecPhase::Exited;
                    }
                    _ => return None,
                }
                Response::Ok
            }
        };

        // One-shot bootstrap: record any replacement so a property can forbid it.
        if let (
            Boot::Ready { token: before, .. },
            Boot::Ready {
                token: after_token, ..
            },
        ) = (last.boot, next.boot)
            && before != after_token
        {
            next.token_replacements += 1;
        }

        next.last = Some((action, response));
        if next == *last {
            return None;
        }
        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            // The claim the review bot filed five times, now checked over every
            // interleaving rather than argued in a docstring. Stated
            // unconditionally on purpose: a property that consults the config it
            // is meant to discriminate becomes vacuous in the very run where it
            // should fail. Under
            // [`Config::deployment_invariant_held`] this passes; under
            // [`Config::deployment_invariant_broken`] stateright reports the
            // counterexample path, which is the point of having both.
            Property::<Self>::always("attacker never authorized", |_, state| {
                !state.attacker_authorized
            }),
            Property::<Self>::always("bootstrap is one-shot", |_, state| {
                state.token_replacements == 0
            }),
            Property::<Self>::always(
                "only the installed token is accepted",
                |_, state| match state.last {
                    Some((Action::RunHook { token, .. }, Response::Ok)) => {
                        state.installed_token() == Some(token)
                    }
                    _ => true,
                },
            ),
            Property::<Self>::always("control API is closed before bootstrap", |_, state| {
                match state.last {
                    Some((Action::Control { .. }, response)) => {
                        state.boot != Boot::Uninitialized || response == Response::Unavailable
                    }
                    _ => true,
                }
            }),
            Property::<Self>::always("output is never released before ack", |_, state| {
                !state.released_without_ack
                    && state
                        .execs
                        .iter()
                        .all(|e| e.output_held || e.phase == ExecPhase::Acked)
            }),
            Property::<Self>::always("a retried start never spawns twice", |_, state| {
                state.execs.iter().all(|e| e.spawns == 1)
            }),
            Property::<Self>::always("one exec entry per id", |_, state| {
                state
                    .execs
                    .iter()
                    .all(|e| state.execs.iter().filter(|o| o.id == e.id).count() == 1)
            }),
            // Coverage checks. Without these, the safety properties above could
            // pass over a state space that never reached the interesting states —
            // the failure mode where a green suite measures nothing.
            Property::<Self>::sometimes("bootstrap completes", |_, state| {
                matches!(
                    state.boot,
                    Boot::Ready {
                        token: Token::Harness,
                        by: Principal::Platform
                    }
                )
            }),
            Property::<Self>::sometimes("a hijack is refused with 409", |_, state| {
                matches!(
                    state.last,
                    Some((Action::RunHook { .. }, Response::Conflict))
                )
            }),
            Property::<Self>::sometimes("an identical replay is accepted", |_, state| {
                matches!(state.last, Some((Action::RunHook { .. }, Response::Ok)))
                    && state.hooks_seen > 1
            }),
            Property::<Self>::sometimes("an exec runs, exits, and is acked", |_, state| {
                state.execs.iter().any(|e| e.phase == ExecPhase::Acked)
            }),
            Property::<Self>::sometimes("a start is retried", |_, state| {
                state.execs.iter().any(|e| e.starts > 1)
            }),
            Property::<Self>::sometimes("an unauthorized caller is refused", |_, state| {
                matches!(
                    state.last,
                    Some((Action::Control { .. }, Response::Unauthorized))
                )
            }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stateright::{Checker, Model};

    /// The headline result: with the deployment invariant held, every safety and
    /// coverage property passes over the whole reachable state space.
    #[test]
    fn correct_deployment_satisfies_every_property() {
        Agentd::new(Config::deployment_invariant_held())
            .checker()
            .spawn_bfs()
            .join()
            .assert_properties();
    }

    /// The honest converse: if a base image starts a process before bootstrap,
    /// the attacker wins, and the model hands back the path by which it does.
    /// This is the price of the `ENTRYPOINT []` + `CMD` invariant, stated as a
    /// checked fact rather than a paragraph.
    #[test]
    fn breaking_the_deployment_invariant_lets_the_attacker_in() {
        let checker = Agentd::new(Config::deployment_invariant_broken())
            .checker()
            .spawn_bfs()
            .join();
        let path = checker.assert_any_discovery("attacker never authorized");
        let steps = path.into_actions();
        assert!(
            steps.iter().any(|a| matches!(
                a,
                Action::RunHook {
                    from: Principal::Attacker,
                    ..
                }
            )),
            "the counterexample must include an attacker bootstrap, got {steps:?}"
        );
    }

    /// Even with an in-VM process racing the platform, the token can only be
    /// installed once: whoever loses gets 409 and never replaces the winner.
    #[test]
    fn one_shot_bootstrap_holds_even_against_a_racing_attacker() {
        Agentd::new(Config::deployment_invariant_broken())
            .checker()
            .spawn_bfs()
            .join()
            .assert_no_discovery("bootstrap is one-shot");
    }

    /// Polling must not mutate the exec it reads. Expressed against the
    /// transition function directly, since read-only is a property of the step
    /// rather than of any reachable state.
    #[test]
    fn polling_does_not_mutate_the_exec() {
        let model = Agentd::new(Config::deployment_invariant_held());
        let mut state = model.init_states().pop().expect("one init state");
        state.boot = Boot::Ready {
            token: Token::Harness,
            by: Principal::Platform,
        };
        state.execs.push(Exec {
            id: 0,
            phase: ExecPhase::Exited,
            output_held: true,
            spawns: 1,
            starts: 1,
        });

        let polled = model
            .next_state(
                &state,
                Action::Control {
                    from: Principal::Client,
                    token: Token::Harness,
                    op: ControlOp::ExecPoll(0),
                },
            )
            .expect("a poll produces a state");

        assert_eq!(
            polled.execs, state.execs,
            "poll changed exec state: {:?} -> {:?}",
            state.execs, polled.execs
        );
    }

    /// A control request arriving before bootstrap gets 503, and creates nothing.
    #[test]
    fn control_before_bootstrap_is_unavailable() {
        let model = Agentd::new(Config::deployment_invariant_held());
        let state = model.init_states().pop().expect("one init state");

        let next = model
            .next_state(
                &state,
                Action::Control {
                    from: Principal::Client,
                    token: Token::Harness,
                    op: ControlOp::ExecStart(0),
                },
            )
            .expect("a rejected control request still records a response");

        assert_eq!(
            next.last.expect("response recorded").1,
            Response::Unavailable
        );
        assert!(
            next.execs.is_empty(),
            "no exec may be created before bootstrap"
        );
    }

    /// A retried start returns success without spawning a second child.
    #[test]
    fn retried_start_is_idempotent() {
        let model = Agentd::new(Config::deployment_invariant_held());
        let mut state = model.init_states().pop().expect("one init state");
        state.boot = Boot::Ready {
            token: Token::Harness,
            by: Principal::Platform,
        };

        let start = Action::Control {
            from: Principal::Client,
            token: Token::Harness,
            op: ControlOp::ExecStart(0),
        };
        let first = model.next_state(&state, start).expect("first start");
        let second = model.next_state(&first, start).expect("retried start");

        assert_eq!(second.execs.len(), 1, "retry created a second entry");
        assert_eq!(second.execs[0].spawns, 1, "retry spawned a second child");
        assert_eq!(second.execs[0].starts, 2, "retry was not counted");
        assert_eq!(second.execs[0].phase, first.execs[0].phase);
    }
}

// SPDX-License-Identifier: Apache-2.0
//! A checked model of the **client's** view of one MicroVM's lifecycle.
//!
//! Sibling to the daemon model in [`crate`], and deliberately a sibling rather than an
//! extension: that model is about what happens *inside* the VM — who can install the
//! bootstrap token when the platform's own `/run` hook is indistinguishable from an in-VM
//! process. This one is about what `microvms-core`'s `Sandbox` may do *from outside*, and
//! its interesting failures are wire calls issued in states where the call is already
//! known to be pointless.
//!
//! # What this model is for
//!
//! `spec/core.symspec.json` carries a five-variable state model and Z3 has already proved
//! three things over it: bootstrap happens at most once, a suspend from a non-RUNNING state
//! is unreachable, and TERMINATED never returns to RUNNING. Those proofs are about the
//! *specification*. This model is the second half — it enumerates every interleaving of the
//! actions a caller and the platform can actually take, so the same three claims become
//! claims about a state machine somebody can execute rather than about prose.
//!
//! The variables are the symspec's, unchanged: `vm_state`, `token_installed`,
//! `image_exists`, `was_terminated`, `bootstrap_count`.
//!
//! # The wire calls are in the state, which is the whole trick
//!
//! [`State::wire`] counts the calls the client issued. That is what makes the properties
//! here say something a state-only model cannot: "a resume after terminate is rejected" is
//! satisfied by a client that calls `ResumeMicrovm`, gets an error, and stays put — and
//! that client burns a poll timeout to learn what it already knew. The property that
//! matters is that **no resume call ever fires once `was_terminated` holds**, and it can
//! only be stated because the count is a state variable.
//!
//! Same shape for STATE-5: the assertion is not that the client ends up in a legal state
//! after a suspend it should not have issued, it is that `wire.suspends` never increments
//! outside RUNNING.
//!
//! # Every always-property has a sometimes-property beside it
//!
//! A safety property over a state space that never reaches the interesting state passes
//! while measuring nothing — the failure mode `.erpaval/solutions/test-failures/` records
//! and the one the daemon model's coverage checks already guard against. So each claim here
//! is paired: `no resume wire call after terminate` is only worth reading next to
//! `a resume is attempted after a terminate`, which proves the checker got there.
//!
//! # The window is a boolean, not a clock
//!
//! [`Action::ResumeRequested`] carries `window_open`, which is the model's whole
//! representation of `suspendedDurationSeconds`. Time is not in the state space, and it
//! should not be: what the model has to settle is whether the client *checks* before
//! calling, and both answers to that check are reachable in one step. The arithmetic — a
//! monotonic clock, a stamp taken before the wait, the inclusive boundary — is what the
//! Rust test suite drives with an injectable clock. Putting a counter in here instead would
//! multiply the state space by the window's width and prove the same one bit.

use stateright::{Model, Property};

/// The symspec's `vm_state`, as the client tracks it.
///
/// Mirrors `microvms_core::sandbox::Lifecycle` by convention rather than by dependency —
/// this crate has no cargo edge to `microvms-core`, exactly as it has none to `agentd`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum VmState {
    /// The initial state, and the state an accepted launch sits in (STATE-1).
    Pending,
    /// The platform reported the run hook succeeded (STATE-2).
    Running,
    /// A suspend was accepted (STATE-4).
    Suspending,
    /// The platform reported suspension complete (STATE-6).
    Suspended,
    /// A terminate was accepted (STATE-9).
    Terminating,
    /// The platform reported termination complete (STATE-10).
    Terminated,
}

/// What the client did to the control plane, counted.
///
/// The reason this is in the state rather than derived: see the module docs. A property
/// about a call that should never happen cannot be written about states alone, because the
/// state a bad call leaves behind is usually the state the client was already in.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Wire {
    /// `RunMicrovm` calls. More than one per sandbox is a second VM behind one handle.
    pub launches: u8,
    /// `SuspendMicrovm` calls. Must only ever increment from RUNNING (STATE-5).
    pub suspends: u8,
    /// `ResumeMicrovm` calls. Must never increment once `was_terminated` (STATE-11), nor
    /// with the suspended window closed (STATE-12).
    pub resumes: u8,
    /// `TerminateMicrovm` calls.
    pub terminates: u8,
    /// Run-hook payload deliveries. Exactly the bootstraps: a resume must add none
    /// (STATE-7).
    pub payloads: u8,
}

/// How a transition was answered, so a property can talk about the request/response
/// mapping and not only about the state that resulted.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Verdict {
    /// The client issued the call.
    Issued,
    /// The client refused locally, before any call. Every STATE-5/11/12 refusal is this.
    RefusedLocally,
    /// The action did not apply — the platform reporting something the client is not
    /// waiting for.
    Ignored,
}

/// What the client or the platform can do.
///
/// Split by who acts, because that is what the interesting interleavings are made of: the
/// platform's completion reports arrive whenever they arrive, and a caller's next request
/// may already be in flight.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Action {
    /// A launch the service accepted (STATE-1).
    LaunchAccepted,
    /// The platform reporting the run hook answered with a success status (STATE-2).
    HookSucceeded,
    /// The caller sending a request through the endpoint proxy, which is what mints —
    /// and caches — a proxy token when none is held. The warm half of STATE-8: without
    /// this action nothing ever fills the cache, and "the completion drops the token"
    /// is a claim about a cache that is empty in every reachable state.
    ExecRequested,
    /// The caller asking for a suspend (STATE-4, STATE-5).
    SuspendRequested,
    /// The platform reporting suspension complete (STATE-6).
    SuspendComplete,
    /// The caller asking for a resume (STATE-7, STATE-11, STATE-12).
    ///
    /// `window_open` is the launch-time `suspendedDurationSeconds` window, as the client
    /// knows it. See the module docs on why a boolean rather than a clock.
    ResumeRequested { window_open: bool },
    /// The platform reporting the resume complete, which is where the cached proxy token is
    /// dropped (STATE-8).
    ResumeComplete,
    /// The caller asking for a terminate (STATE-9).
    TerminateRequested,
    /// The platform reporting termination complete (STATE-10).
    TerminateComplete,
}

/// The symspec's five variables, plus what reached the wire and what the client holds.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct State {
    // ── the symspec's five ───────────────────────────────────────────────────
    pub vm_state: VmState,
    pub token_installed: bool,
    pub image_exists: bool,
    pub was_terminated: bool,
    pub bootstrap_count: u8,

    // ── what the client did and holds ────────────────────────────────────────
    /// Every control-plane call, counted. See [`Wire`].
    pub wire: Wire,
    /// Whether a proxy token is cached. Filled by the first proxied request
    /// ([`Action::ExecRequested`]) that finds it cold, dropped on every resume
    /// completion (STATE-8).
    pub proxy_token_cached: bool,
    /// How many proxy tokens have been minted, which is the only externally visible
    /// evidence that an invalidation happened at all: a token cached forever and one
    /// refreshed on schedule produce identical successful requests.
    pub mints: u8,
    /// The identity of the installed token, so a property can say it is the *same* token
    /// across a suspend/resume rather than merely that one is installed.
    ///
    /// Symbolic: `Some(0)` is the token the first launch's payload delivered, `Some(1)`
    /// a planted second launch's. Identities are distinguishable per launch precisely so
    /// the replacement detector has two values that *can* differ — an install that
    /// always wrote `Some(0)` would make `token_replacements` a comparison of a value
    /// against itself. What matters is only whether two readings are equal, never the
    /// value.
    pub installed_token: Option<u8>,
    /// Whether a resume the client issued has not yet been reported complete.
    ///
    /// Without this the model had a real hole, found by the checker rather than by reading:
    /// a resume issued legally from SUSPENDED, then a terminate, then the resume's
    /// completion arriving — which set `vm_state` back to RUNNING on a VM that
    /// `was_terminated`. That is STATE-11 violated by an interleaving no state-only gate
    /// catches, and the fix is that a completion only applies while the client is still
    /// waiting for one *and* still SUSPENDED. The terminate wins, which is what the real
    /// client does: `terminate` clears the session and the lifecycle before anything else.
    pub resume_in_flight: bool,

    // ── audit counters, in the daemon model's idiom ───────────────────────────
    //
    // Recorded in the transition, where the *pre*-state is still in hand. The first attempt
    // at these properties tried to infer the pre-state from the post-state and was wrong —
    // a suspend from SUSPENDED and one from RUNNING both land in SUSPENDING, so there is
    // nothing in the resulting state to tell them apart. A counter set at the moment of the
    // decision is the honest way to say "this call should not have happened".
    /// Incremented when a suspend call was issued from a non-RUNNING state (STATE-5).
    pub suspends_outside_running: u8,
    /// Incremented when a resume call was issued on a terminated VM (STATE-11).
    pub resumes_after_terminate: u8,
    /// Incremented when a resume call was issued with the window closed (STATE-12).
    pub resumes_window_closed: u8,
    /// Incremented if the installed token is ever replaced (STATE-3, STATE-7).
    pub token_replacements: u8,

    /// The action just taken and how it was answered.
    pub last: Option<(Action, Verdict)>,
    /// Bound on suspend/resume cycling, to keep the space finite.
    pub cycles: u8,
}

/// Model parameters.
#[derive(Clone, Debug)]
pub struct Config {
    /// How many suspend/resume cycles a caller may drive.
    ///
    /// Two, not one: the defect a single cycle cannot see is a client that accumulates
    /// every suspension's elapsed time into one total and refuses the second resume even
    /// though its own window is wide open. That is a real bug the Rust suite has a test
    /// for, and one cycle is blind to it.
    pub max_cycles: u8,
    /// Whether the client is allowed to skip its local guards.
    ///
    /// The falsification switch, and the reason the model is worth running: with this true
    /// the client issues suspends from non-RUNNING states, resumes after a terminate, and
    /// resumes with a closed window, and stateright hands back the path. That is how the
    /// properties are shown to be capable of failing rather than merely green.
    pub skip_local_guards: bool,
    /// Whether a second bootstrap may be attempted — the planted double-bootstrap the
    /// packet's guard proofs name.
    pub allow_double_bootstrap: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_cycles: 2,
            skip_local_guards: false,
            allow_double_bootstrap: false,
        }
    }
}

impl Config {
    /// A correct client: every local guard in place.
    pub fn guards_held() -> Self {
        Self::default()
    }

    /// The client that calls first and reads the failure afterwards.
    pub fn guards_skipped() -> Self {
        Self {
            skip_local_guards: true,
            ..Self::default()
        }
    }

    /// The client whose `run` may be called twice.
    pub fn double_bootstrap_planted() -> Self {
        Self {
            allow_double_bootstrap: true,
            ..Self::default()
        }
    }
}

/// The client lifecycle model.
#[derive(Clone, Debug)]
pub struct ClientLifecycle {
    pub cfg: Config,
}

impl ClientLifecycle {
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    /// Whether a local guard stops this call. `false` under
    /// [`Config::guards_skipped`], which is what makes the properties falsifiable.
    fn guarded(&self) -> bool {
        !self.cfg.skip_local_guards
    }
}

impl Model for ClientLifecycle {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![State {
            vm_state: VmState::Pending,
            token_installed: false,
            image_exists: false,
            was_terminated: false,
            bootstrap_count: 0,
            wire: Wire::default(),
            proxy_token_cached: false,
            mints: 0,
            installed_token: None,
            resume_in_flight: false,
            suspends_outside_running: 0,
            resumes_after_terminate: 0,
            resumes_window_closed: 0,
            token_replacements: 0,
            last: None,
            cycles: 0,
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        // A launch, while none has been accepted — or a second one when the double
        // bootstrap is planted, which is the only way `bootstrap_count` can reach 2.
        if state.wire.launches == 0 || (self.cfg.allow_double_bootstrap && state.wire.launches < 2)
        {
            actions.push(Action::LaunchAccepted);
        }
        // The platform's hook report. Offered whenever a launch is outstanding, and also
        // when the double bootstrap is planted, so the second delivery is reachable.
        if state.wire.launches > state.bootstrap_count {
            actions.push(Action::HookSucceeded);
        }

        // Caller requests. Offered from *every* state rather than only the legal one: the
        // model's job is to check what the client does with an illegal request, and an
        // action the model never offers is a request the properties never see.
        //
        // # Why each one is bounded by its own call count
        //
        // Because a request the client *issues* increments a counter, and a counter that can
        // increment from every state makes the space infinite — the first run of this model
        // did exactly that and never terminated. The bound is on the wire count rather than
        // on a step counter, which is the load-bearing detail: a request the client **refuses
        // locally** increments nothing, so it stays offered from every state and the
        // refusal paths are explored exhaustively. Only the calls that reach the wire are
        // capped, and they are capped one above the cycle bound so a legal cycle is never
        // the thing that runs out.
        if state.wire.launches > 0 {
            // A proxied request, when the cache is cold and the VM can serve it. Offered
            // narrowly rather than from every state: no STATE-n guard governs the exec
            // path, so the illegal-request verdicts the lifecycle calls need have
            // nothing to say here — the action exists so the cache is fillable and the
            // STATE-8 drop has something to drop.
            if state.vm_state == VmState::Running && !state.proxy_token_cached {
                actions.push(Action::ExecRequested);
            }
            if state.wire.suspends <= self.cfg.max_cycles {
                actions.push(Action::SuspendRequested);
            }
            // Once, because a second terminate proves nothing a first does not and every
            // later state would carry a different count.
            if state.wire.terminates == 0 {
                actions.push(Action::TerminateRequested);
            }
            if state.wire.resumes <= self.cfg.max_cycles {
                for window_open in [true, false] {
                    actions.push(Action::ResumeRequested { window_open });
                }
            }
        }

        // Platform completion reports, each only where the client is waiting for it.
        if state.vm_state == VmState::Suspending {
            actions.push(Action::SuspendComplete);
        }
        if state.wire.resumes > 0 && state.vm_state != VmState::Running {
            actions.push(Action::ResumeComplete);
        }
        if state.vm_state == VmState::Terminating {
            actions.push(Action::TerminateComplete);
        }
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut next = last.clone();

        let verdict = match action {
            // STATE-1. The accepted launch moves to PENDING and records the image.
            Action::LaunchAccepted => {
                next.vm_state = VmState::Pending;
                next.image_exists = true;
                next.wire.launches += 1;
                // The payload rides on the launch — this is the run-hook delivery, and it
                // is the only action that produces one. STATE-7 rests on that: a resume
                // adds none, which is checkable precisely because deliveries have exactly
                // one source.
                next.wire.payloads += 1;
                Verdict::Issued
            }

            // STATE-2 and STATE-3. The platform reporting success is what installs the
            // token, and the count is incremented here and nowhere else.
            Action::HookSucceeded => {
                if last.vm_state != VmState::Pending || last.bootstrap_count > 0 {
                    // A hook report for a VM that already bootstrapped is the one-shot
                    // bootstrap's 409: the daemon refuses it, so nothing is replaced. This
                    // is what keeps `bootstrap_count <= 1` true even with the double
                    // bootstrap planted — the *client* may call `run` twice, and the token
                    // still installs once.
                    Verdict::Ignored
                } else {
                    next.vm_state = VmState::Running;
                    next.token_installed = true;
                    next.bootstrap_count += 1;
                    // The identity rides the launch payload, so each launch delivers a
                    // distinguishable token: `Some(0)` from the first, `Some(1)` from a
                    // planted second. That is what gives the replacement detector below
                    // two values that can differ — a fixed `Some(0)` here would make
                    // `token_replacements` a comparison of a value against itself, and
                    // the "never replaced" property a claim nothing could break.
                    next.installed_token = Some(last.wire.launches - 1);
                    Verdict::Issued
                }
            }

            // STATE-8's warm half. A proxied request that finds the cache cold caches
            // the token it minted, and what it cached stays until a resume completion
            // drops it. Not a wire call in the control-plane sense — the request rides
            // the endpoint proxy — so no `Wire` counter moves. Without this arm the
            // cache is empty in every reachable state and the STATE-8 drop below drops
            // nothing.
            Action::ExecRequested => {
                if last.vm_state != VmState::Running || last.proxy_token_cached {
                    Verdict::Ignored
                } else {
                    next.proxy_token_cached = true;
                    Verdict::Issued
                }
            }

            // STATE-4 and STATE-5.
            Action::SuspendRequested => {
                if self.guarded() && last.vm_state != VmState::Running {
                    // Refused before the wire. No counter moves, which is the assertion.
                    Verdict::RefusedLocally
                } else {
                    next.wire.suspends += 1;
                    // A suspend issued from a non-RUNNING state is what the unguarded client
                    // does. The state it leaves behind is SUSPENDING either way — which is
                    // exactly why the violation has to be recorded *here*, where the
                    // pre-state is still readable, rather than inferred from the result.
                    if last.vm_state != VmState::Running {
                        next.suspends_outside_running += 1;
                    }
                    next.vm_state = VmState::Suspending;
                    Verdict::Issued
                }
            }

            // STATE-6.
            Action::SuspendComplete => {
                if last.vm_state != VmState::Suspending {
                    Verdict::Ignored
                } else {
                    next.vm_state = VmState::Suspended;
                    Verdict::Issued
                }
            }

            // STATE-7, STATE-11, STATE-12.
            Action::ResumeRequested { window_open } => {
                let terminated = last.was_terminated || last.vm_state == VmState::Terminated;
                let wrong_state = last.vm_state != VmState::Suspended;
                if self.guarded() && (terminated || wrong_state || !window_open) {
                    // All three refusals are local and cost no call. The window is checked
                    // *first* in the real client, but the model does not order the three
                    // because the observable — zero resume calls — is the same.
                    Verdict::RefusedLocally
                } else {
                    next.wire.resumes += 1;
                    next.resume_in_flight = true;
                    // Recorded at the decision, for the same reason the suspend is: the
                    // resulting state cannot say whether the call should have been made.
                    if terminated {
                        next.resumes_after_terminate += 1;
                    }
                    if !window_open {
                        next.resumes_window_closed += 1;
                    }
                    // STATE-7's other half: no payload is added here. A resume that
                    // re-delivered would show up as `payloads > launches`.
                    Verdict::Issued
                }
            }

            // STATE-8. The resume completing is where the cached token is dropped, and the
            // next request mints — which is the only externally visible difference.
            Action::ResumeComplete => {
                // Two conditions, and the second is the one the checker found. A resume the
                // client issued legally from SUSPENDED, followed by a terminate, followed by
                // this completion arriving late, would otherwise put a `was_terminated` VM
                // back in RUNNING — STATE-11 broken by an interleaving rather than by a
                // missing guard. The terminate wins, which is what the real client does:
                // `terminate` drops the session and moves the lifecycle before anything else,
                // so a completion for a resume it has stopped waiting for changes nothing.
                if !last.resume_in_flight || last.vm_state != VmState::Suspended {
                    Verdict::Ignored
                } else {
                    next.vm_state = VmState::Running;
                    next.resume_in_flight = false;
                    next.proxy_token_cached = false;
                    next.mints += 1;
                    next.cycles += 1;
                    Verdict::Issued
                }
            }

            // STATE-9.
            Action::TerminateRequested => {
                next.wire.terminates += 1;
                next.vm_state = VmState::Terminating;
                next.was_terminated = true;
                // Whatever the client was waiting for, it has stopped waiting.
                next.resume_in_flight = false;
                Verdict::Issued
            }

            // STATE-10.
            Action::TerminateComplete => {
                if last.vm_state != VmState::Terminating {
                    Verdict::Ignored
                } else {
                    next.vm_state = VmState::Terminated;
                    Verdict::Issued
                }
            }
        };

        // One-shot bootstrap, recorded rather than assumed — the daemon model's idiom at
        // `crate`'s `token_replacements`, and for the same reason: a property can then forbid
        // a replacement without any transition having to promise it never happens.
        if let (Some(before), Some(after)) = (last.installed_token, next.installed_token)
            && before != after
        {
            next.token_replacements += 1;
        }

        // Bound the space. Checked after the transition so a cycle-capped resume is not
        // silently a different action.
        if next.cycles > self.cfg.max_cycles {
            return None;
        }

        next.last = Some((action, verdict));
        if next == *last {
            return None;
        }
        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            // ── the three Z3 already proved, restated over interleavings ─────
            //
            // Stated unconditionally rather than consulting the config, for the reason the
            // daemon model's headline property records: a property that consults the flag
            // it is meant to discriminate becomes vacuous in the very run where it should
            // fail.
            Property::<Self>::always("bootstrap happens at most once", |_, state| {
                state.bootstrap_count <= 1
            }),
            Property::<Self>::always("no suspend call outside RUNNING", |_, state| {
                // Against the counter, not against the state. The first version of this
                // property tried to infer the pre-state from the post-state — "a legal
                // suspend leaves a bootstrapped, unterminated VM" — and the checker produced
                // a twelve-step counterexample where a suspend really was issued from
                // SUSPENDED and every inferred condition still held. A suspend from RUNNING
                // and one from SUSPENDED both land in SUSPENDING, so there is nothing in the
                // resulting state to tell them apart, and only the transition knows.
                state.suspends_outside_running == 0
            }),
            Property::<Self>::always("a terminated VM never reaches RUNNING", |_, state| {
                !(state.was_terminated && state.vm_state == VmState::Running)
            }),
            Property::<Self>::always(
                "the image exists exactly when a launch was accepted",
                |_, state| {
                    // The symspec's `image_exists` is written by STATE-1 alone ("record
                    // the image as existing") and read by STATE-2's precondition, so the
                    // coherence claim is an equality: no launch, no image; a launch, an
                    // image — and a bootstrapped token therefore implies one. Stated so
                    // the variable is read at all; a symspec variable the properties
                    // never consult is dead weight the module docs promise not to carry.
                    state.image_exists == (state.wire.launches > 0)
                        && (!state.token_installed || state.image_exists)
                },
            ),
            // ── the wire-call properties, which is why calls are in the state ──
            Property::<Self>::always("no resume call after a terminate", |_, state| {
                // The packet's headline: the rejection must cost **zero** wire calls, not
                // merely end in a legal state. A client that called and read the failure
                // satisfies every state-only property here and burns a poll timeout.
                state.resumes_after_terminate == 0
            }),
            Property::<Self>::always("no resume call with the window closed", |_, state| {
                state.resumes_window_closed == 0
            }),
            Property::<Self>::always("a resume re-delivers no run-hook payload", |_, state| {
                // The launch is the only source of a payload, so a resume that delivered
                // one would push `payloads` past the launches — and a daemon whose one-shot
                // bootstrap answered 409 would read like a broken VM.
                state.wire.payloads == state.wire.launches
            }),
            Property::<Self>::always("the installed token is never replaced", |_, state| {
                // STATE-7's "reuse the installed token": the identity is stable across every
                // suspend and resume, so a client that re-minted would be caught even though
                // the count of installed tokens stayed at one.
                state.token_replacements == 0
                    && (state.installed_token.is_some() == state.token_installed)
            }),
            Property::<Self>::always(
                "a suspended session keeps its token across the cycle",
                |_, state| {
                    // The invariant the packet asks for as state equality: whatever happens
                    // to the proxy token, the *agent* token survives a freeze. Once
                    // installed it stays installed until the VM is gone.
                    !state.token_installed
                        || state.installed_token.is_some()
                        || state.vm_state == VmState::Terminated
                },
            ),
            Property::<Self>::always("a resume completion drops the proxy token", |_, state| {
                match state.last {
                    Some((Action::ResumeComplete, Verdict::Issued)) => !state.proxy_token_cached,
                    _ => true,
                }
            }),
            Property::<Self>::always("every wire call is accounted for", |_, state| {
                // The property that makes every `RefusedLocally` above mean something, and it
                // is worth stating because the first version of it did not: it matched on the
                // verdict and returned `true` in every arm, which is a property that cannot
                // fail — the exact defect the prior lessons name.
                //
                // What it says instead: the counts are consistent with the transitions that
                // could have produced them. A suspend reaches SUSPENDING, so a suspend count
                // above zero means the VM left PENDING; a resume count above zero means a
                // suspend happened first, because SUSPENDED is the only state a guarded
                // resume is issued from. A model where a refusal incremented a counter
                // anyway would break the second conjunct.
                let launched = state.wire.launches > 0;
                (state.wire.suspends == 0 || launched)
                    && (state.wire.resumes == 0
                        || state.wire.suspends > 0
                        || state.resumes_after_terminate > 0
                        || state.resumes_window_closed > 0)
                    && state.mints <= state.wire.resumes
            }),
            // ── coverage, so none of the above can pass by measuring nothing ──
            //
            // The prior lesson, made mechanical: a green suite over a state space that never
            // reached the interesting state is a suite that measures nothing.
            Property::<Self>::sometimes("a VM reaches RUNNING with its token", |_, state| {
                state.vm_state == VmState::Running && state.token_installed
            }),
            Property::<Self>::sometimes("a full suspend and resume cycle completes", |_, state| {
                state.cycles >= 1 && state.vm_state == VmState::Running
            }),
            Property::<Self>::sometimes("two cycles complete", |_, state| state.cycles >= 2),
            Property::<Self>::sometimes("a VM reaches TERMINATED", |_, state| {
                state.vm_state == VmState::Terminated && state.was_terminated
            }),
            Property::<Self>::sometimes("a resume is attempted after a terminate", |_, state| {
                // The witness that makes "no resume call after a terminate" non-vacuous: the
                // checker really did try it, and really was refused.
                matches!(
                    state.last,
                    Some((Action::ResumeRequested { .. }, Verdict::RefusedLocally))
                ) && state.was_terminated
            }),
            Property::<Self>::sometimes(
                "a resume is attempted with the window closed",
                |_, state| {
                    matches!(
                        state.last,
                        Some((
                            Action::ResumeRequested { window_open: false },
                            Verdict::RefusedLocally
                        ))
                    )
                },
            ),
            Property::<Self>::sometimes("a suspend is attempted outside RUNNING", |_, state| {
                matches!(
                    state.last,
                    Some((Action::SuspendRequested, Verdict::RefusedLocally))
                )
            }),
            Property::<Self>::sometimes("a proxy token is minted after a resume", |_, state| {
                state.mints >= 1
            }),
            Property::<Self>::sometimes(
                "a cached proxy token survives into SUSPENDED",
                |_, state| {
                    // The witness that makes the STATE-8 drop non-vacuous: the checker
                    // reached a SUSPENDED state still holding a cached token, so the
                    // resume completion offered from here really has something to drop.
                    state.vm_state == VmState::Suspended && state.proxy_token_cached
                },
            ),
            Property::<Self>::sometimes("a suspend completes", |_, state| {
                state.vm_state == VmState::Suspended
            }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stateright::{Checker, Model};

    /// The headline result: a client with its local guards in place satisfies every safety
    /// property and witnesses every coverage property, over the whole reachable space.
    #[test]
    fn a_guarded_client_satisfies_every_property() {
        ClientLifecycle::new(Config::guards_held())
            .checker()
            .spawn_bfs()
            .join()
            .assert_properties();
    }

    /// **STATE-11's guard proof, in the model.** A client that calls first and reads the
    /// failure afterwards issues a resume after a terminate, and stateright hands back the
    /// path.
    ///
    /// This is the half that makes the property above worth having. Without it, "no resume
    /// call after a terminate" is a claim about a model that might be incapable of
    /// expressing the violation.
    #[test]
    fn skipping_the_local_guards_issues_a_resume_after_a_terminate() {
        let checker = ClientLifecycle::new(Config::guards_skipped())
            .checker()
            .spawn_bfs()
            .join();
        let path = checker.assert_any_discovery("no resume call after a terminate");
        let steps = path.into_actions();
        assert!(
            steps
                .iter()
                .any(|a| matches!(a, Action::TerminateRequested)),
            "the counterexample must terminate before it resumes, got {steps:?}"
        );
        assert!(
            steps
                .iter()
                .any(|a| matches!(a, Action::ResumeRequested { .. })),
            "and it must then resume, got {steps:?}"
        );
    }

    /// **STATE-12's guard proof, in the model.** The unguarded client issues a resume with
    /// the window closed, and the counterexample names the closed window.
    #[test]
    fn skipping_the_local_guards_issues_a_resume_with_the_window_closed() {
        let checker = ClientLifecycle::new(Config::guards_skipped())
            .checker()
            .spawn_bfs()
            .join();
        let path = checker.assert_any_discovery("no resume call with the window closed");
        let steps = path.into_actions();
        assert!(
            steps
                .iter()
                .any(|a| matches!(a, Action::ResumeRequested { window_open: false })),
            "the counterexample must resume on a closed window, got {steps:?}"
        );
    }

    /// **STATE-5's guard proof, in the model.** Without the RUNNING check the client issues
    /// a suspend outside RUNNING, which is the property going red.
    #[test]
    fn skipping_the_local_guards_issues_a_suspend_outside_running() {
        ClientLifecycle::new(Config::guards_skipped())
            .checker()
            .spawn_bfs()
            .join()
            .assert_any_discovery("no suspend call outside RUNNING");
    }

    /// **STATE-3.** Even with a double bootstrap planted — the client calling `run` twice —
    /// the token installs at most once, because the daemon's one-shot bootstrap refuses the
    /// second hook.
    ///
    /// The interesting result is that this passes rather than fails: `bootstrap_count <= 1`
    /// survives a client that does the wrong thing, which is what "the only defense on that
    /// route" means. The Rust suite's guard proof covers the other half — that the client
    /// refuses the second `run` locally as well.
    #[test]
    fn a_planted_double_bootstrap_still_installs_the_token_once() {
        let model = ClientLifecycle::new(Config::double_bootstrap_planted());
        model
            .clone()
            .checker()
            .spawn_bfs()
            .join()
            .assert_no_discovery("bootstrap happens at most once");

        // The second launch has to be *reachable*, or the config above is silently a no-op
        // and the assertion is over the same space as the guarded run. Driven directly
        // rather than searched for, and it is the second hook report that carries the point:
        // the client called `run` twice and the token still installed once, because the
        // daemon's one-shot bootstrap refuses the second delivery.
        let start = model.init_states().pop().expect("one init state");
        let first = model
            .next_state(&start, Action::LaunchAccepted)
            .expect("the first launch");
        let running = model
            .next_state(&first, Action::HookSucceeded)
            .expect("the first hook installs the token");
        assert_eq!(running.bootstrap_count, 1);

        let relaunched = model
            .next_state(&running, Action::LaunchAccepted)
            .expect("the planted second launch is reachable");
        assert_eq!(relaunched.wire.launches, 2, "the config is not a no-op");
        assert_eq!(
            relaunched.wire.payloads, 2,
            "a second launch really does carry a second run-hook payload, which is why the \
             re-delivery property counts payloads per *launch* rather than per VM"
        );

        let second_hook = model
            .next_state(&relaunched, Action::HookSucceeded)
            .expect("the second hook report is still an action");
        assert_eq!(
            second_hook.bootstrap_count, 1,
            "STATE-3: the one-shot bootstrap refuses the second delivery"
        );
        assert_eq!(
            second_hook.installed_token,
            Some(0),
            "and the first token is the one still installed"
        );
        assert_eq!(second_hook.token_replacements, 0);
        assert_eq!(
            second_hook.last.expect("a verdict").1,
            Verdict::Ignored,
            "the second hook is refused rather than silently applied"
        );
    }

    /// A resume from SUSPENDED with an open window reaches RUNNING, re-delivers nothing, and
    /// drops the proxy token — asserted against the transition function directly, since a
    /// three-step sequence is clearer read than searched for.
    #[test]
    fn a_resume_reuses_the_token_and_drops_the_proxy_token() {
        let model = ClientLifecycle::new(Config::guards_held());
        let start = model.init_states().pop().expect("one init state");

        let launched = model
            .next_state(&start, Action::LaunchAccepted)
            .expect("a launch is accepted");
        let running = model
            .next_state(&launched, Action::HookSucceeded)
            .expect("the hook succeeds");
        assert_eq!(running.vm_state, VmState::Running);
        assert_eq!(running.bootstrap_count, 1);
        assert_eq!(running.installed_token, Some(0));

        // A proxied request warms the cache, so the drop below has a token to drop —
        // without this step the final assertion holds on an always-empty cache and
        // measures nothing.
        let warmed = model
            .next_state(&running, Action::ExecRequested)
            .expect("a proxied request caches a token");
        assert!(warmed.proxy_token_cached, "the request must warm the cache");

        let suspending = model
            .next_state(&warmed, Action::SuspendRequested)
            .expect("a suspend from RUNNING is issued");
        assert_eq!(suspending.wire.suspends, 1);
        let suspended = model
            .next_state(&suspending, Action::SuspendComplete)
            .expect("the platform reports SUSPENDED");
        assert_eq!(suspended.vm_state, VmState::Suspended);

        let resuming = model
            .next_state(&suspended, Action::ResumeRequested { window_open: true })
            .expect("an open window resumes");
        assert_eq!(resuming.wire.resumes, 1);
        let resumed = model
            .next_state(&resuming, Action::ResumeComplete)
            .expect("the platform reports RUNNING");

        assert_eq!(resumed.vm_state, VmState::Running);
        assert_eq!(
            resumed.bootstrap_count, 1,
            "a resume must not re-bootstrap: the in-memory token survived the freeze"
        );
        assert_eq!(
            resumed.wire.payloads, 1,
            "a resume must re-deliver no run-hook payload (STATE-7)"
        );
        assert_eq!(
            resumed.installed_token,
            Some(0),
            "the same token, not a new one"
        );
        assert!(
            !resumed.proxy_token_cached,
            "STATE-8: the cached proxy token must be dropped"
        );
        assert_eq!(resumed.mints, 1);
    }

    /// A suspend from SUSPENDED is refused with no call, which is STATE-5 stated against the
    /// transition rather than searched for in the space.
    #[test]
    fn a_suspend_from_suspended_increments_no_counter() {
        let model = ClientLifecycle::new(Config::guards_held());
        let start = model.init_states().pop().expect("one init state");
        let launched = model
            .next_state(&start, Action::LaunchAccepted)
            .expect("launch");
        let running = model
            .next_state(&launched, Action::HookSucceeded)
            .expect("hook");
        let suspending = model
            .next_state(&running, Action::SuspendRequested)
            .expect("suspend");
        let suspended = model
            .next_state(&suspending, Action::SuspendComplete)
            .expect("suspended");

        let refused = model
            .next_state(&suspended, Action::SuspendRequested)
            .expect("a refusal still records a verdict");
        assert_eq!(
            refused.wire.suspends, suspended.wire.suspends,
            "a refused suspend must reach no wire call"
        );
        assert_eq!(
            refused.last.expect("a verdict").1,
            Verdict::RefusedLocally,
            "and it must be a local refusal rather than a service answer"
        );
        assert_eq!(refused.vm_state, VmState::Suspended, "and nothing moved");
    }

    /// A resume after a terminate is refused with no call. The model's half of the packet's
    /// "fake records ZERO resume wire calls" criterion.
    #[test]
    fn a_resume_after_a_terminate_increments_no_counter() {
        let model = ClientLifecycle::new(Config::guards_held());
        let start = model.init_states().pop().expect("one init state");
        let launched = model
            .next_state(&start, Action::LaunchAccepted)
            .expect("launch");
        let running = model
            .next_state(&launched, Action::HookSucceeded)
            .expect("hook");
        let terminating = model
            .next_state(&running, Action::TerminateRequested)
            .expect("terminate");
        let terminated = model
            .next_state(&terminating, Action::TerminateComplete)
            .expect("terminated");
        assert!(terminated.was_terminated);

        for window_open in [true, false] {
            let refused = model
                .next_state(&terminated, Action::ResumeRequested { window_open })
                .expect("a refusal still records a verdict");
            assert_eq!(
                refused.wire.resumes, 0,
                "window_open={window_open}: no resume may reach the wire once terminated"
            );
            assert_ne!(
                refused.vm_state,
                VmState::Running,
                "window_open={window_open}: a terminated VM must not return to RUNNING"
            );
        }
    }

    /// The state space really is explored rather than cut short by the cycle bound: two
    /// cycles are reachable, which is what the second-cycle window defect needs.
    #[test]
    fn two_suspend_resume_cycles_are_reachable() {
        ClientLifecycle::new(Config::guards_held())
            .checker()
            .spawn_bfs()
            .join()
            .assert_any_discovery("two cycles complete");
    }
}

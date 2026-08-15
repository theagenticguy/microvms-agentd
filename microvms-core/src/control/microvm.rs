// SPDX-License-Identifier: Apache-2.0
//! Launch, the launch wait, the lifecycle calls, and the proxy token (TRAP-5, TRAP-8).
//!
//! # TRAP-5: the payload ceiling is 4096 bytes, inclusive
//!
//! [`RunHookPayload`] cannot hold an over-ceiling value, so the check happens when the
//! payload is *made* rather than when the request is sent — which means every path into
//! `RunMicrovm` is closed by one guard instead of each needing its own.
//!
//! The number is worth stating precisely because the project got it wrong in the dangerous
//! direction. `docs/STRATEGY.md` and `docs/TRUST.md` claimed 16 KB — and so does the
//! service model's own **documentation** string for `runHookPayload`, which reads "Maximum:
//! 16,384 bytes" while its shape `RunMicrovmRequestRunHookPayloadString` says `max: 4096`.
//! So a reader who checks the docs rather than the shape reproduces the bug. 4x wrong in
//! the direction that tells a caller four times as much secret material fits as actually
//! does. Bracketed 2026-08-07 by calling `RunMicrovm` with a deliberately bogus
//! `imageIdentifier` so nothing could be created or billed: **4096 passes, 4097 fails.**
//!
//! Measured in UTF-8 **bytes**, not characters. A payload counted by character length
//! passes while the same value with one multi-byte character in it does not.
//!
//! # TRAP-5's other half: why the payload at all
//!
//! It is the only per-VM secret channel the platform offers, and it is what keeps the agent
//! token out of the shared image snapshot. That is safe because the platform forwards no
//! external traffic until the run hook returns 200, so a per-VM secret delivered at launch
//! wins the first-writer race through the endpoint.
//!
//! The daemon reads it one JSON parse deeper than expected: the platform **wraps** the
//! string, so the hook body is `{"runHookPayload": "{\"agent_token\": \"...\"}"}`.
//!
//! # TRAP-8: a terminal state before RUNNING
//!
//! A VM that reaches a terminal state before RUNNING died during startup, and for a
//! hook-serving daemon that almost always means a lifecycle hook failed. Polling through it
//! wastes minutes and then reports a connection error that hides the cause — and by then
//! the VM is gone, so `stateReason` is the only evidence left. [`ControlPlane::wait_for_running`]
//! fails fast and attaches both the state and the reason.
//!
//! # TRAP-11, again: there is no shell-auth method here
//!
//! [`ControlPlane::mint_auth_token`] calls `CreateMicrovmAuthToken`. There is no sibling for
//! `CreateMicrovmShellAuthToken`, deliberately — see [`super`].

use super::transport::{Call, paths, send_with_retry};
use super::{ControlPlane, RunMicrovmRequest, WaitOpts, ops, timed_out, token};
use crate::error::{Error, ErrorKind};

/// The proxy token's auth header, which is also the key in the `authToken` map.
pub const PROXY_AUTH_HEADER: &str = "X-aws-proxy-auth";

/// The proxy port header.
///
/// Sent on **every** endpoint request alongside the auth header: `X-aws-proxy-auth` without
/// `X-aws-proxy-port` is rejected in a way that reads like a bad token, which is a long
/// detour for a missing header.
pub const PROXY_PORT_HEADER: &str = "X-aws-proxy-port";

/// The service ceiling on a proxy token's life, in minutes.
pub const MAX_TOKEN_MINUTES: u32 = 60;

/// A `runHookPayload` that is known to fit the service ceiling.
///
/// The type is the guard: there is no way to build one over 4096 bytes, so no call site has
/// to remember to check. See the module docs for the measurement and for why the service
/// model's own documentation string disagrees with its shape.
#[derive(Clone, Eq, PartialEq)]
pub struct RunHookPayload(String);

/// Prints the size and nothing else.
///
/// The payload is the platform's only per-VM secret channel — its whole purpose is to carry
/// the agent token — so a derived `Debug` would print that token into every log line that
/// formats one, and into every line that formats the [`RunMicrovmRequest`] holding it.
/// [`RunMicrovmRequest`] keeps its derive: with this impl in place, the derived one prints
/// `RunHookPayload(<N bytes>)` for that field, so the safety is inherited rather than
/// restated. The length is kept because it is the number every TRAP-5 diagnosis needs.
impl std::fmt::Debug for RunHookPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RunHookPayload(<{} bytes>)", self.0.len())
    }
}

impl RunHookPayload {
    /// The payload carrying an agent token, as the daemon expects to find it.
    ///
    /// `{"agent_token": "<token>"}` — and the platform wraps *this whole string* as the
    /// value of `runHookPayload`, which is the extra parse the daemon does.
    ///
    /// Checked even though this function builds the JSON itself, because the token is
    /// caller-supplied: someone passing a JWT or a signed blob rather than a bearer token is
    /// exactly who this catches.
    pub fn for_agent_token(agent_token: &str) -> Result<Self, Error> {
        Self::for_launch(agent_token, &std::collections::HashMap::new())
    }

    /// The payload carrying an agent token and a launch environment.
    ///
    /// `{"agent_token": "<token>", "env": {...}}`, and the platform wraps that whole
    /// string as the value of `runHookPayload` — the extra parse the daemon does.
    /// The daemon applies `env` as the base environment of every subsequent exec,
    /// under the per-request `env`.
    ///
    /// **This is where the 4096-byte refusal fires for a launch env.** The check is
    /// on the serialized payload rather than on the map, because the ceiling is on
    /// the string: JSON escaping, the key names, and the punctuation all count, so a
    /// caller who measured their own values would measure the wrong thing. And it is
    /// local rather than left to the service for the same reason
    /// [`super::super::control::image`]'s `require_workdir` and
    /// `require_matching_from` are local: the service's answer arrives as a
    /// `ValidationException` on a member the caller did not know they were filling,
    /// after the call, and botocore does not enforce it client-side at all (measured;
    /// `docs/PLATFORM.md`). `env` is what makes this reachable in practice — one
    /// bearer token has always fit with room to spare, and a map of credentials does
    /// not.
    ///
    /// The env is omitted from the JSON entirely when it is empty, so a caller who
    /// passes no launch env produces byte-for-byte the payload
    /// [`RunHookPayload::for_agent_token`] always produced. A pinned daemon that has
    /// never heard of `env` therefore sees no change, and the two constructors do not
    /// have two different budgets.
    pub fn for_launch(
        agent_token: &str,
        env: &std::collections::HashMap<String, String>,
    ) -> Result<Self, Error> {
        let payload = if env.is_empty() {
            serde_json::json!({ "agent_token": agent_token })
        } else {
            serde_json::json!({ "agent_token": agent_token, "env": env })
        };
        let text = serde_json::to_string(&payload).map_err(|error| {
            Error::new(
                ErrorKind::Unexpected,
                format!("could not serialize the run-hook payload: {error}"),
            )
        })?;
        // The length is read before the value moves into `new`, so the refusal below can
        // account for it without rebuilding the payload.
        let total = text.len();
        Self::new(text).map_err(|error| {
            if env.is_empty() {
                return error;
            }
            // The generic message names the byte count and the ceiling, which is the
            // whole diagnosis for a token. For a launch env it is half of one: the
            // caller wants to know how much of the budget their env is, because the
            // fix is to move a value out of it rather than to shorten the token.
            Error::invalid_arg(format!(
                "{error} The launch env contributed {} of those bytes across {} variable(s); \
                 the payload's other {} are the token and the JSON framing. A launch env is \
                 for small values a workload needs at startup — move credential-scale \
                 material to PUT /v1/fs/file after bootstrap, or to a role the workload \
                 assumes.",
                env_contribution(agent_token, env),
                env.len(),
                total.saturating_sub(env_contribution(agent_token, env)),
            ))
        })
    }

    /// An arbitrary payload, if it fits.
    ///
    /// Public because the run-hook body is the caller's channel and this crate does not own
    /// what goes in it; the ceiling is the only thing it enforces.
    pub fn new(payload: impl Into<String>) -> Result<Self, Error> {
        let payload = payload.into();
        // Bytes, not characters: the ceiling is on the serialized string, so a payload
        // measured by character count passes while the same value with one multi-byte
        // character in it does not.
        let size = payload.len();
        if size <= crate::constants::MAX_RUN_HOOK_PAYLOAD_BYTES {
            return Ok(Self(payload));
        }
        Err(Error::invalid_arg(format!(
            "runHookPayload is {size} bytes, over the ceiling of {} (service model {}, measured \
             inclusive 2026-08-07: 4096 passes and 4097 fails). This is the only per-VM secret \
             channel the platform offers — one bearer token fits, a cloud credential set does \
             not. Note that docs/STRATEGY.md, docs/TRUST.md, and the service model's own \
             documentation string for this member all claim 16 KB, which is wrong by 4x in the \
             dangerous direction; the shape RunMicrovmRequestRunHookPayloadString is the \
             authority and it states max 4096.",
            crate::constants::MAX_RUN_HOOK_PAYLOAD_BYTES,
            crate::constants::MODEL_API_VERSION,
        )))
    }

    /// The payload's bytes as the wire will carry them.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The size in bytes, which is what the ceiling measures.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the payload is empty. The model's minimum is 0, so an empty payload is legal.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// How many of a launch payload's bytes the env accounts for.
///
/// Measured by difference against the token-only payload rather than by summing key and
/// value lengths, because the ceiling is on the *serialized* string: the `"env":{}`
/// wrapper, the quotes, the commas, and every backslash JSON escaping adds all count
/// against the budget, and a figure that summed the raw values would understate exactly
/// the payloads that are near the limit.
///
/// Only ever called on the refusal path, where the payload has already been built once.
fn env_contribution(agent_token: &str, env: &std::collections::HashMap<String, String>) -> usize {
    let with = serde_json::json!({ "agent_token": agent_token, "env": env }).to_string();
    let without = serde_json::json!({ "agent_token": agent_token }).to_string();
    with.len().saturating_sub(without.len())
}

/// A launched MicroVM, as the service last described it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Microvm {
    pub id: String,
    pub state: String,
    /// The proxy endpoint. Present from the launch response onward.
    pub endpoint: String,
    pub image_arn: String,
    pub image_version: String,
    /// Why the VM is in this state, when the service said. The absence is information —
    /// TRAP-8's message distinguishes "no stateReason" from an empty one.
    pub state_reason: Option<String>,
    /// The idle policy the service reports the VM is running under.
    ///
    /// Carried rather than dropped because it is the platform's own account of the window
    /// that will suspend and then terminate this VM, and it was measured coming back on a
    /// real `GetMicrovm` — see [`ops::IdlePolicy`], which used to claim it does not. The
    /// client's request is what *asked* for a window; this is what the service says it got,
    /// and a caller diagnosing a VM that vanished earlier than expected needs the second.
    ///
    /// `Option` because the model does not mark the member required on either response
    /// shape, so an absent one must parse rather than fail.
    pub idle_policy: Option<ops::IdlePolicy>,
}

impl Microvm {
    /// The reason the service gave, or a phrase saying it gave none.
    ///
    /// A phrase rather than an empty string, because "the service said nothing" and "the
    /// service said nothing useful" are different diagnoses and a caller reading a message
    /// has to be able to tell them apart.
    fn reason(&self) -> &str {
        self.state_reason.as_deref().unwrap_or("no stateReason")
    }
}

impl From<ops::MicrovmResponseWire> for Microvm {
    fn from(wire: ops::MicrovmResponseWire) -> Self {
        Self {
            id: wire.microvm_id,
            state: wire.state,
            endpoint: wire.endpoint,
            image_arn: wire.image_arn,
            image_version: wire.image_version,
            state_reason: wire.state_reason,
            idle_policy: wire.idle_policy,
        }
    }
}

/// The minted proxy token, as a header map (TRAP-7).
///
/// # Why a map rather than a string
///
/// Because that is what the service answers: `authToken` is a `TokenParts` map, and the
/// value at `X-aws-proxy-auth` is the header value. A client that treated the response as a
/// string would send a stringified map and get a rejection that reads like a bad token.
///
/// [`ProxyToken::headers`] always emits **both** headers, which is the other half of the
/// same trap: `X-aws-proxy-auth` without `X-aws-proxy-port` is rejected the same
/// indistinguishable way.
#[derive(Clone, Eq, PartialEq)]
pub struct ProxyToken {
    value: String,
    port: u16,
}

/// Names the headers and the port, never the token value.
///
/// A minted proxy token is a credential, and a derived `Debug` would print it into every
/// log line that formats a [`ProxyToken`], a [`RunMicrovmRequest`] holding one, or an error
/// chain containing either. The sibling type in [`crate::session::proxy`] made this choice
/// first; this is the same rule on the control-plane side of the conversion.
impl std::fmt::Debug for ProxyToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyToken")
            .field("headers", &[PROXY_AUTH_HEADER, PROXY_PORT_HEADER])
            .field("port", &self.port)
            .finish()
    }
}

impl ProxyToken {
    /// Reads the token out of the service's `authToken` map.
    ///
    /// A missing `X-aws-proxy-auth` key is [`ErrorKind::Retryable`] via
    /// [`crate::error::WireKind::AuthTokenMint`]: minting is inside the request retry path
    /// (TRAP-9), and a throttle at minute 30 of a long run must not kill a trial that is
    /// otherwise healthy.
    pub fn from_map(
        map: &std::collections::BTreeMap<String, String>,
        port: u16,
    ) -> Result<Self, Error> {
        let value = map.get(PROXY_AUTH_HEADER).ok_or_else(|| {
            Error::wire(
                crate::error::WireKind::AuthTokenMint,
                format!(
                    "the CreateMicrovmAuthToken response carried no {PROXY_AUTH_HEADER} key. The \
                     authToken member is a map, not a string, and that key is the header value \
                     (keys present: {:?}).",
                    map.keys().collect::<Vec<_>>()
                ),
            )
        })?;
        Ok(Self {
            value: value.clone(),
            port,
        })
    }

    /// Both headers every endpoint request needs.
    ///
    /// Both, always. See the type docs: sending only the auth header is rejected in a way
    /// that reads like a bad token.
    pub fn headers(&self) -> [(&'static str, String); 2] {
        [
            (PROXY_AUTH_HEADER, self.value.clone()),
            (PROXY_PORT_HEADER, self.port.to_string()),
        ]
    }

    /// The raw token value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The port the token was minted for.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl ControlPlane {
    /// Launches a MicroVM.
    ///
    /// TRAP-5 is already closed by [`RunHookPayload`]'s type; the duration range is checked
    /// here, and the connectors are derived from intents (TRAP-4). The `clientToken` is
    /// minted from a label (TRAP-1).
    pub async fn run_microvm(&self, request: RunMicrovmRequest) -> Result<Microvm, Error> {
        super::require_duration_in_range(request.max_duration_sec)?;

        // Ingress and egress go in separate members, so they are split by intent rather
        // than concatenated into one list.
        let ingress: Vec<String> = request
            .connectors
            .iter()
            .filter(|intent| matches!(intent, super::ConnectorIntent::AllIngress))
            .map(|intent| intent.arn(&self.region))
            .collect();
        let egress: Vec<String> = request
            .connectors
            .iter()
            .filter(|intent| matches!(intent, super::ConnectorIntent::Egress))
            .map(|intent| intent.arn(&self.region))
            .collect();

        if ingress.len() + egress.len() > crate::constants::MAX_NETWORK_CONNECTORS {
            return Err(Error::invalid_arg(format!(
                "{} network connectors were requested, over the NetworkConnectorList ceiling of \
                 {} (service model {}).",
                ingress.len() + egress.len(),
                crate::constants::MAX_NETWORK_CONNECTORS,
                crate::constants::MODEL_API_VERSION,
            )));
        }

        let wire = ops::RunMicrovmWire {
            image_identifier: request.image_identifier.clone(),
            execution_role_arn: request.execution_role_arn.clone(),
            ingress_network_connectors: ingress,
            // Absent rather than empty: omitting egress is how you get no outbound network.
            egress_network_connectors: (!egress.is_empty()).then_some(egress),
            idle_policy: ops::IdlePolicy {
                max_idle_duration_seconds: request.max_idle_sec,
                suspended_duration_seconds: request.suspended_sec,
                auto_resume_enabled: request.auto_resume,
            },
            maximum_duration_in_seconds: request.max_duration_sec,
            run_hook_payload: request.run_hook_payload.as_str().to_string(),
            client_token: token::run_token(
                request
                    .token_scope
                    .as_deref()
                    .unwrap_or(&request.image_identifier),
            ),
        };

        let call = Call::post_json("RunMicrovm", paths::microvms(), &wire)?;
        let reply = send_with_retry(self.transport(), call).await?;
        let launched: ops::MicrovmResponseWire = reply.json("RunMicrovm")?;
        Ok(launched.into())
    }

    /// Polls to RUNNING, failing fast on a terminal state (TRAP-8).
    ///
    /// The fast failure is the whole value. A VM that reaches a terminal state before
    /// RUNNING died during startup; polling through it wastes minutes and then reports a
    /// connection error that hides the cause, and by then the VM is gone so `stateReason` is
    /// the only evidence left.
    pub async fn wait_for_running(&self, id: &str, opts: WaitOpts) -> Result<Microvm, Error> {
        self.wait_for_state(id, &["RUNNING"], &crate::constants::TERMINAL_STATES, opts)
            .await
    }

    /// Polls until the VM reaches one of `wanted`, failing fast on any of `fail_on`.
    ///
    /// # Why `fail_on` is a parameter rather than always the terminal set
    ///
    /// Because `suspend` **wants** SUSPENDED and tolerates TERMINATED: a VM that dies while
    /// suspending is a state to report, not an exception to raise out of the middle of a
    /// teardown. The resume path passes the *dead* states instead, because a VM the idle
    /// policy terminated during suspension never reaches RUNNING — and waiting only for
    /// RUNNING there burns the full timeout and then reports "never reached RUNNING", a
    /// timeout message hiding a cause the service had already stated.
    pub async fn wait_for_state(
        &self,
        id: &str,
        wanted: &[&str],
        fail_on: &[&str],
        opts: WaitOpts,
    ) -> Result<Microvm, Error> {
        let started = self.clock().elapsed();
        loop {
            let got = self.get_microvm(id).await?;

            if wanted.contains(&got.state.as_str()) {
                return Ok(got);
            }
            if fail_on.contains(&got.state.as_str()) {
                return Err(self.reached_terminal_state(&got, wanted));
            }

            let elapsed = self.clock().elapsed().saturating_sub(started);
            if elapsed >= opts.timeout {
                return Err(timed_out(
                    &format!(
                        "microvm {id} never reached {wanted:?} (last state {})",
                        got.state
                    ),
                    elapsed,
                ));
            }
            self.clock().sleep(opts.poll_interval).await;
        }
    }

    /// TRAP-8's error: the state **and** the reason, both in the message.
    ///
    /// Both because either alone is unactionable. The state says the VM is gone; the reason
    /// says why, and it is the only evidence that survives the VM.
    fn reached_terminal_state(&self, got: &Microvm, wanted: &[&str]) -> Error {
        Error::new(
            ErrorKind::LaunchDied,
            format!(
                "microvm {} reached {} before {wanted:?}: {}. A VM that reaches a terminal state \
                 before RUNNING died during startup, and for a hook-serving daemon that almost \
                 always means a lifecycle hook failed — the stateReason above is the only evidence \
                 left, because the VM is gone (docs/PLATFORM.md, 'A MicroVM that dies during \
                 startup reports a connection error that hides the cause').",
                got.id,
                got.state,
                got.reason(),
            ),
        )
    }

    /// `GetMicrovm`.
    pub async fn get_microvm(&self, id: &str) -> Result<Microvm, Error> {
        let call = Call::get("GetMicrovm", paths::microvm(id));
        let reply = send_with_retry(self.transport(), call).await?;
        let got: ops::MicrovmResponseWire = reply.json("GetMicrovm")?;
        Ok(got.into())
    }

    /// `ListMicrovms`, read to its last page.
    ///
    /// # Why every page
    ///
    /// `maxResults` caps at 50, so a fleet above 50 VMs read from one page is a
    /// **confidently wrong** answer rather than a missing one: the caller gets a list that
    /// looks complete, and a VM absent from it is a VM nothing here will terminate. That is
    /// the same argument [`ops::ListImagesResponseWire`] makes for the image listing, and a
    /// fleet listing is the one a teardown reads.
    pub async fn list_microvms(&self) -> Result<Vec<ops::MicrovmItemWire>, Error> {
        let mut items = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let call = Call::get("ListMicrovms", paths::microvms_list(next_token.as_deref()));
            let reply = send_with_retry(self.transport(), call).await?;
            let page: ops::ListMicrovmsResponseWire = reply.json("ListMicrovms")?;
            items.extend(page.items);
            match page.next_token {
                Some(token) => next_token = Some(token),
                None => return Ok(items),
            }
        }
    }

    /// `SuspendMicrovm`. Freezes the VM; the caller waits for SUSPENDED.
    pub async fn suspend(&self, id: &str) -> Result<(), Error> {
        let call = Call::post_empty("SuspendMicrovm", paths::suspend(id));
        send_with_retry(self.transport(), call).await?;
        Ok(())
    }

    /// `ResumeMicrovm`. Thaws the VM; no token re-delivery and no re-bootstrap.
    pub async fn resume(&self, id: &str) -> Result<(), Error> {
        let call = Call::post_empty("ResumeMicrovm", paths::resume(id));
        send_with_retry(self.transport(), call).await?;
        Ok(())
    }

    /// `TerminateMicrovm`.
    pub async fn terminate(&self, id: &str) -> Result<(), Error> {
        let call = Call::delete("TerminateMicrovm", paths::microvm(id));
        send_with_retry(self.transport(), call).await?;
        Ok(())
    }

    /// Mints a proxy token for the agent port (TRAP-7, TRAP-9).
    ///
    /// `expirationInMinutes` is capped at [`MAX_TOKEN_MINUTES`] here rather than passed
    /// through, because over-asking is rejected and the rejection reads like a bad request
    /// rather than like a ceiling. The *refresh* interval — half the ceiling — belongs to
    /// the endpoint client that holds the token (T-W2-5), not here.
    pub async fn mint_auth_token(&self, id: &str) -> Result<ProxyToken, Error> {
        let wire = ops::CreateAuthTokenWire {
            expiration_in_minutes: MAX_TOKEN_MINUTES,
            allowed_ports: vec![ops::PortSpecification { port: self.port() }],
        };
        let call = Call::post_json("CreateMicrovmAuthToken", paths::auth_token(id), &wire)?;
        let reply = send_with_retry(self.transport(), call).await?;
        let minted: ops::CreateAuthTokenResponseWire = reply.json("CreateMicrovmAuthToken")?;
        ProxyToken::from_map(&minted.auth_token, self.port())
    }
}

/// The states a suspend may end in.
///
/// TERMINATED is *wanted* rather than a failure: a VM that dies while suspending is a state
/// to report, not an exception to raise out of the middle of a teardown.
pub const SUSPEND_WANTED: [&str; 2] = ["SUSPENDED", "TERMINATED"];

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::fake::{self as fake, Answer, FakeControlPlane, TestClock};
    use super::*;
    use crate::region::Region;

    fn planted() -> (ControlPlane, Arc<FakeControlPlane>, Arc<TestClock>) {
        let fake = Arc::new(FakeControlPlane::new());
        let clock = Arc::new(TestClock::new());
        let plane = ControlPlane::with_transport(fake.clone(), Region::UsEast1, clock.clone());
        (plane, fake, clock)
    }

    /// **TRAP-5, the boundary.** 4096 bytes passes, 4097 is rejected — inclusive, as
    /// measured 2026-08-07.
    ///
    /// **Falsification** — change the comparison to `<` and the 4096 case fails; change the
    /// ceiling to 16384 (the number the docs and the model's own documentation string claim)
    /// and the 4097 case fails.
    #[test]
    fn the_payload_ceiling_is_four_thousand_ninety_six_bytes_inclusive() {
        let at_ceiling = "a".repeat(4096);
        let payload = RunHookPayload::new(at_ceiling.clone()).expect("4096 bytes fits");
        assert_eq!(payload.len(), 4096);
        assert_eq!(payload.as_str(), at_ceiling);

        let over = "a".repeat(4097);
        let error = RunHookPayload::new(over).expect_err("4097 bytes does not fit");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        let message = error.to_string();
        assert!(message.contains("4097 bytes"), "{message}");
        assert!(
            message.contains("ceiling of 4096"),
            "the message must name the service-model ceiling: {message}"
        );
        assert!(message.contains("2025-09-09"), "{message}");
        assert!(
            message.contains("wrong by 4x"),
            "the 16 KB claim has to be named as wrong: {message}"
        );
    }

    /// **TRAP-5, the local-refusal half.** An over-ceiling payload means the fake records
    /// **zero** calls: the launch is refused before any control-plane call.
    #[tokio::test]
    async fn an_over_ceiling_payload_reaches_no_control_plane_call() {
        let (plane, fake, _) = planted();

        // The payload type refuses first, so there is no way to even build the request.
        let refused = RunHookPayload::new("a".repeat(4097));
        assert!(refused.is_err());
        assert_eq!(fake.calls().len(), 0);

        // And the at-ceiling one does launch, so the guard is not simply refusing
        // everything.
        fake.answer(
            "RunMicrovm",
            Answer::ok(fake::microvm_response("PENDING", None)),
        );
        let payload = RunHookPayload::new("a".repeat(4096)).expect("fits");
        plane
            .run_microvm(RunMicrovmRequest::new("arn:image", payload))
            .await
            .expect("launches");
        assert_eq!(fake.call_count("RunMicrovm"), 1);
    }

    /// The ceiling is measured in **bytes**, not characters. A payload of 2049 two-byte
    /// characters is 4098 bytes and must be refused even though it is 2049 characters.
    #[test]
    fn the_ceiling_counts_utf8_bytes_rather_than_characters() {
        let two_byte_chars = "é".repeat(2049);
        assert_eq!(
            two_byte_chars.chars().count(),
            2049,
            "well under 4096 chars"
        );
        assert_eq!(two_byte_chars.len(), 4098, "but over 4096 bytes");

        let error = RunHookPayload::new(two_byte_chars).expect_err("bytes are what count");
        assert!(error.to_string().contains("4098 bytes"), "{error}");

        // 2048 of them is exactly 4096 bytes, which fits.
        let exact = "é".repeat(2048);
        assert_eq!(exact.len(), 4096);
        RunHookPayload::new(exact).expect("exactly at the ceiling");
    }

    /// The agent-token payload is the shape the daemon parses, and an oversized token is
    /// caught even though the client builds the JSON — because the token is caller-supplied.
    #[test]
    fn the_agent_token_payload_is_checked_even_though_the_client_builds_it() {
        let payload = RunHookPayload::for_agent_token("bearer-token-value").expect("fits");
        assert_eq!(payload.as_str(), r#"{"agent_token":"bearer-token-value"}"#);

        // A caller passing a credential bundle rather than a bearer token is who this
        // catches.
        let huge = "x".repeat(4096);
        let error = RunHookPayload::for_agent_token(&huge)
            .expect_err("the JSON wrapper pushes it over the ceiling");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        assert!(
            error.to_string().contains("one bearer token fits"),
            "{error}"
        );
    }

    /// An empty launch env produces byte-for-byte the payload the token-only
    /// constructor always produced, so a caller who never passes one is unaffected by
    /// the field existing — and a daemon baked into an older image sees no new key.
    ///
    /// **Falsification** — always emit `"env"` and this fails on both assertions.
    #[test]
    fn an_empty_launch_env_emits_the_payload_the_token_only_constructor_did() {
        let with_empty = RunHookPayload::for_launch("tok", &std::collections::HashMap::new())
            .expect("an empty env fits");
        let token_only = RunHookPayload::for_agent_token("tok").expect("fits");
        assert_eq!(with_empty.as_str(), token_only.as_str());
        assert_eq!(with_empty.as_str(), r#"{"agent_token":"tok"}"#);
        assert!(
            !with_empty.as_str().contains("env"),
            "an empty env must not appear on the wire at all: {}",
            with_empty.as_str()
        );
    }

    /// A launch env is carried in the payload the daemon parses.
    #[test]
    fn a_launch_env_rides_in_the_payload_the_daemon_parses() {
        let mut env = std::collections::HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_string(), "https://x".to_string());
        let payload = RunHookPayload::for_launch("tok", &env).expect("fits");

        // Parsed rather than string-compared, since a HashMap has no key order.
        let value: serde_json::Value =
            serde_json::from_str(payload.as_str()).expect("the payload is JSON");
        assert_eq!(value["agent_token"], "tok");
        assert_eq!(value["env"]["ANTHROPIC_BASE_URL"], "https://x");
    }

    /// **The local 4096-byte refusal for a launch env.**
    ///
    /// The reason this is the case worth its own test rather than a repeat of the
    /// ceiling test above: one bearer token has always fit with room to spare, so
    /// before `env` existed the ceiling was effectively unreachable through the typed
    /// constructor. A launch env is what makes it reachable, and botocore does not
    /// enforce it client-side — the oversized request goes to the wire and comes back
    /// as a `ValidationException` on a member the caller did not know they were
    /// filling (`docs/PLATFORM.md`). So this refusal is the only local signal there is.
    ///
    /// The message must carry the byte count, the ceiling, and the env's share of it.
    /// Without the share, a caller reading "4128 bytes, ceiling 4096" cannot tell
    /// whether to shorten the token or drop a variable.
    ///
    /// **Falsification** — return `Ok` from `for_launch` without calling `new`, or drop
    /// the byte-count clause from the message, and this goes red on the specific
    /// assertion.
    #[test]
    fn an_over_ceiling_launch_env_is_refused_locally_with_the_byte_count() {
        let mut env = std::collections::HashMap::new();
        // Comfortably over on its own, the way a set of session credentials is.
        env.insert("CREDENTIALS".to_string(), "c".repeat(4096));

        // The expected figure is derived from the same serialization the constructor
        // does rather than written as a literal, because a literal here asserts my
        // arithmetic about JSON framing and the point is the *count*, not the framing.
        let expected = serde_json::json!({ "agent_token": "tok", "env": &env })
            .to_string()
            .len();
        assert!(
            expected > 4096,
            "the fixture must actually be over: {expected}"
        );

        let error = RunHookPayload::for_launch("tok", &env)
            .expect_err("a credential-scale launch env does not fit");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        let message = error.to_string();

        assert!(
            message.contains("ceiling of 4096"),
            "the ceiling has to be named: {message}"
        );
        assert!(
            message.contains(&format!("{expected} bytes")),
            "the actual byte count has to be named: {message}"
        );
        assert!(
            message.contains("launch env contributed"),
            "the env's share of the budget is the actionable half: {message}"
        );
        assert!(
            message.contains("1 variable(s)"),
            "the variable count has to be named: {message}"
        );
    }

    /// The refusal fires on the *serialized* payload, so a caller who measured their own
    /// values would be measuring the wrong thing.
    ///
    /// Two variables whose raw bytes sum to 4060 — under the ceiling — go over it once
    /// the JSON framing, the key names, and the quotes are counted. A check on the map
    /// rather than on the string would accept this and the launch would fail at AWS.
    #[test]
    fn the_launch_env_ceiling_counts_the_serialized_payload_and_not_the_raw_values() {
        let mut env = std::collections::HashMap::new();
        env.insert("A".to_string(), "a".repeat(2030));
        env.insert("B".to_string(), "b".repeat(2030));
        let raw: usize = env.values().map(String::len).sum();
        assert_eq!(raw, 4060, "the raw values are under the ceiling");
        assert!(
            raw <= crate::constants::MAX_RUN_HOOK_PAYLOAD_BYTES,
            "the fixture only means something if the raw sum fits: {raw}"
        );

        let error = RunHookPayload::for_launch("tok", &env)
            .expect_err("the serialized payload is over even though the raw values are not");
        assert!(error.to_string().contains("2 variable(s)"), "{error}");

        // And the boundary is still inclusive with an env in play: trimmed to fit, the
        // same shape succeeds — so the refusal is a ceiling and not a blanket ban.
        let mut fits = std::collections::HashMap::new();
        fits.insert("A".to_string(), "a".repeat(2000));
        fits.insert("B".to_string(), "b".repeat(2000));
        let payload = RunHookPayload::for_launch("tok", &fits).expect("4000-odd bytes fits");
        assert!(payload.len() <= 4096, "{}", payload.len());
    }

    /// An empty payload is legal: the model's minimum is 0.
    #[test]
    fn an_empty_payload_is_legal() {
        let payload = RunHookPayload::new("").expect("min is 0");
        assert!(payload.is_empty());
        assert_eq!(payload.len(), 0);
    }

    /// The launch request lands on the model's path with the model's member names, and the
    /// connectors are derived ARNs split across the two members (TRAP-4).
    #[tokio::test]
    async fn the_launch_emits_derived_connector_arns_in_the_right_members() {
        let (plane, fake, _) = planted();
        fake.answer(
            "RunMicrovm",
            Answer::ok(fake::microvm_response("PENDING", None)),
        );

        let payload = RunHookPayload::for_agent_token("token").expect("fits");
        let request = RunMicrovmRequest::new("arn:image", payload).with_egress();
        plane.run_microvm(request).await.expect("launches");

        let calls = fake.calls();
        assert_eq!(calls[0].path, "/2025-09-09/microvms");
        assert_eq!(calls[0].method, super::super::transport::Method::Post);

        let body = fake.first_body("RunMicrovm");
        assert_eq!(
            body["ingressNetworkConnectors"],
            serde_json::json!([
                "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:ALL_INGRESS"
            ])
        );
        assert_eq!(
            body["egressNetworkConnectors"],
            serde_json::json!([
                "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:INTERNET_EGRESS"
            ])
        );
        assert_eq!(body["runHookPayload"], r#"{"agent_token":"token"}"#);
        assert_eq!(body["maximumDurationInSeconds"], 3600);
    }

    /// Without egress, the member is **absent** — which is how you get a VM with no
    /// outbound network.
    #[tokio::test]
    async fn a_launch_without_egress_omits_the_member_entirely() {
        let (plane, fake, _) = planted();
        fake.answer(
            "RunMicrovm",
            Answer::ok(fake::microvm_response("PENDING", None)),
        );
        let payload = RunHookPayload::for_agent_token("token").expect("fits");
        plane
            .run_microvm(RunMicrovmRequest::new("arn:image", payload))
            .await
            .expect("launches");

        let body = fake.first_body("RunMicrovm");
        assert!(
            body.get("egressNetworkConnectors").is_none(),
            "omitting egress is the whole mechanism: {body}"
        );
        assert!(body.get("ingressNetworkConnectors").is_some());
    }

    /// The connectors reaching the wire are ARNs, never bare names — a bare name is rejected
    /// with "Malformed network connector ARN", and it is the value that reads most natural
    /// to write.
    #[tokio::test]
    async fn no_bare_connector_name_reaches_the_wire() {
        let (plane, fake, _) = planted();
        fake.answer(
            "RunMicrovm",
            Answer::ok(fake::microvm_response("PENDING", None)),
        );
        let payload = RunHookPayload::for_agent_token("token").expect("fits");
        plane
            .run_microvm(RunMicrovmRequest::new("arn:image", payload).with_egress())
            .await
            .expect("launches");

        let body = fake.first_body("RunMicrovm");
        for member in ["ingressNetworkConnectors", "egressNetworkConnectors"] {
            for value in body[member].as_array().expect("a list") {
                let text = value.as_str().expect("a string");
                assert!(text.starts_with("arn:aws:lambda:"), "{member}: {text}");
                assert_ne!(text, "ALL_INGRESS");
                assert_ne!(text, "INTERNET_EGRESS");
            }
        }
    }

    /// An out-of-range duration is refused before any call.
    #[tokio::test]
    async fn an_out_of_range_duration_reaches_no_control_plane_call() {
        let (plane, fake, _) = planted();
        let payload = RunHookPayload::for_agent_token("token").expect("fits");
        let mut request = RunMicrovmRequest::new("arn:image", payload);
        request.max_duration_sec = 28_801;

        let error = plane
            .run_microvm(request)
            .await
            .expect_err("over the ceiling");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        assert_eq!(fake.calls().len(), 0);
    }

    /// **TRAP-8.** A VM that reaches a terminal state before RUNNING is rejected with the
    /// state **and** the `stateReason` in the message.
    ///
    /// The message is asserted rather than merely the error, because a timeout also produces
    /// an error — so "an error occurred" would pass against the exact defect this closes.
    ///
    /// **Falsification** — remove the `fail_on` branch from `wait_for_state` and this test
    /// fails with an `ErrorKind::Timeout` whose message names neither the state nor the
    /// reason. Verified; see the packet's guard proofs.
    #[tokio::test]
    async fn a_terminal_state_before_running_carries_both_the_state_and_the_reason() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovm",
            Answer::ok(fake::microvm_response(
                "TERMINATED",
                Some("run hook returned 500"),
            )),
        );

        let error = plane
            .wait_for_running("mvm-abc123", WaitOpts::for_launch())
            .await
            .expect_err("a dead VM must not be waited out");

        assert_eq!(error.kind(), ErrorKind::LaunchDied);
        assert_eq!(error.code(), "ERR_LAUNCH_DIED");
        let message = error.to_string();
        assert!(
            message.contains("TERMINATED"),
            "the state must be attached: {message}"
        );
        assert!(
            message.contains("run hook returned 500"),
            "the stateReason must be attached: {message}"
        );
        assert!(
            message.contains("lifecycle hook failed"),
            "the likely cause is worth naming: {message}"
        );
        assert!(
            fake.call_count("GetMicrovm") == 1,
            "it fails fast rather than polling to the deadline"
        );
    }

    /// Every terminal state fails the launch fast, not just TERMINATED. SUSPENDED reached
    /// before RUNNING is also a death — the VM never came up.
    ///
    /// The states are read from `constants::TERMINAL_STATES` rather than listed, so a state
    /// added there is covered here without a second edit — and the count is asserted so the
    /// loop cannot silently become empty.
    #[tokio::test]
    async fn every_terminal_state_fails_a_launch_fast() {
        assert_eq!(crate::constants::TERMINAL_STATES.len(), 4);
        for state in crate::constants::TERMINAL_STATES {
            let (plane, fake, _) = planted();
            fake.answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response(state, Some("hook timeout"))),
            );
            let error = plane
                .wait_for_running("mvm-abc123", WaitOpts::for_launch())
                .await
                .expect_err(&format!("{state} before RUNNING is a death"));
            assert_eq!(error.kind(), ErrorKind::LaunchDied, "{state}");
            assert!(error.to_string().contains(state), "{state}: {error}");
            assert!(
                error.to_string().contains("hook timeout"),
                "{state}: {error}"
            );
            assert_eq!(
                fake.call_count("GetMicrovm"),
                1,
                "{state} must fail fast rather than polling to the deadline"
            );
        }
    }

    /// A terminal state with **no** `stateReason` says so rather than printing an empty
    /// string: "the service said nothing" is a different diagnosis from "the service said
    /// nothing useful".
    #[tokio::test]
    async fn a_missing_state_reason_is_named_rather_than_printed_empty() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovm",
            Answer::ok(fake::microvm_response("TERMINATED", None)),
        );

        let error = plane
            .wait_for_running("mvm-abc123", WaitOpts::for_launch())
            .await
            .expect_err("still a death");
        let message = error.to_string();
        assert!(message.contains("no stateReason"), "{message}");
        // The failure this guards against is an absent reason rendering as an empty string,
        // which reads as the service having answered with nothing rather than not having
        // answered. `: .` is what that looks like in this message's shape.
        assert!(
            !message.contains(r#"["RUNNING"]: ."#),
            "an absent reason must not render as an empty string: {message}"
        );
        assert_eq!(error.kind(), ErrorKind::LaunchDied);
    }

    /// The wait returns on RUNNING, having polled through PENDING.
    #[tokio::test]
    async fn the_launch_wait_polls_through_pending_and_returns_on_running() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovm",
            Answer::ok(fake::microvm_response("PENDING", None)),
        )
        .answer(
            "GetMicrovm",
            Answer::ok(fake::microvm_response("RUNNING", None)),
        );

        let vm = plane
            .wait_for_running("mvm-abc123", WaitOpts::for_launch())
            .await
            .expect("reaches RUNNING");
        assert_eq!(vm.state, "RUNNING");
        assert_eq!(
            vm.endpoint,
            "https://mvm-abc123.microvm.us-east-1.amazonaws.com"
        );
        assert_eq!(fake.call_count("GetMicrovm"), 2);
    }

    /// A launch that never comes up ends at its deadline with a `Timeout` — which is a
    /// *different* error from TRAP-8's, and the distinction is the whole reason the TRAP-8
    /// test asserts on the message.
    #[tokio::test]
    async fn a_launch_that_never_comes_up_times_out_rather_than_reporting_a_death() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovm",
            Answer::ok(fake::microvm_response("PENDING", None)),
        );

        let error = plane
            .wait_for_running("mvm-abc123", WaitOpts::for_launch())
            .await
            .expect_err("the deadline elapses");
        assert_eq!(error.kind(), ErrorKind::Timeout);
        assert_ne!(
            error.kind(),
            ErrorKind::LaunchDied,
            "a timeout is not a death, and conflating them is what hid the cause"
        );
        assert!(error.to_string().contains("PENDING"), "{error}");
    }

    /// The resume path fails fast on the **dead** states and waits through SUSPENDED —
    /// which is the state a resume is called from, so failing on it would fail every resume.
    #[tokio::test]
    async fn the_resume_wait_tolerates_suspended_and_fails_on_terminated() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovm",
            Answer::ok(fake::microvm_response("SUSPENDED", None)),
        )
        .answer(
            "GetMicrovm",
            Answer::ok(fake::microvm_response("RUNNING", None)),
        );

        let vm = plane
            .wait_for_state(
                "mvm-abc123",
                &["RUNNING"],
                &crate::constants::DEAD_STATES,
                WaitOpts::for_launch(),
            )
            .await
            .expect("SUSPENDED is a waypoint on the resume path");
        assert_eq!(vm.state, "RUNNING");

        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovm",
            Answer::ok(fake::microvm_response(
                "TERMINATED",
                Some("suspended window elapsed"),
            )),
        );
        let error = plane
            .wait_for_state(
                "mvm-abc123",
                &["RUNNING"],
                &crate::constants::DEAD_STATES,
                WaitOpts::for_launch(),
            )
            .await
            .expect_err("a terminated VM never reaches RUNNING");
        assert_eq!(error.kind(), ErrorKind::LaunchDied);
        assert!(
            error.to_string().contains("suspended window elapsed"),
            "{error}"
        );
    }

    /// A suspend **wants** TERMINATED as well as SUSPENDED: a VM that dies while suspending
    /// is a state to report, not an exception out of a teardown.
    #[tokio::test]
    async fn a_suspend_treats_terminated_as_a_state_to_report() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovm",
            Answer::ok(fake::microvm_response("TERMINATED", None)),
        );

        let vm = plane
            .wait_for_state("mvm-abc123", &SUSPEND_WANTED, &[], WaitOpts::for_launch())
            .await
            .expect("a suspend reports the state it reached");
        assert_eq!(vm.state, "TERMINATED");
    }

    /// The lifecycle calls land on the model's paths and methods. `POST` for suspend and
    /// resume, `DELETE` for terminate — transcribed from each operation's `http` trait.
    #[tokio::test]
    async fn the_lifecycle_calls_use_the_models_paths_and_methods() {
        let (plane, fake, _) = planted();
        fake.answer("SuspendMicrovm", Answer::ok(fake::empty_response()))
            .answer("ResumeMicrovm", Answer::ok(fake::empty_response()))
            .answer("TerminateMicrovm", Answer::ok(fake::empty_response()));

        plane.suspend("mvm-1").await.expect("suspends");
        plane.resume("mvm-1").await.expect("resumes");
        plane.terminate("mvm-1").await.expect("terminates");

        let calls = fake.calls();
        use super::super::transport::Method;
        assert_eq!(
            calls
                .iter()
                .map(|call| (call.operation, call.method, call.path.as_str()))
                .collect::<Vec<_>>(),
            [
                (
                    "SuspendMicrovm",
                    Method::Post,
                    "/2025-09-09/microvms/mvm-1/suspend"
                ),
                (
                    "ResumeMicrovm",
                    Method::Post,
                    "/2025-09-09/microvms/mvm-1/resume"
                ),
                (
                    "TerminateMicrovm",
                    Method::Delete,
                    "/2025-09-09/microvms/mvm-1"
                ),
            ]
        );
    }

    /// **TRAP-7.** The minted token is read out of the `authToken` **map**, and
    /// [`ProxyToken::headers`] emits both headers.
    #[tokio::test]
    async fn the_minted_token_comes_from_the_map_and_carries_both_headers() {
        let (plane, fake, _) = planted();
        fake.answer(
            "CreateMicrovmAuthToken",
            Answer::ok(fake::auth_token_response("opaque-proxy-token")),
        );

        let token = plane.mint_auth_token("mvm-1").await.expect("mints");
        assert_eq!(token.value(), "opaque-proxy-token");
        assert_eq!(token.port(), 9000);

        let headers = token.headers();
        assert_eq!(
            headers[0],
            ("X-aws-proxy-auth", "opaque-proxy-token".to_string())
        );
        assert_eq!(
            headers[1],
            ("X-aws-proxy-port", "9000".to_string()),
            "the auth header without the port header is rejected in a way that reads like a \
             bad token"
        );

        let body = fake.first_body("CreateMicrovmAuthToken");
        assert_eq!(body["expirationInMinutes"], 60);
        assert_eq!(body["allowedPorts"][0]["port"], 9000);
    }

    /// A launch request's `Debug` does not print the agent token.
    ///
    /// Asserted on the whole [`RunMicrovmRequest`] rather than on the payload alone,
    /// because that is the type a caller actually logs — and it keeps its derived `Debug`,
    /// so what is pinned here is that the derive inherits [`RunHookPayload`]'s hand-written
    /// one. **Falsification** — restore `#[derive(Debug)]` on [`RunHookPayload`] and the
    /// `SECRETTOKEN` assertion fails through the derived request `Debug`.
    #[test]
    fn a_launch_requests_debug_does_not_print_the_agent_token() {
        let payload = RunHookPayload::for_agent_token("SECRETTOKEN").expect("fits");

        let rendered = format!("{payload:?}");
        assert!(
            !rendered.contains("SECRETTOKEN"),
            "the agent token reached a Debug string: {rendered}"
        );
        assert!(
            rendered.contains(&format!("{} bytes", payload.len())),
            "the size is what a TRAP-5 diagnosis needs: {rendered}"
        );

        let request = RunMicrovmRequest::new("arn:image", payload);
        let rendered = format!("{request:?}");
        assert!(rendered.contains("arn:image"), "{rendered}");
        assert!(
            !rendered.contains("SECRETTOKEN"),
            "the agent token reached a Debug string through the request: {rendered}"
        );
    }

    /// A minted token's `Debug` names its headers and never its value, so a logged
    /// launch does not log a credential.
    ///
    /// The mirror of `session::proxy`'s `a_proxy_token_debug_does_not_print_the_credential`,
    /// on the control-plane side of the conversion. **Falsification** — restore
    /// `#[derive(Debug)]` on [`ProxyToken`] and this fails on the `secret` assertion.
    #[test]
    fn a_proxy_token_debug_does_not_print_the_credential() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            PROXY_AUTH_HEADER.to_string(),
            "eyJhbGciOi-secret".to_string(),
        );
        let token = ProxyToken::from_map(&map, 9000).expect("the map has the auth key");

        let rendered = format!("{token:?}");
        assert!(rendered.contains(PROXY_AUTH_HEADER), "{rendered}");
        assert!(rendered.contains("9000"), "{rendered}");
        assert!(
            !rendered.contains("secret"),
            "the token value reached a Debug string: {rendered}"
        );
    }

    /// A response whose map lacks the auth key is **retryable**, because minting happens
    /// inside the request retry path and a throttle mid-run must not kill a healthy trial.
    #[test]
    fn a_map_missing_the_auth_key_is_a_retryable_mint_failure() {
        let mut map = std::collections::BTreeMap::new();
        map.insert("SomeOtherKey".to_string(), "value".to_string());

        let error = ProxyToken::from_map(&map, 9000).expect_err("no auth key");
        assert_eq!(
            error.wire_kind(),
            Some(crate::error::WireKind::AuthTokenMint)
        );
        assert!(error.retryable(), "minting is inside the retry path");
        assert!(
            error.to_string().contains("is a map, not a string"),
            "{error}"
        );
    }

    /// The token is minted for the configured port, not always 9000.
    #[tokio::test]
    async fn the_token_is_minted_for_the_configured_port() {
        let fake = Arc::new(FakeControlPlane::new());
        let plane =
            ControlPlane::with_transport(fake.clone(), Region::UsEast1, Arc::new(TestClock::new()))
                .with_port(8080);
        fake.answer(
            "CreateMicrovmAuthToken",
            Answer::ok(fake::auth_token_response("t")),
        );

        let token = plane.mint_auth_token("mvm-1").await.expect("mints");
        assert_eq!(token.port(), 8080);
        assert_eq!(
            fake.first_body("CreateMicrovmAuthToken")["allowedPorts"][0]["port"],
            8080
        );
    }

    /// **TRAP-11, across a full lifecycle.** Drive create, wait, launch, wait, mint, suspend,
    /// resume, terminate, delete — and then assert the recorder saw **zero** shell-auth calls
    /// and **zero** `SHELL_INGRESS` anywhere.
    ///
    /// A count rather than a refusal, because the closure is an absence: there is no method
    /// to call and no intent to name, so there is nothing that *rejects* a shell request.
    /// The only honest assertion is that a client doing everything it can do never emits one.
    ///
    /// Three independent scans, because a single one would be easy to satisfy accidentally:
    /// the operation names (a method added later), the request paths (a route reached under a
    /// different operation name), and the raw request bodies (a connector value however it
    /// got there).
    ///
    /// **Falsification** — add a `mint_shell_auth_token` calling
    /// `POST /2025-09-09/microvms/{id}/shell-auth-token` and call it here: the operation
    /// scan and the path scan both fail. Add `ConnectorIntent::ShellIngress` and request it:
    /// the body scan fails.
    #[tokio::test]
    async fn a_full_lifecycle_never_calls_shell_auth_or_requests_shell_ingress() {
        let (plane, fake, _) = planted();
        fake.answer(
            "CreateMicrovmImage",
            Answer::created(fake::create_image_response("img")),
        )
        .answer(
            "GetMicrovmImage",
            Answer::ok(fake::get_image_response("img", "CREATED")),
        )
        .answer(
            "RunMicrovm",
            Answer::ok(fake::microvm_response("PENDING", None)),
        )
        .answer(
            "GetMicrovm",
            Answer::ok(fake::microvm_response("RUNNING", None)),
        )
        .answer(
            "CreateMicrovmAuthToken",
            Answer::ok(fake::auth_token_response("proxy-token")),
        )
        .answer("SuspendMicrovm", Answer::ok(fake::empty_response()))
        .answer("ResumeMicrovm", Answer::ok(fake::empty_response()))
        .answer("TerminateMicrovm", Answer::ok(fake::empty_response()))
        .answer(
            "ListMicrovmImageVersions",
            Answer::ok(fake::list_versions_response("1")),
        )
        .answer(
            "DeleteMicrovmImage",
            Answer::ok(fake::delete_image_response()),
        );

        // Everything this client can do, in the order a real run does it.
        let request = super::super::CreateImageRequest::new(
            "img",
            b"binary".to_vec(),
            "s3://bucket/img.zip",
            "arn:aws:iam::123456789012:role/build",
        );
        let image = plane.create_image(request).await.expect("creates");
        plane
            .wait_for_image(&image.identifier, image.size, WaitOpts::default())
            .await
            .expect("becomes usable");

        let payload = RunHookPayload::for_agent_token("agent-token").expect("fits");
        let vm = plane
            .run_microvm(
                super::super::RunMicrovmRequest::new(&image.identifier, payload).with_egress(),
            )
            .await
            .expect("launches");
        plane
            .wait_for_running(&vm.id, WaitOpts::for_launch())
            .await
            .expect("reaches RUNNING");
        plane.mint_auth_token(&vm.id).await.expect("mints");
        plane.suspend(&vm.id).await.expect("suspends");
        plane.resume(&vm.id).await.expect("resumes");
        plane.terminate(&vm.id).await.expect("terminates");
        plane
            .delete_image(&image.identifier, 1, std::time::Duration::from_secs(1))
            .await;

        // The lifecycle really ran, so the zero counts below are about absence rather than
        // about nothing having happened.
        let operations = fake.operations();
        assert!(
            operations.len() >= 10,
            "the lifecycle must actually have run: {operations:?}"
        );
        assert!(operations.contains(&"CreateMicrovmAuthToken"));

        assert_eq!(
            fake.call_count("CreateMicrovmShellAuthToken"),
            0,
            "shell auth gates a debug console, not programmatic exec: {operations:?}"
        );
        for operation in &operations {
            assert!(
                !operation.contains("Shell"),
                "no operation may be a shell one: {operation}"
            );
        }
        for path in fake.paths() {
            assert!(
                !path.contains("shell"),
                "no request may reach a shell route: {path}"
            );
        }
        for body in fake.bodies_as_text() {
            assert!(
                !body.contains("SHELL_INGRESS"),
                "no request may name the shell connector: {body}"
            );
            assert!(!body.contains("SHELL"), "{body}");
        }
    }

    /// **Issue #23.** The fleet listing follows `nextToken`, so a fleet larger than one page
    /// is not silently truncated.
    ///
    /// One page is a **confidently wrong** answer rather than a missing one: the caller gets a
    /// list that looks complete, and a VM absent from it is a VM nothing here will terminate.
    ///
    /// The assertion is the fleet **length** and the presence of the page-two ids, which is
    /// what a test against the single-page code cannot satisfy.
    ///
    /// **Falsification** — run 2026-08-15. Replace the `Some(token)` arm with
    /// `Some(_) => return Ok(items)` and this fails with 2 VMs instead of 3, and both
    /// page-two `contains` assertions go red. Restored.
    #[tokio::test]
    async fn the_fleet_listing_follows_next_token_to_the_vms_on_page_two() {
        let (plane, fake, _) = planted();
        fake.answer(
            "ListMicrovms",
            Answer::ok(fake::list_microvms_page(
                &["mvm-page1-a", "mvm-page1-b"],
                Some("fleet-page-2"),
            )),
        )
        .answer(
            "ListMicrovms",
            Answer::ok(fake::list_microvms_page(&["mvm-page2-a"], None)),
        );

        let fleet = plane.list_microvms().await.expect("lists");
        assert_eq!(
            fleet.len(),
            3,
            "a fleet spanning two pages is three VMs, not two"
        );

        let ids: Vec<&str> = fleet.iter().map(|vm| vm.microvm_id.as_str()).collect();
        assert!(ids.contains(&"mvm-page2-a"), "page two is missing: {ids:?}");
        assert!(ids.contains(&"mvm-page1-a"), "{ids:?}");
        assert_eq!(fake.call_count("ListMicrovms"), 2, "both pages were read");

        let paths = fake.paths();
        assert!(
            paths[1].contains("nextToken=fleet-page-2"),
            "the second request must carry the first page's token: {}",
            paths[1]
        );
        assert!(
            !paths[0].contains('?'),
            "the first request carries no cursor: {}",
            paths[0]
        );
    }

    /// **Issue #25.** The `idlePolicy` the service reports is carried rather than dropped.
    ///
    /// The comment on [`ops::IdlePolicy`] used to claim `suspendedDurationSeconds` "exists
    /// only in the request". A live `GetMicrovm` on a RUNNING VM disagrees, which is what the
    /// fake's body now reflects — so this test is over a response shape that was measured
    /// rather than assumed.
    ///
    /// **Falsification** — drop `idle_policy` from the `From<MicrovmResponseWire>` impl and
    /// this does not compile; set it to `None` there and every field assertion goes red.
    #[tokio::test]
    async fn the_reported_idle_policy_is_carried_rather_than_dropped() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovm",
            Answer::ok(fake::microvm_response("RUNNING", None)),
        );

        let vm = plane.get_microvm("mvm-1").await.expect("reads");
        let policy = vm
            .idle_policy
            .expect("GetMicrovm returns an idlePolicy, measured 2026-08-15");
        assert_eq!(policy.max_idle_duration_seconds, 1800);
        assert_eq!(
            policy.suspended_duration_seconds, 600,
            "the member the comment claimed was request-only"
        );
        assert!(!policy.auto_resume_enabled);
    }

    /// A response with no `idlePolicy` still parses, and the absence is `None` rather than a
    /// default. The model does not mark the member required on either response shape, so an
    /// invented `0` would be a window nobody configured.
    #[tokio::test]
    async fn an_absent_idle_policy_is_none_rather_than_a_zero_window() {
        let (plane, fake, _) = planted();
        fake.answer(
            "GetMicrovm",
            Answer::ok(
                r#"{
                    "microvmId": "mvm-1",
                    "state": "RUNNING",
                    "endpoint": "https://e",
                    "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                    "imageVersion": "1"
                }"#,
            ),
        );

        let vm = plane.get_microvm("mvm-1").await.expect("parses");
        assert_eq!(vm.idle_policy, None);
    }

    /// A transport failure is retried and then succeeds, so a connection reset on the way to
    /// a launch is not a failed launch.
    #[tokio::test]
    async fn a_transport_failure_is_retried_rather_than_reported() {
        let (plane, fake, _) = planted();
        fake.fail_transport("GetMicrovm", 2).answer(
            "GetMicrovm",
            Answer::ok(fake::microvm_response("RUNNING", None)),
        );

        let vm = plane
            .get_microvm("mvm-1")
            .await
            .expect("the retry succeeds");
        assert_eq!(vm.state, "RUNNING");
        assert_eq!(
            fake.call_count("GetMicrovm"),
            3,
            "two failures then a success"
        );
    }
}

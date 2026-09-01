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
        Self::for_launch_with_identity(agent_token, env, None)
    }

    /// [`RunHookPayload::for_launch`], optionally carrying the tunnel identity material.
    ///
    /// The two identity fields are the VM's own seed and the launching host's public key,
    /// under the keys `protocol::identity` declares — the daemon pins one and derives from
    /// the other, which is what makes `microvm tunnel --verify-identity` able to prove the
    /// far end without trusting the endpoint proxy. `None` produces byte-for-byte the
    /// payload the other constructors always built, so a caller who never asks for identity
    /// cannot be affected by the field existing (the same compatibility rule `env` follows).
    ///
    /// The material costs [`protocol::identity::IDENTITY_PAYLOAD_BYTES`] of the shared
    /// 4096-byte budget — two 44-character base64 values plus JSON framing — and the ceiling
    /// check below covers it like everything else in the payload.
    pub fn for_launch_with_identity(
        agent_token: &str,
        env: &std::collections::HashMap<String, String>,
        identity: Option<&crate::identity::LaunchIdentity>,
    ) -> Result<Self, Error> {
        let mut payload = if env.is_empty() {
            serde_json::json!({ "agent_token": agent_token })
        } else {
            serde_json::json!({ "agent_token": agent_token, "env": env })
        };
        if let Some(identity) = identity {
            let object = payload
                .as_object_mut()
                .expect("the payload is built as an object two lines up");
            object.insert(
                protocol::identity::SEED_KEY.to_string(),
                serde_json::Value::String(identity.seed_field()),
            );
            object.insert(
                protocol::identity::HOST_PUBLIC_KEY_KEY.to_string(),
                serde_json::Value::String(identity.host_public_field()),
            );
        }
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
        super::require_valid_identifier("imageIdentifier", &request.image_identifier)?;
        super::require_duration_in_range(request.max_duration_sec)?;
        // `min: 60` on `IdlePolicyMaxIdleDurationSecondsInteger`. There used to be no guard for
        // this on the grounds that botocore enforces `min` locally — true of the deleted Python
        // client and never of this one. See `super::require_idle_duration`.
        super::require_idle_duration(request.max_idle_sec)?;
        // Checked before the wire because a pinned launch is what a rollback does, and a
        // `ValidationException` on the member rather than on the version is the least useful
        // failure available at that moment. See `super::require_valid_version`.
        if let Some(version) = request.image_version.as_deref() {
            super::require_valid_version("imageVersion", version)?;
        }
        // `RoleArn` — optional in the model, and every real launch has one. A malformed value
        // here is a `ValidationException` on a member the caller may not know is being filled
        // from their infra config, so the refusal names it.
        if let Some(role) = request.execution_role_arn.as_deref() {
            super::require_valid_role_arn("executionRoleArn", role)?;
        }

        // `ALL_INGRESS` cannot be combined with any other ingress connector, and the
        // platform says so only at token-mint time: `RunMicrovm` accepts the invalid
        // set, the VM reaches RUNNING, and it bills until something asks for a shell
        // token (`docs/PLATFORM.md`, measured 2026-08-15). Refused here instead, before
        // anything launches or bills. The pair that works is `[HTTP_INGRESS,
        // SHELL_INGRESS]`.
        let has_all_ingress = request
            .connectors
            .contains(&super::ConnectorIntent::AllIngress);
        let has_finer_ingress = request.connectors.iter().any(|intent| {
            matches!(
                intent,
                super::ConnectorIntent::HttpIngress | super::ConnectorIntent::ShellIngress
            )
        });
        if has_all_ingress && has_finer_ingress {
            return Err(Error::invalid_arg(
                "ALL_INGRESS cannot be combined with other ingress connectors; use \
                 HTTP_INGRESS and/or SHELL_INGRESS instead. The platform accepts the \
                 combination at launch and rejects it only when a shell token is minted, \
                 after the VM has launched and billed — so it is refused here.",
            ));
        }

        // The other half of the same measured constraint: the pair that launches and
        // mints a shell token is `[HTTP_INGRESS, SHELL_INGRESS]`. A shell-only ingress
        // set has no measured success path, and the platform accepts an invalid set at
        // launch — so it too is refused here, before anything launches or bills.
        let has_shell_ingress = request
            .connectors
            .contains(&super::ConnectorIntent::ShellIngress);
        let has_http_ingress = request
            .connectors
            .contains(&super::ConnectorIntent::HttpIngress);
        if has_shell_ingress && !has_http_ingress {
            return Err(Error::invalid_arg(
                "SHELL_INGRESS was requested without HTTP_INGRESS. The measured pair that \
                 launches and mints a shell token is [HTTP_INGRESS, SHELL_INGRESS] \
                 (docs/PLATFORM.md); the platform accepts an invalid ingress set at launch \
                 and the VM runs and bills before any failure appears — so it is refused \
                 here.",
            ));
        }

        // Ingress and egress go in separate members, so they are split by intent rather
        // than concatenated into one list.
        let ingress: Vec<String> = request
            .connectors
            .iter()
            .filter(|intent| {
                matches!(
                    intent,
                    super::ConnectorIntent::AllIngress
                        | super::ConnectorIntent::HttpIngress
                        | super::ConnectorIntent::ShellIngress
                )
            })
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
            image_version: request.image_version.clone(),
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
    ///
    /// `microvmIdentifier` is a URI parameter, so an empty one collapses the path onto the
    /// listing and asks a different question — see [`super::require_valid_identifier`]. This is
    /// the read every wait loop makes, so the check also covers
    /// [`Self::wait_for_state`] and everything built on it.
    pub async fn get_microvm(&self, id: &str) -> Result<Microvm, Error> {
        super::require_valid_identifier("microvmIdentifier", id)?;
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
        super::require_valid_identifier("microvmIdentifier", id)?;
        let call = Call::post_empty("SuspendMicrovm", paths::suspend(id));
        send_with_retry(self.transport(), call).await?;
        Ok(())
    }

    /// `ResumeMicrovm`. Thaws the VM; no token re-delivery and no re-bootstrap.
    pub async fn resume(&self, id: &str) -> Result<(), Error> {
        super::require_valid_identifier("microvmIdentifier", id)?;
        let call = Call::post_empty("ResumeMicrovm", paths::resume(id));
        send_with_retry(self.transport(), call).await?;
        Ok(())
    }

    /// `TerminateMicrovm`.
    ///
    /// The identifier check matters most on this one, and for a reason the others do not share:
    /// the path is `DELETE /microvms/{microvmIdentifier}`, so an empty identifier is a `DELETE`
    /// addressed at the **collection**. There is no operation on that path and the service
    /// refuses it, but "send a delete at the fleet and rely on the service to refuse" is not a
    /// thing to leave to the service.
    pub async fn terminate(&self, id: &str) -> Result<(), Error> {
        super::require_valid_identifier("microvmIdentifier", id)?;
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
    ///
    /// Scoped to the agent port alone, which is right for every request this client makes on
    /// its own behalf and **wrong for a caller reaching another port on the same VM**. That
    /// caller wants [`Self::mint_auth_token_for`]; see [`ops::PortSpecification`] for the
    /// measurement that made the distinction necessary.
    pub async fn mint_auth_token(&self, id: &str) -> Result<ProxyToken, Error> {
        self.mint_auth_token_for(id, &[ops::PortSpecification::port(self.port())])
            .await
    }

    /// Mints a proxy token scoped to `ports`, whatever they are.
    ///
    /// The agent port is **not** added for you. A caller naming ports is answering the
    /// question the token asks, and silently widening the scope would hand back a
    /// credential broader than the one requested — the opposite of the mistake this exists
    /// to fix, and a worse one, since it is invisible. A caller that needs both the agent
    /// port and a workload port names both, which is what [`ProxyAuth`] does.
    ///
    /// The returned token still records the agent port as its *default* header port, because
    /// that is the port a request carries when nobody names one.
    ///
    /// [`ProxyAuth`]: crate::session::ProxyAuth
    pub async fn mint_auth_token_for(
        &self,
        id: &str,
        ports: &[ops::PortSpecification],
    ) -> Result<ProxyToken, Error> {
        super::require_valid_identifier("microvmIdentifier", id)?;
        if ports.is_empty() {
            // The model declares `min: 1` on `allowedPorts`, so an empty list is a
            // round-trip that comes back as a validation error naming a member the caller
            // did not know it was sending. Refused here, with the reason.
            return Err(Error::new(
                ErrorKind::Precondition,
                "a proxy token must allow at least one port ('allowedPorts' declares min: 1). \
                 Name the ports this token is for — the agent port plus any workload port \
                 reached through the endpoint.",
            ));
        }
        // `PortNumber` is `min: 1`, and the value that reaches here as a 0 is not a typo — it is
        // a caller who took `ControlPlane::with_port(0)`'s default port, or who passed a
        // `SocketAddr`'s port before binding. A token minted for port 0 authorizes nothing, and
        // the proxy's refusal is `403 Access to port denied`, which reads like the token being
        // wrong rather than its scope being empty (measured 2026-08-15, see
        // `ops::PortSpecification`). Both ends of a range are checked, because `PortRange`'s two
        // members name the same shape and a range starting at 0 is the same defect.
        for spec in ports {
            match spec {
                ops::PortSpecification::One { port } => {
                    super::require_valid_port("allowedPorts[].port", *port)?;
                }
                ops::PortSpecification::Range { range } => {
                    super::require_valid_port("allowedPorts[].range.startPort", range.start_port)?;
                    super::require_valid_port("allowedPorts[].range.endPort", range.end_port)?;
                }
                // `allPorts` names no number, so there is nothing to bound.
                ops::PortSpecification::All { .. } => {}
            }
        }
        let wire = ops::CreateAuthTokenWire {
            expiration_in_minutes: MAX_TOKEN_MINUTES,
            allowed_ports: ports.to_vec(),
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

    /// The identity material costs exactly its declared budget, and a launch without it is
    /// byte-for-byte what the older constructors built.
    ///
    /// Measured by difference rather than trusted from the constant's comment, the same
    /// discipline `env_contribution` applies: the ceiling is on the serialized string, so
    /// only the serialized string can say what the material costs. The compatibility half
    /// matters just as much — a pinned daemon that has never heard of identity must see no
    /// change from a client that did not ask for it.
    #[test]
    fn the_identity_material_costs_its_declared_budget_and_absence_costs_nothing() {
        let env = std::collections::HashMap::new();
        let without = RunHookPayload::for_launch("tok", &env).expect("fits");
        let none_asked = RunHookPayload::for_launch_with_identity("tok", &env, None).expect("fits");
        assert_eq!(
            without.as_str(),
            none_asked.as_str(),
            "no identity must produce byte-for-byte the payload this client always sent"
        );

        let identity = crate::identity::LaunchIdentity::from_seeds([7_u8; 32], [9_u8; 32]);
        let with =
            RunHookPayload::for_launch_with_identity("tok", &env, Some(&identity)).expect("fits");
        assert_eq!(
            with.len() - without.len(),
            protocol::identity::IDENTITY_PAYLOAD_BYTES,
            "the budget constant must equal what the material really costs on the wire"
        );

        // And the payload parses as the daemon parses it, with both halves present.
        let parsed = protocol::hook::RunHook::parse(with.as_str()).expect("the daemon can read it");
        assert_eq!(
            parsed.identity_seed.as_deref(),
            Some(identity.seed_field().as_str())
        );
        assert_eq!(
            parsed.identity_host_public_key.as_deref(),
            Some(identity.host_public_field().as_str())
        );
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

    /// **A pinned `imageVersion` reaches the wire, and an unpinned launch omits the member.**
    ///
    /// The absence half is the one that matters for compatibility: an unpinned launch has to
    /// emit byte-for-byte what this client always sent, so a `"imageVersion": null` on every
    /// launch would be a new member on a request that has worked for months. The presence half
    /// is what makes a canary a canary — the launch goes against the version named rather than
    /// against whatever became latest while it was starting.
    ///
    /// **Falsification** — run 2026-08-16. Drop the `image_version` assignment in
    /// `run_microvm` and the pinned assertion goes red with the member absent; remove
    /// `skip_serializing_if` from the wire field and the absence assertion goes red with a
    /// `null`.
    #[tokio::test]
    async fn a_pinned_launch_version_reaches_the_wire_and_an_unpinned_one_omits_the_member() {
        let (plane, fake, _) = planted();
        fake.answer(
            "RunMicrovm",
            Answer::ok(fake::microvm_response("PENDING", None)),
        );
        let payload = RunHookPayload::for_agent_token("token").expect("fits");
        plane
            .run_microvm(RunMicrovmRequest::new("arn:image", payload).with_image_version("2.0"))
            .await
            .expect("launches");

        let body = fake.first_body("RunMicrovm");
        assert_eq!(body["imageVersion"], "2.0");
        assert_eq!(
            body["imageIdentifier"], "arn:image",
            "pinning a version does not replace the identifier; both are sent"
        );

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
        assert!(
            fake.first_body("RunMicrovm").get("imageVersion").is_none(),
            "an unpinned launch must send what this client always sent: {}",
            fake.first_body("RunMicrovm")
        );
    }

    /// An invalid `imageVersion` is refused before any call — including on the launch path,
    /// which is where a rollback happens.
    ///
    /// A `ValidationException` about the request rather than about the version, arriving at the
    /// moment someone is re-pinning away from a bad build, is the least useful failure
    /// available. So the `Version` shape's three constraints are checked here.
    ///
    /// **Falsification** — run 2026-08-16. Delete the `require_valid_version` call from
    /// `run_microvm` and the zero-call assertion goes red.
    #[tokio::test]
    async fn an_invalid_launch_version_reaches_no_control_plane_call() {
        for bad in ["", "2.0\n", "a b"] {
            let (plane, fake, _) = planted();
            let payload = RunHookPayload::for_agent_token("token").expect("fits");
            let error = plane
                .run_microvm(RunMicrovmRequest::new("arn:image", payload).with_image_version(bad))
                .await
                .expect_err(&format!("{bad:?} is not a legal Version"));
            assert_eq!(error.kind(), ErrorKind::InvalidArg, "{bad:?}");
            assert!(
                error.to_string().contains("imageVersion"),
                "{bad:?}: {error}"
            );
            assert_eq!(fake.calls().len(), 0, "{bad:?} must not reach the wire");
        }
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
                .with_port(8080)
                .expect("8080 is a legal port");
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

    /// **TRAP-11, revised, across a full lifecycle.** Drive create, wait, launch, wait,
    /// mint, suspend, resume, terminate, delete — and then assert the recorder saw
    /// **zero** shell-auth calls, and that the launch request carried **exactly the
    /// connectors the caller asked for**, nothing added and nothing dropped.
    ///
    /// The body scan used to assert zero `SHELL_INGRESS` anywhere, standing on the claim
    /// that no intent could name it. `ConnectorIntent::ShellIngress` exists now — the
    /// measured ground is that one interactive session is not programmatic exec, not
    /// that the connector is unspeakable (`docs/PLATFORM.md`, 2026-08-15) — so the
    /// honest assertion is set-equality against the request: a caller that asked for
    /// `[ALL_INGRESS]` plus egress gets that set on the wire and no other.
    ///
    /// The shell-auth scans stay as counts, because that half of TRAP-11 is still an
    /// absence: there is no method on this client to call, so nothing *rejects* a shell
    /// operation and the only honest assertion is that a full lifecycle never emits one.
    ///
    /// **Falsification** — add a `mint_shell_auth_token` calling
    /// `POST /2025-09-09/microvms/{id}/shell-auth-token` and call it here: the operation
    /// scan and the path scan both fail. Make `run_microvm` inject a connector the
    /// request did not carry: the set-equality assertions fail.
    #[tokio::test]
    async fn a_full_lifecycle_never_calls_shell_auth_and_requests_only_what_was_asked() {
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
            "this client has no shell-auth method, and a lifecycle that asked for no \
             shell must mint no shell token: {operations:?}"
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

        // The launch carried exactly the connector set the caller asked for —
        // `ALL_INGRESS` from the default plus the egress `with_egress` added — and this
        // client injected nothing of its own. Set-equality cuts both ways: a connector
        // the caller never named (the shell, or anything else) fails it, and so does a
        // dropped one.
        let launch = fake.first_body("RunMicrovm");
        assert_eq!(
            launch["ingressNetworkConnectors"],
            serde_json::json!([
                "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:ALL_INGRESS"
            ]),
            "ingress is exactly what the caller asked for"
        );
        assert_eq!(
            launch["egressNetworkConnectors"],
            serde_json::json!([
                "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:INTERNET_EGRESS"
            ]),
            "egress is exactly what the caller asked for"
        );
    }

    /// The measured pair `[HTTP_INGRESS, SHELL_INGRESS]` reaches the wire intact — the
    /// combination `docs/PLATFORM.md` (2026-08-15) records as launching and minting a
    /// shell token successfully. Both are ingress connectors, so both must land in
    /// `ingressNetworkConnectors`; a split that only recognized `ALL_INGRESS` as ingress
    /// would silently drop them, and this is the test that catches that.
    #[tokio::test]
    async fn http_plus_shell_ingress_is_the_pair_that_reaches_the_wire() {
        let (plane, fake, _) = planted();
        fake.answer(
            "RunMicrovm",
            Answer::ok(fake::microvm_response("PENDING", None)),
        );

        let payload = RunHookPayload::for_agent_token("agent-token").expect("fits");
        let mut request = RunMicrovmRequest::new("arn:image", payload);
        request.connectors = vec![
            super::super::ConnectorIntent::HttpIngress,
            super::super::ConnectorIntent::ShellIngress,
        ];
        plane.run_microvm(request).await.expect("launches");

        let launch = fake.first_body("RunMicrovm");
        assert_eq!(
            launch["ingressNetworkConnectors"],
            serde_json::json!([
                "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:HTTP_INGRESS",
                "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:SHELL_INGRESS"
            ]),
            "both fine-grained ingress connectors reach the wire, in the order asked"
        );
        assert_eq!(
            launch.get("egressNetworkConnectors"),
            None,
            "no egress was asked for, so the member is absent rather than empty"
        );
    }

    /// `ALL_INGRESS` combined with the shell connector is refused **before the wire**.
    /// The platform accepts the invalid set at launch — the VM reaches RUNNING and bills
    /// — and rejects it only when a shell token is minted (`docs/PLATFORM.md`,
    /// 2026-08-15). A local refusal is the only one that costs nothing.
    ///
    /// **Falsification** — run 2026-08-31: with the combination guard in
    /// [`ControlPlane::run_microvm`] disabled, this failed on the error assertion.
    /// Restored.
    #[tokio::test]
    async fn all_ingress_combined_with_shell_ingress_is_refused_before_the_wire() {
        let (plane, fake, _) = planted();

        let payload = RunHookPayload::for_agent_token("agent-token").expect("fits");
        let mut request = RunMicrovmRequest::new("arn:image", payload);
        request
            .connectors
            .push(super::super::ConnectorIntent::ShellIngress);
        let error = plane
            .run_microvm(request)
            .await
            .expect_err("the invalid combination is refused locally");

        assert!(
            error
                .to_string()
                .contains("HTTP_INGRESS and/or SHELL_INGRESS"),
            "the refusal names the pair that works: {error}"
        );
        assert_eq!(
            fake.call_count("RunMicrovm"),
            0,
            "nothing launched, so nothing billed"
        );
    }

    /// **W1, measured constraint 1, second half.** `SHELL_INGRESS` without `HTTP_INGRESS`
    /// is the same launches-bills-fails-late shape as `ALL_INGRESS` plus a finer connector:
    /// the measured pair that launches and mints a shell token is `[HTTP_INGRESS,
    /// SHELL_INGRESS]` (`docs/PLATFORM.md`), no measurement says a shell-only ingress set
    /// even mints, and the platform accepts an invalid set at launch — the VM reaches
    /// RUNNING and bills before any refusal appears. Refused here instead, before the wire.
    ///
    /// **Falsification** — run 2026-08-31. This test was written before the guard clause
    /// existed and failed on both assertions (`run_microvm` reached the wire and launched);
    /// adding the shell-without-HTTP refusal in `run_microvm` turned it green.
    #[tokio::test]
    async fn shell_ingress_without_http_ingress_is_refused_before_the_wire() {
        let (plane, fake, _) = planted();

        let payload = RunHookPayload::for_agent_token("agent-token").expect("fits");
        let mut request = RunMicrovmRequest::new("arn:image", payload);
        request.connectors = vec![
            super::super::ConnectorIntent::ShellIngress,
            super::super::ConnectorIntent::Egress,
        ];
        let error = plane
            .run_microvm(request)
            .await
            .expect_err("shell ingress without HTTP ingress is refused locally");

        assert!(
            error.to_string().contains("HTTP_INGRESS"),
            "the refusal names the missing connector: {error}"
        );
        assert_eq!(
            fake.call_count("RunMicrovm"),
            0,
            "nothing launched, so nothing billed"
        );
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

    // ── issue #24's guards on the launch and lifecycle paths ─────────────────

    /// **`max_idle_sec: 59` no longer reaches the wire.**
    ///
    /// The measurement issue #24 made, turned into a test. The exemption it replaces was not a
    /// wrong number — it was a correct fact about botocore's `VALIDATED_METADATA_ATTRS` applied to
    /// a client that does not use botocore, so `IdlePolicy.maxIdleDurationSeconds` was the one
    /// constraint in the model with a *reason* for having no guard rather than an oversight.
    ///
    /// The fake has `RunMicrovm` answered, so a missing guard shows as a launch that **succeeded**
    /// with 59 on the wire rather than as some other failure. That is what makes the zero-call
    /// assertion the load-bearing one.
    ///
    /// **Guard proof** — run 2026-08-16. Delete `require_idle_duration` from `run_microvm` and
    /// this fails on `expect_err`, with the fake recording `RunMicrovm` once and
    /// `idlePolicy.maxIdleDurationSeconds: 59` in its body.
    #[tokio::test]
    async fn an_under_minimum_idle_duration_reaches_no_control_plane_call() {
        for under in [0, 1, 59] {
            let (plane, fake, _) = planted();
            fake.answer(
                "RunMicrovm",
                Answer::ok(fake::microvm_response("PENDING", None)),
            );

            let payload = RunHookPayload::for_agent_token("token").expect("fits");
            let mut request = RunMicrovmRequest::new("arn:image", payload);
            request.max_idle_sec = under;

            let error = plane
                .run_microvm(request)
                .await
                .expect_err("the model's min is 60");
            assert_eq!(error.kind(), ErrorKind::InvalidArg);
            let message = error.to_string();
            assert!(message.contains("maxIdleDurationSeconds"), "{message}");
            assert!(message.contains(&under.to_string()), "{message}");
            assert_eq!(
                fake.calls().len(),
                0,
                "{under} was refused locally; before this guard it was serialized and sent"
            );
        }

        // 60 exactly does launch, so the boundary is inclusive and the guard is not refusing
        // every idle window.
        let (plane, fake, _) = planted();
        fake.answer(
            "RunMicrovm",
            Answer::ok(fake::microvm_response("PENDING", None)),
        );
        let payload = RunHookPayload::for_agent_token("token").expect("fits");
        let mut request = RunMicrovmRequest::new("arn:image", payload);
        request.max_idle_sec = 60;
        plane.run_microvm(request).await.expect("60 is the minimum");
        assert_eq!(
            fake.first_body("RunMicrovm")["idlePolicy"]["maxIdleDurationSeconds"],
            60
        );
    }

    /// **The launch's `executionRoleArn` and `imageIdentifier`, refused before the call.**
    ///
    /// `executionRoleArn` is filled from an infra config on every real launch — the CLI reads it
    /// out of `MICROVM_EXECUTION_ROLE_ARN` — so a malformed one is a `ValidationException` about a
    /// member the caller never typed. Naming it locally is the whole difference.
    ///
    /// **Guard proof** — run 2026-08-16. Delete `require_valid_role_arn("executionRoleArn", …)`
    /// and the role rows launch successfully with the bad ARN on the wire; delete
    /// `require_valid_identifier` and the identifier rows do.
    #[tokio::test]
    async fn a_malformed_launch_role_or_identifier_reaches_no_control_plane_call() {
        /// What to break, aliased for the reason clippy's `type_complexity` gives — and the same
        /// alias `image.rs`'s create-path table uses, so the two read alike.
        type Mutate = fn(&mut RunMicrovmRequest);
        let rows: [(&str, Mutate, &str); 6] = [
            (
                "an execution role that is a bare name",
                |request| {
                    request.execution_role_arn = Some("execution-role".to_string());
                },
                "role *name*",
            ),
            (
                "an execution role with eleven account digits",
                |request| {
                    request.execution_role_arn =
                        Some("arn:aws:iam::12345678901:role/exec".to_string());
                },
                "exactly twelve digits",
            ),
            (
                "an execution role that is a function ARN",
                |request| {
                    request.execution_role_arn =
                        Some("arn:aws:lambda:us-east-1:123456789012:function:h".to_string());
                },
                "RoleArn pattern",
            ),
            (
                "an empty image identifier",
                |request| {
                    request.image_identifier = String::new();
                },
                "collapses the path",
            ),
            (
                "a 257-character image identifier",
                |request| {
                    request.image_identifier = "a".repeat(257);
                },
                "MicrovmImageArn permits 2048",
            ),
            (
                "an image version with a newline",
                |request| {
                    request.image_version = Some("2.0\n".to_string());
                },
                "contains whitespace",
            ),
        ];

        for (label, mutate, expected) in rows {
            let (plane, fake, _) = planted();
            fake.answer(
                "RunMicrovm",
                Answer::ok(fake::microvm_response("PENDING", None)),
            );

            let payload = RunHookPayload::for_agent_token("token").expect("fits");
            let mut request = RunMicrovmRequest::new("arn:image", payload);
            mutate(&mut request);

            let error = plane
                .run_microvm(request)
                .await
                .expect_err(&format!("{label} must be refused"));
            assert_eq!(error.kind(), ErrorKind::InvalidArg, "{label}");
            assert!(
                error.to_string().contains(expected),
                "{label}: wanted {expected:?}, got {error}"
            );
            assert_eq!(fake.calls().len(), 0, "{label}");
        }

        // The control case: a launch carrying a real execution role, a pinned version, and the
        // account's actual ARN shape goes through.
        let (plane, fake, _) = planted();
        fake.answer(
            "RunMicrovm",
            Answer::ok(fake::microvm_response("PENDING", None)),
        );
        let payload = RunHookPayload::for_agent_token("token").expect("fits");
        let mut request = RunMicrovmRequest::new(
            "arn:aws:lambda:us-east-1:392583147479:microvm-image:agentd-conformance",
            payload,
        );
        request.execution_role_arn =
            Some("arn:aws:iam::392583147479:role/bonk-sandbox-microvm-execution".to_string());
        request.image_version = Some("1.0".to_string());
        plane
            .run_microvm(request)
            .await
            .expect("a realistic pinned launch with a real execution role");
        assert_eq!(fake.call_count("RunMicrovm"), 1);
    }

    /// **The `microvmIdentifier` guard on every lifecycle operation, proved by the call count.**
    ///
    /// `TerminateMicrovm` is the one worth reading twice. Its path is
    /// `DELETE /microvms/{microvmIdentifier}`, so an empty identifier is a `DELETE` addressed at
    /// the fleet collection. The service has no operation there and refuses it, but "send a delete
    /// at the collection and trust the service" is not a thing to leave to the service.
    ///
    /// **Guard proof** — run 2026-08-16. Delete `require_valid_identifier` from any one of the
    /// five and its assertion goes red with `calls: 1`.
    #[tokio::test]
    async fn no_lifecycle_operation_sends_an_empty_or_over_long_microvm_identifier() {
        for bad in ["", &"a".repeat(257)] {
            let (plane, fake, _) = planted();
            // Every operation answered, so a missing guard is a *successful* call rather than a
            // different failure.
            fake.answer(
                "GetMicrovm",
                Answer::ok(fake::microvm_response("RUNNING", None)),
            );
            fake.answer("SuspendMicrovm", Answer::ok("{}"));
            fake.answer("ResumeMicrovm", Answer::ok("{}"));
            fake.answer("TerminateMicrovm", Answer::ok("{}"));
            fake.answer(
                "CreateMicrovmAuthToken",
                Answer::ok(fake::auth_token_response("t")),
            );

            plane.get_microvm(bad).await.expect_err("GetMicrovm");
            plane.suspend(bad).await.expect_err("SuspendMicrovm");
            plane.resume(bad).await.expect_err("ResumeMicrovm");
            plane.terminate(bad).await.expect_err("TerminateMicrovm");
            plane
                .mint_auth_token(bad)
                .await
                .expect_err("CreateMicrovmAuthToken");
            plane
                .wait_for_running(bad, WaitOpts::for_launch())
                .await
                .expect_err("GetMicrovm, in a loop");

            assert_eq!(
                fake.calls().len(),
                0,
                "six operations refused {bad:?} locally. An empty identifier on the terminate \
                 path is a DELETE addressed at the collection."
            );
        }
    }

    /// **Issue #24's `PortNumber` half: a token is never minted for port 0.**
    ///
    /// The measurement that makes this worth a guard rather than a comment is in
    /// [`ops::PortSpecification`]: a token whose `allowedPorts` does not cover the port a request
    /// names is refused with **403 `Access to port denied`**, and on the WebSocket path it is close
    /// code 1006 with no reason. So a token minted for port 0 authorizes nothing and fails in a
    /// way that reads like a bad credential rather than like an empty scope.
    ///
    /// Both ends of a range are checked as well as the single-port form, because `PortRange`'s two
    /// members name the same shape and a range starting at 0 is the same defect.
    ///
    /// **Guard proof** — run 2026-08-16. Delete the `for spec in ports` loop from
    /// `mint_auth_token_for` and all three rows mint successfully with `{"port": 0}` or
    /// `{"range": {"startPort": 0, …}}` on the wire.
    #[tokio::test]
    async fn no_proxy_token_is_minted_for_port_zero() {
        let rows: [(&str, ops::PortSpecification, &str); 3] = [
            (
                "a single port of 0",
                ops::PortSpecification::port(0),
                "allowedPorts[].port",
            ),
            (
                "a range starting at 0",
                ops::PortSpecification::range(0, 9000),
                "allowedPorts[].range.startPort",
            ),
            (
                "a range ending at 0",
                ops::PortSpecification::range(0, 0),
                "allowedPorts[].range.startPort",
            ),
        ];

        for (label, spec, member) in rows {
            let (plane, fake, _) = planted();
            fake.answer(
                "CreateMicrovmAuthToken",
                Answer::ok(fake::auth_token_response("t")),
            );

            let error = plane
                .mint_auth_token_for("mvm-1", &[spec])
                .await
                .expect_err(&format!("{label} must be refused"));
            assert_eq!(error.kind(), ErrorKind::InvalidArg, "{label}");
            let message = error.to_string();
            assert!(message.contains(member), "{label}: {message}");
            assert!(
                message.contains("authorizes nothing"),
                "{label}: the 403 consequence has to be in the message: {message}"
            );
            assert_eq!(fake.calls().len(), 0, "{label}");
        }

        // A zero port among legal ones is still refused: the check is per-entry, not on the
        // first. A token minted with `[{port: 9000}, {port: 0}]` would look scoped correctly and
        // carry a member the service refuses the whole request over.
        let (plane, fake, _) = planted();
        fake.answer(
            "CreateMicrovmAuthToken",
            Answer::ok(fake::auth_token_response("t")),
        );
        plane
            .mint_auth_token_for(
                "mvm-1",
                &[
                    ops::PortSpecification::port(9000),
                    ops::PortSpecification::port(0),
                ],
            )
            .await
            .expect_err("every entry is checked, not the first");
        assert_eq!(fake.calls().len(), 0);

        // And the three legal forms mint, including `allPorts` — which names no number, so there
        // is nothing to bound and the guard must not refuse it.
        for legal in [
            ops::PortSpecification::port(1),
            ops::PortSpecification::port(65_535),
            ops::PortSpecification::range(1, 65_535),
            ops::PortSpecification::all(),
        ] {
            let (plane, fake, _) = planted();
            fake.answer(
                "CreateMicrovmAuthToken",
                Answer::ok(fake::auth_token_response("t")),
            );
            plane
                .mint_auth_token_for("mvm-1", &[legal])
                .await
                .expect("a legal port specification mints");
            assert_eq!(fake.call_count("CreateMicrovmAuthToken"), 1);
        }
    }
}

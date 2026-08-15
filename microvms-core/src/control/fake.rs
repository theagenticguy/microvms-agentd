// SPDX-License-Identifier: Apache-2.0
//! The contract recorder: a fake control plane that asserts on what was *emitted*.
//!
//! # The rule this fake follows
//!
//! **It never asserts on a value the client computed, and it never answers in a spelling
//! the client chose.**
//!
//! That rule is the fix for a specific bug. The Python client's stall probe read
//! `b.get("state")` from a build summary whose member is `buildState`, so it read `None`
//! from every real response — and its unit test passed, because the test's fake returned
//! `{"state": "PENDING"}`. The fake shared the client's own misreading, so the two agreed
//! with each other and with nothing else, and the only guard separating a wedged image
//! from a slow build was dead against live AWS for a review round.
//!
//! So this fake:
//!
//! * answers with **literal JSON strings** written in the service model's spelling, not
//!   with values serialized from this crate's own types;
//! * records every [`Call`] verbatim and lets a test assert on the method, the path, and
//!   the body's *JSON member names*;
//! * counts calls per operation, which is what makes the TRAP-11 assertion possible — "no
//!   `CreateMicrovmShellAuthToken` across a full lifecycle" is a count, not a refusal.
//!
//! # Why it is hand-rolled rather than wiremock
//!
//! Because the assertion is on the request's *shape against the model*, not on an HTTP
//! exchange. A wiremock matcher would sit behind the signing path and test reqwest; this
//! sits at the [`Transport`] seam and tests the thing the model constrains. It also runs
//! with no ports, no runtime setup, and no ordering flakiness.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use super::Clock;
use super::transport::{Call, Reply, Transport};
use crate::error::{Error, ErrorKind};

/// A queued answer: the status and the raw body bytes.
///
/// The body is a `String` because every one is written as a literal in a test — which is
/// the honest-fake rule made mechanical, since there is no way to *accidentally* pass a
/// serialized `ops::` type here.
#[derive(Clone, Debug)]
pub struct Answer {
    pub status: u16,
    pub body: String,
}

impl Answer {
    /// A 200 carrying `body`.
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
        }
    }

    /// A 201 carrying `body`, which is what `CreateMicrovmImage` answers.
    pub fn created(body: impl Into<String>) -> Self {
        Self {
            status: 201,
            body: body.into(),
        }
    }

    /// A failure status carrying a modeled error body.
    pub fn failure(status: u16, message: &str) -> Self {
        Self {
            status,
            body: format!(r#"{{"message": {}}}"#, json_string(message)),
        }
    }
}

/// Escapes `text` as a JSON string, so a test can put a quote in a message.
fn json_string(text: &str) -> String {
    serde_json::to_string(text).expect("a string always serializes")
}

/// The fake, plus its ledger.
#[derive(Debug, Default)]
pub struct FakeControlPlane {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Queued answers per operation, consumed front-to-back. The last one repeats, so a
    /// polling test can queue two answers and let the second stand for every later poll.
    queued: HashMap<&'static str, std::collections::VecDeque<Answer>>,
    /// Every call, in order.
    calls: Vec<Call>,
    /// A transport-level failure to raise for an operation instead of answering.
    transport_errors: HashMap<&'static str, usize>,
}

impl FakeControlPlane {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues `answer` for the next call to `operation`.
    ///
    /// Answers are consumed in order; when the queue for an operation runs dry the **last**
    /// answer repeats. That is what lets `wait_for_running` be tested with two queued
    /// answers — PENDING then RUNNING — rather than with one per expected poll, which would
    /// make the test depend on the poll count it is not trying to assert.
    pub fn answer(&self, operation: &'static str, answer: Answer) -> &Self {
        self.inner
            .lock()
            .expect("no test panics while holding this")
            .queued
            .entry(operation)
            .or_default()
            .push_back(answer);
        self
    }

    /// Makes the next `count` calls to `operation` fail at the transport level — no status
    /// at all, which is what a connection reset looks like.
    pub fn fail_transport(&self, operation: &'static str, count: usize) -> &Self {
        self.inner
            .lock()
            .expect("not poisoned")
            .transport_errors
            .insert(operation, count);
        self
    }

    /// Every call made, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().expect("not poisoned").calls.clone()
    }

    /// How many times `operation` was called.
    ///
    /// The TRAP-11 assertion is this function returning zero for
    /// `CreateMicrovmShellAuthToken` after a full lifecycle.
    pub fn call_count(&self, operation: &str) -> usize {
        self.inner
            .lock()
            .expect("not poisoned")
            .calls
            .iter()
            .filter(|call| call.operation == operation)
            .count()
    }

    /// The operations called, in order, deduplicated only where consecutive.
    pub fn operations(&self) -> Vec<&'static str> {
        self.inner
            .lock()
            .expect("not poisoned")
            .calls
            .iter()
            .map(|call| call.operation)
            .collect()
    }

    /// The body of the *n*th call to `operation`, parsed as generic JSON.
    ///
    /// Generic `Value` rather than an `ops::` type on purpose: a test that deserialized
    /// into this crate's own struct would only ever see the members that struct declares,
    /// so a wrong member name would be invisible — which is the whole failure mode this
    /// fake exists to prevent. Asserting on `value["additionalOsCapabilities"]` reads the
    /// wire member.
    pub fn body_of(&self, operation: &str, nth: usize) -> serde_json::Value {
        let inner = self.inner.lock().expect("not poisoned");
        let call = inner
            .calls
            .iter()
            .filter(|call| call.operation == operation)
            .nth(nth)
            .unwrap_or_else(|| panic!("no call {nth} to {operation}; calls: {:?}", inner.calls));
        let body = call
            .body
            .as_deref()
            .unwrap_or_else(|| panic!("{operation} sent no body"));
        serde_json::from_slice(body)
            .unwrap_or_else(|error| panic!("{operation} body is not JSON: {error}"))
    }

    /// The first body sent to `operation`.
    pub fn first_body(&self, operation: &str) -> serde_json::Value {
        self.body_of(operation, 0)
    }

    /// Every `clientToken` this fake saw, in call order.
    ///
    /// Read off the **wire member name**, which is what makes the TRAP-1 proof honest: it
    /// observes what was emitted rather than calling the minting function again.
    pub fn client_tokens(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("not poisoned")
            .calls
            .iter()
            .filter_map(|call| {
                let body = call.body.as_deref()?;
                let value: serde_json::Value = serde_json::from_slice(body).ok()?;
                Some(value.get("clientToken")?.as_str()?.to_string())
            })
            .collect()
    }

    /// Every path this fake was called on, which is where a `SHELL_INGRESS` or shell-auth
    /// route would show up even if the operation name were changed.
    pub fn paths(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("not poisoned")
            .calls
            .iter()
            .map(|call| call.path.clone())
            .collect()
    }

    /// Every request body, as raw text. Used for the TRAP-11 scan: a `SHELL_INGRESS`
    /// connector would appear here whatever named it.
    pub fn bodies_as_text(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("not poisoned")
            .calls
            .iter()
            .filter_map(|call| call.body.as_deref().map(String::from_utf8_lossy))
            .map(|text| text.to_string())
            .collect()
    }
}

impl Transport for FakeControlPlane {
    fn send(
        &self,
        call: Call,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Reply, Error>> + Send + '_>>
    {
        let result = {
            let mut inner = self.inner.lock().expect("not poisoned");
            let operation = call.operation;
            inner.calls.push(call);

            if let Some(remaining) = inner.transport_errors.get_mut(operation)
                && *remaining > 0
            {
                *remaining -= 1;
                Err(Error::new(
                    ErrorKind::Retryable,
                    format!("the {operation} request did not complete: connection reset"),
                ))
            } else {
                let queue = inner
                    .queued
                    .get_mut(operation)
                    .unwrap_or_else(|| panic!("the fake has no answer queued for {operation}"));
                // The last answer repeats, so a polling test queues states rather than
                // one answer per poll.
                let answer = if queue.len() > 1 {
                    queue.pop_front().expect("non-empty")
                } else {
                    queue
                        .front()
                        .cloned()
                        .unwrap_or_else(|| panic!("the fake's queue for {operation} is empty"))
                };
                Ok(Reply {
                    status: answer.status,
                    body: answer.body.into_bytes(),
                })
            }
        };
        Box::pin(async move { result })
    }
}

/// A clock a test drives by hand.
///
/// `elapsed` returns whatever was set, and `sleep` **advances** it rather than waiting —
/// so a 45-minute build wait runs instantly and the code under test still sees time pass
/// exactly as it would have. That is what makes the TRAP-2 stall test possible without a
/// four-minute test.
#[derive(Debug, Default)]
pub struct TestClock {
    elapsed: Mutex<Duration>,
}

impl TestClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the clock forward by `duration`, as if a sleep had happened.
    pub fn advance(&self, duration: Duration) {
        *self.elapsed.lock().expect("not poisoned") += duration;
    }

    /// The current elapsed reading.
    pub fn now(&self) -> Duration {
        *self.elapsed.lock().expect("not poisoned")
    }
}

impl Clock for TestClock {
    fn elapsed(&self) -> Duration {
        self.now()
    }

    fn sleep(
        &self,
        duration: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.advance(duration);
        Box::pin(std::future::ready(()))
    }
}

// ── literal response bodies, in the model's spelling ────────────────────────
//
// Every string below is written by hand from `service-2.json`. None is produced by
// serializing an `ops::` type, which is the point: a member misspelled in `ops.rs` cannot
// be misspelled identically here by accident.
//
// **Every image ARN below uses `microvm-image:<name>`, with a colon.** These fakes used to
// spell it `microvm-image/<name>` while `artifact.rs` built the colon form for the managed
// base, so the repo disagreed with itself about the shape of the one identifier every call
// takes — and the model's own `TaggableResource` pattern accepts only the colon.
//
// Settled by measurement rather than by reading, 2026-08-15, one read-only
// `ListMicrovmImages` plus one `GetMicrovmImage` in us-east-1:
//
//     arn:aws:lambda:us-east-1:<account>:microvm-image:coding-agents-on-bedrock
//
// The colon form is what the service returns and what it accepts. The slash form is not
// merely cosmetic: `GetMicrovmImage` on it answers **`AccessDeniedException`**, because IAM
// evaluates the malformed ARN as a resource the caller has no policy for. So a client that
// built a slash ARN would report a permissions problem for a resource that exists, which is
// the most expensive possible spelling of this mistake.

/// `CreateMicrovmImageResponse` for an image that has just entered `CREATING`.
pub fn create_image_response(name: &str) -> String {
    format!(
        r#"{{
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:{name}",
            "name": "{name}",
            "state": "CREATING",
            "createdAt": 1754524800,
            "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
            "buildRoleArn": "arn:aws:iam::123456789012:role/build",
            "codeArtifact": {{"uri": "s3://bucket/{name}.zip"}},
            "imageVersion": "1"
        }}"#
    )
}

/// `GetMicrovmImageOutput` in `state`.
///
/// `tags` and `updatedAt` are here because a real response carries them — measured
/// 2026-08-15, a `GetMicrovmImage` on an untagged image answers `"tags": {}` and an
/// `updatedAt` — and a fake that omitted a member the client now reads would let a
/// misreading of it pass.
pub fn get_image_response(name: &str, state: &str) -> String {
    format!(
        r#"{{
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:{name}",
            "name": "{name}",
            "state": "{state}",
            "createdAt": 1754524800,
            "updatedAt": 1754528400,
            "tags": {{}}
        }}"#
    )
}

/// `GetMicrovmImageOutput` in `state`, naming a failed version and carrying tags.
///
/// Separate from [`get_image_response`] rather than another parameter on it, because the
/// failure-diagnosis path is the only caller that needs `latestFailedImageVersion` and
/// every other test would have to pass a `None` for it.
pub fn get_image_response_failed(name: &str, state: &str, failed_version: &str) -> String {
    format!(
        r#"{{
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:{name}",
            "name": "{name}",
            "state": "{state}",
            "latestFailedImageVersion": "{failed_version}",
            "createdAt": 1754524800,
            "updatedAt": 1754528400,
            "tags": {{"owner": "conformance"}}
        }}"#
    )
}

/// `ListMicrovmImageVersionsOutput` with one version.
pub fn list_versions_response(version: &str) -> String {
    list_versions_page(&[version], None)
}

/// `ListMicrovmImageVersionsOutput` with one version in `state`, carrying `state_reason`.
pub fn list_versions_response_failed(
    version: &str,
    state: &str,
    state_reason: Option<&str>,
) -> String {
    let reason = match state_reason {
        Some(reason) => format!(r#", "stateReason": {}"#, json_string(reason)),
        None => String::new(),
    };
    format!(
        r#"{{"items": [{}]}}"#,
        version_item(version, state, &reason)
    )
}

/// One page of `ListMicrovmImageVersionsOutput`, with an optional `nextToken`.
///
/// The `nextToken` is **absent** on the last page rather than null, which is what a real
/// final page looks like and what the pagination loop's exit reads.
pub fn list_versions_page(versions: &[&str], next_token: Option<&str>) -> String {
    let items: Vec<String> = versions
        .iter()
        .map(|version| version_item(version, "IN_PROGRESS", ""))
        .collect();
    let token = match next_token {
        Some(token) => format!(r#", "nextToken": {}"#, json_string(token)),
        None => String::new(),
    };
    format!(r#"{{"items": [{}]{token}}}"#, items.join(", "))
}

/// One `MicrovmImageVersionSummary`, in the model's spelling. `state`, not `buildState` —
/// the asymmetry with the build summary is the whole trap.
fn version_item(version: &str, state: &str, extra: &str) -> String {
    format!(
        r#"{{
            "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
            "buildRoleArn": "arn:aws:iam::123456789012:role/build",
            "codeArtifact": {{"uri": "s3://bucket/img.zip"}},
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
            "imageVersion": "{version}",
            "state": "{state}",
            "status": "ACTIVE",
            "createdAt": 1754524800{extra}
        }}"#
    )
}

/// `ListMicrovmImageBuildsOutput` whose builds are all in `build_state`.
///
/// **`buildState`**, spelled as the model spells it. This is the single most important
/// literal in this file: the Python fake wrote `state` here and that is what made the
/// stall guard unfalsifiable.
pub fn list_builds_response(build_state: &str) -> String {
    list_builds_page(&[("build-1", build_state), ("build-2", build_state)], None)
}

/// One page of `ListMicrovmImageBuildsOutput`: `(buildId, buildState)` pairs and an
/// optional `nextToken`.
///
/// The pairs are named rather than counted so a two-page test can assert *which* build the
/// verdict was made over — a page-one/page-two pair with different states is the only shape
/// that distinguishes a paginating probe from one that stops early.
pub fn list_builds_page(builds: &[(&str, &str)], next_token: Option<&str>) -> String {
    list_builds_page_with_reasons(
        &builds
            .iter()
            .map(|(id, state)| (*id, *state, None))
            .collect::<Vec<_>>(),
        next_token,
    )
}

/// One page of `ListMicrovmImageBuildsOutput` whose builds may carry a `stateReason`.
pub fn list_builds_page_with_reasons(
    builds: &[(&str, &str, Option<&str>)],
    next_token: Option<&str>,
) -> String {
    let items: Vec<String> = builds
        .iter()
        .map(|(build_id, build_state, state_reason)| {
            let reason = match state_reason {
                Some(reason) => format!(r#", "stateReason": {}"#, json_string(reason)),
                None => String::new(),
            };
            format!(
                r#"{{
                    "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                    "imageVersion": "1",
                    "buildId": "{build_id}",
                    "buildState": "{build_state}",
                    "architecture": "ARM_64",
                    "chipset": "GRAVITON",
                    "chipsetGeneration": "1",
                    "createdAt": 1754524800{reason}
                }}"#
            )
        })
        .collect();
    let token = match next_token {
        Some(token) => format!(r#", "nextToken": {}"#, json_string(token)),
        None => String::new(),
    };
    format!(r#"{{"items": [{}]{token}}}"#, items.join(", "))
}

/// `RunMicrovmResponse`/`GetMicrovmResponse` in `state`, with an optional `stateReason`.
///
/// `idlePolicy` is present with all three members, because a real `GetMicrovm` sends it
/// that way — measured 2026-08-15 against a RUNNING VM, which is what corrected
/// [`super::ops::IdlePolicy`]'s claim that `suspendedDurationSeconds` is request-only.
pub fn microvm_response(state: &str, state_reason: Option<&str>) -> String {
    let reason = match state_reason {
        Some(reason) => format!(r#", "stateReason": {}"#, json_string(reason)),
        None => String::new(),
    };
    format!(
        r#"{{
            "microvmId": "mvm-abc123",
            "state": "{state}",
            "endpoint": "https://mvm-abc123.microvm.us-east-1.amazonaws.com",
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
            "imageVersion": "1",
            "idlePolicy": {{
                "maxIdleDurationSeconds": 1800,
                "suspendedDurationSeconds": 600,
                "autoResumeEnabled": false
            }},
            "maximumDurationInSeconds": 3600,
            "startedAt": 1754524800{reason}
        }}"#
    )
}

/// One page of `ListMicrovmsResponse`, with an optional `nextToken`.
///
/// `MicrovmItem` is narrower than `GetMicrovmResponse`: no `endpoint` and no `stateReason`,
/// which is the model's own asymmetry and not an omission here.
pub fn list_microvms_page(ids: &[&str], next_token: Option<&str>) -> String {
    let items: Vec<String> = ids
        .iter()
        .map(|id| {
            format!(
                r#"{{
                    "microvmId": "{id}",
                    "state": "RUNNING",
                    "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
                    "imageVersion": "1",
                    "startedAt": 1754524800
                }}"#
            )
        })
        .collect();
    let token = match next_token {
        Some(token) => format!(r#", "nextToken": {}"#, json_string(token)),
        None => String::new(),
    };
    format!(r#"{{"items": [{}]{token}}}"#, items.join(", "))
}

/// `CreateMicrovmAuthTokenResponse` — a header **map**, per TRAP-7.
pub fn auth_token_response(token: &str) -> String {
    format!(
        r#"{{"authToken": {{"X-aws-proxy-auth": {}}}}}"#,
        json_string(token)
    )
}

/// `ListMicrovmImagesResponse` with one page of named images and an optional `nextToken`.
///
/// The member names are the model's: `imageArn`, `name`, `state`, `createdAt`, and a
/// top-level `nextToken` that is **absent** on the last page rather than null — which is
/// what a real final page looks like, and what the pagination loop's exit reads.
pub fn list_images_response(names: &[&str], next_token: Option<&str>) -> String {
    let items: Vec<String> = names
        .iter()
        .map(|name| {
            format!(
                r#"{{
                    "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image:{name}",
                    "name": "{name}",
                    "state": "ACTIVE",
                    "latestActiveImageVersion": "1",
                    "createdAt": 1754524800
                }}"#
            )
        })
        .collect();
    let token = match next_token {
        Some(token) => format!(r#", "nextToken": {}"#, json_string(token)),
        None => String::new(),
    };
    format!(r#"{{"items": [{}]{token}}}"#, items.join(", "))
}

/// `DeleteMicrovmImageOutput` in `DELETING`, the ordinary answer.
pub fn delete_image_response() -> String {
    delete_image_response_in("DELETING")
}

/// `DeleteMicrovmImageOutput` in `state`.
///
/// The parameter exists so a test can seed the `DELETE_FAILED` readback — a 2xx whose state
/// says the work was refused, which is the case that makes reading this shape worth doing.
pub fn delete_image_response_in(state: &str) -> String {
    format!(
        r#"{{"imageIdentifier": "arn:aws:lambda:us-east-1:123456789012:microvm-image:img",
        "state": "{state}"}}"#
    )
}

/// `SuspendMicrovmResponse`, `ResumeMicrovmResponse`, `TerminateMicrovmResponse` — all
/// three are empty structures in the model.
pub fn empty_response() -> String {
    "{}".to_string()
}

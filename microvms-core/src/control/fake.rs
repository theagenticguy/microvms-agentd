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

/// `CreateMicrovmImageResponse` for an image that has just entered `CREATING`.
pub fn create_image_response(name: &str) -> String {
    format!(
        r#"{{
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/{name}",
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
pub fn get_image_response(name: &str, state: &str) -> String {
    format!(
        r#"{{
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/{name}",
            "name": "{name}",
            "state": "{state}",
            "createdAt": 1754524800
        }}"#
    )
}

/// `ListMicrovmImageVersionsOutput` with one version.
pub fn list_versions_response(version: &str) -> String {
    format!(
        r#"{{
            "items": [{{
                "baseImageArn": "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
                "buildRoleArn": "arn:aws:iam::123456789012:role/build",
                "codeArtifact": {{"uri": "s3://bucket/img.zip"}},
                "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
                "imageVersion": "{version}",
                "state": "IN_PROGRESS",
                "status": "ACTIVE",
                "createdAt": 1754524800
            }}]
        }}"#
    )
}

/// `ListMicrovmImageBuildsOutput` whose builds are all in `build_state`.
///
/// **`buildState`**, spelled as the model spells it. This is the single most important
/// literal in this file: the Python fake wrote `state` here and that is what made the
/// stall guard unfalsifiable.
pub fn list_builds_response(build_state: &str) -> String {
    format!(
        r#"{{
            "items": [
                {{
                    "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
                    "imageVersion": "1",
                    "buildId": "build-1",
                    "buildState": "{build_state}",
                    "architecture": "ARM_64",
                    "chipset": "GRAVITON",
                    "chipsetGeneration": "1",
                    "createdAt": 1754524800
                }},
                {{
                    "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
                    "imageVersion": "1",
                    "buildId": "build-2",
                    "buildState": "{build_state}",
                    "architecture": "ARM_64",
                    "chipset": "GRAVITON",
                    "chipsetGeneration": "1",
                    "createdAt": 1754524800
                }}
            ]
        }}"#
    )
}

/// `RunMicrovmResponse`/`GetMicrovmResponse` in `state`, with an optional `stateReason`.
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
            "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
            "imageVersion": "1",
            "maximumDurationInSeconds": 3600,
            "startedAt": 1754524800{reason}
        }}"#
    )
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
                    "imageArn": "arn:aws:lambda:us-east-1:123456789012:microvm-image/{name}",
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

/// `DeleteMicrovmImageOutput`.
pub fn delete_image_response() -> String {
    r#"{"imageIdentifier": "arn:aws:lambda:us-east-1:123456789012:microvm-image/img",
        "state": "DELETING"}"#
        .to_string()
}

/// `SuspendMicrovmResponse`, `ResumeMicrovmResponse`, `TerminateMicrovmResponse` — all
/// three are empty structures in the model.
pub fn empty_response() -> String {
    "{}".to_string()
}

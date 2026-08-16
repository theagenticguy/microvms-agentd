// SPDX-License-Identifier: Apache-2.0
//! The signed rest-json transport, and the seam a test replaces it at.
//!
//! # Why this is hand-rolled
//!
//! `lambda-microvms` has no `aws-sdk-rust` crate. The alternatives were vendoring
//! smithy-rs codegen — Gradle, JDK 17, Kotlin, and a project that declares its external
//! interfaces unstable — or signing 24 rest-json operations by hand with the first-party
//! low-level crates, which is the flow the AWS SDK maintainers point at for exactly this
//! case. rest-json is also the easiest protocol to hand-roll: plain JSON bodies,
//! URI-bound operations, standard headers, no XML and no query-protocol encoding.
//!
//! # The seam, and what the fake is allowed to know
//!
//! [`Transport`] is a trait with one method. The real implementation signs and sends;
//! the test implementation is a **contract recorder** that asserts on the emitted
//! request in the shape the service model declares.
//!
//! The rule the recorder follows is the lesson from the `buildState` bug: a fake that
//! shares the client's assumptions hides exactly the class of defect the fake exists to
//! catch. So the recorder never asserts on a value the client computed, and every
//! response it hands back is **literal JSON in the model's spelling** — see
//! [`crate::control::ops`] for why that is not a round trip through this crate's own
//! serializer.
//!
//! # Local validation is not optional
//!
//! botocore's `VALIDATED_METADATA_ATTRS` is `{required, min, document, union}`, so
//! `max`, `pattern`, and `enum` violations are serialized, sent, and answered with a
//! `ValidationException` — confirmed empirically for `max` (runHookPayload 4097,
//! maximumDurationInSeconds 28801, ImageName 65 chars, clientToken 129 chars), for
//! `pattern`, and for `enum`. The same holds here with more force: **nothing** in this
//! transport reads the service model at runtime, so every constraint is checked in the
//! caller before a request is built or it is not checked at all.

use std::time::SystemTime;

use crate::error::{Error, ErrorKind};
use crate::region::Region;

/// The pinned API version, which is also the first path segment of every operation.
///
/// Read from [`crate::constants::MODEL_API_VERSION`] rather than written again, so the
/// drift gate's version check and the URLs cannot disagree: a client signing
/// `/2025-09-09/...` while claiming to implement a different model is a client whose
/// constraint checks were verified against the wrong thing.
const API_PATH_VERSION: &str = crate::constants::MODEL_API_VERSION;

/// The SigV4 signing name, from the model's `signingName`.
///
/// `lambda`, not `lambda-microvms`. The `endpointPrefix` is also `lambda`, which is why
/// an endpoint resolves for every AWS region and a client constructs happily for a region
/// that does not carry MicroVMs (TRAP-6, `crate::region`).
const SIGNING_NAME: &str = "lambda";

/// One control-plane call: method, path, optional JSON body.
///
/// A struct rather than a long parameter list because the recorder asserts on it as a
/// whole, and because `method` and `path` next to each other is how a reader checks a
/// path against the model's `http` trait.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Call {
    /// The operation name, for diagnostics and for the recorder's ledger.
    ///
    /// Carried rather than derived from the path: the recorder's TRAP-11 assertion is
    /// "no call named `CreateMicrovmShellAuthToken`", and matching that on a path is a
    /// substring test that a renamed route would slip past.
    pub operation: &'static str,
    /// `GET`, `POST`, `PATCH`, or `DELETE` — the four the model's `http` traits use for the
    /// operations this client implements.
    pub method: Method,
    /// The path **after** the host, with the API version and every URI parameter already
    /// percent-encoded. Includes the query string when the operation has one.
    pub path: String,
    /// The rest-json body, or `None` for an operation the model gives no body members.
    pub body: Option<Vec<u8>>,
}

/// The HTTP methods the model's operations use.
///
/// An enum so a call site cannot pass `"post"` and have the canonical request silently
/// disagree with the wire method.
///
/// # `Patch` is one operation's, and only one
///
/// The model uses `PATCH` for exactly `UpdateMicrovmImageVersion` and `PUT` for exactly
/// `UpdateMicrovmImage`. `Put` is deliberately absent: this client does not implement that
/// operation, and a variant nothing constructs is a variant that suggests it does.
/// `PATCH` is not interchangeable with `POST` here — a `POST` to the version path is a route
/// the service does not declare, and the answer is a 404 that reads as a missing version
/// rather than as a wrong method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

impl Call {
    /// A `GET` with no body.
    pub fn get(operation: &'static str, path: impl Into<String>) -> Self {
        Self {
            operation,
            method: Method::Get,
            path: path.into(),
            body: None,
        }
    }

    /// A `POST` carrying a serialized rest-json body.
    pub fn post_json<T: serde::Serialize>(
        operation: &'static str,
        path: impl Into<String>,
        body: &T,
    ) -> Result<Self, Error> {
        let body = serde_json::to_vec(body).map_err(|error| {
            Error::new(
                ErrorKind::Unexpected,
                format!("could not serialize the {operation} request body: {error}"),
            )
        })?;
        Ok(Self {
            operation,
            method: Method::Post,
            path: path.into(),
            body: Some(body),
        })
    }

    /// A `PATCH` carrying a serialized rest-json body.
    ///
    /// The same shape as [`Call::post_json`] and written beside it rather than folded into a
    /// method-taking constructor, for the reason [`Method`]'s docs give: the method is part of
    /// the route the model declares, and a `with_method(Method::Patch)` builder is one a call
    /// site can forget to call. `UpdateMicrovmImageVersion` is the only operation that reaches
    /// this.
    pub fn patch_json<T: serde::Serialize>(
        operation: &'static str,
        path: impl Into<String>,
        body: &T,
    ) -> Result<Self, Error> {
        let body = serde_json::to_vec(body).map_err(|error| {
            Error::new(
                ErrorKind::Unexpected,
                format!("could not serialize the {operation} request body: {error}"),
            )
        })?;
        Ok(Self {
            operation,
            method: Method::Patch,
            path: path.into(),
            body: Some(body),
        })
    }

    /// A `POST` with no body members. `SuspendMicrovm` and `ResumeMicrovm` are both this
    /// shape: their only member is the URI-located identifier.
    ///
    /// The body is `Some(b"{}")` rather than `None`, because an empty POST and a POST of
    /// `{}` are different canonical requests and rest-json services answer the former
    /// inconsistently.
    pub fn post_empty(operation: &'static str, path: impl Into<String>) -> Self {
        Self {
            operation,
            method: Method::Post,
            path: path.into(),
            body: Some(b"{}".to_vec()),
        }
    }

    /// A `DELETE` with no body.
    pub fn delete(operation: &'static str, path: impl Into<String>) -> Self {
        Self {
            operation,
            method: Method::Delete,
            path: path.into(),
            body: None,
        }
    }

    /// The body as bytes, empty when there is none — which is what gets signed.
    fn body_bytes(&self) -> &[u8] {
        self.body.as_deref().unwrap_or(&[])
    }
}

/// What the control plane answered: a status and a raw body.
///
/// Raw bytes rather than a deserialized value because the error path has to read a body
/// the success types cannot hold, and because TRAP-6's null `message` field is only
/// visible to something that did not already require a `String` there.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    /// Deserializes the body as `T`, or reports the failure with the body attached.
    ///
    /// The body goes in the message because a rest-json parse failure with the text
    /// withheld is unactionable: the reader cannot tell a renamed member from a truncated
    /// response. Truncated to 512 bytes, since a list response can be large and the first
    /// 512 characters always contain the member that failed.
    pub fn json<T: serde::de::DeserializeOwned>(&self, operation: &str) -> Result<T, Error> {
        serde_json::from_slice(&self.body).map_err(|error| {
            let text = String::from_utf8_lossy(&self.body);
            let shown: String = text.chars().take(512).collect();
            Error::new(
                ErrorKind::Platform,
                format!(
                    "could not read the {operation} response ({error}). The service model for \
                     {API_PATH_VERSION} is what this client's shapes are transcribed from, so a \
                     member that has moved is a drift-gate failure rather than a transport bug. \
                     Body: {shown}"
                ),
            )
            .with_source(error)
        })
    }

    /// Whether the status is a success. rest-json operations here answer 200 or 201.
    fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The one seam every operation goes through.
///
/// One method, so a fake is a few lines and there is no partially-overridden transport
/// whose behaviour is half real. `&self` rather than `&mut self` so the recorder can be
/// shared behind an `Arc` while a lifecycle runs.
pub trait Transport: Send + Sync {
    /// Performs `call`, returning whatever the far side answered.
    ///
    /// A transport-level failure (no status at all) is an `Err`; a 4xx or 5xx is an
    /// `Ok(Reply)` carrying the status, because classifying a status is
    /// [`classify_failure`]'s job and a transport that classified would be a second
    /// place the taxonomy lives.
    fn send(
        &self,
        call: Call,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Reply, Error>> + Send + '_>>;
}

/// Sends `call` through `transport`, retrying the retryable statuses, and turns a failure
/// status into a classified [`Error`].
///
/// # Which failures retry
///
/// 429 (`ThrottlingException`), 500 (`InternalServerException`), and any other 5xx, plus
/// a transport error that produced no status. AWS standard-mode semantics, matched by
/// hand because the smithy retry pieces are coupled to an orchestrator this client does
/// not have.
///
/// # Why retrying the mutating operations is safe
///
/// Every mutating operation this client sends carries a `clientToken`, so a retry is
/// idempotent by the service's own contract — which is the same property that makes the
/// token dangerous to derive (TRAP-1). `SuspendMicrovm`/`ResumeMicrovm`/`TerminateMicrovm`
/// take no token and are retried anyway: they are state transitions whose second
/// application is a no-op or a `ConflictException`, and a conflict is not retried.
pub async fn send_with_retry(transport: &dyn Transport, call: Call) -> Result<Reply, Error> {
    use backon::{ExponentialBuilder, Retryable};

    let operation = call.operation;
    let attempt = || {
        let call = call.clone();
        async move {
            let reply = transport.send(call).await?;
            if reply.is_success() {
                return Ok(reply);
            }
            Err(classify_failure(operation, &reply))
        }
    };

    attempt
        .retry(
            ExponentialBuilder::default()
                .with_jitter()
                .with_min_delay(std::time::Duration::from_millis(200))
                .with_max_delay(std::time::Duration::from_secs(20))
                .with_max_times(5),
        )
        .when(Error::retryable)
        .await
}

/// Turns a failure status and body into an [`Error`] of the right kind.
///
/// # The statuses are the control plane's, not the daemon's
///
/// [`crate::error::WireKind`] is the *daemon's* status discipline and deliberately has no
/// generic 4xx fallback, because the daemon's 400-versus-404 distinction is load-bearing.
/// None of that applies here: these seven statuses come from the modeled exception shapes
/// (`ValidationException` 400, `ServiceQuotaExceededException` 402,
/// `AccessDeniedException` 403, `ResourceNotFoundException` 404, `ConflictException` 409,
/// `ThrottlingException` 429, `InternalServerException` 500). So a control-plane failure
/// carries an [`ErrorKind`] and **no** `WireKind` — which is what `error.rs` already
/// documents: "`None` for every local reject and every control-plane failure — those never
/// reached the in-VM daemon".
///
/// # 403 and the null message
///
/// A 403 whose `message` is null is the unsupported-region signature (TRAP-6). The
/// message says so, because the alternative — forwarding a null as "AccessDenied: " — is
/// precisely what sends someone to audit an IAM policy that is fine.
pub fn classify_failure(operation: &str, reply: &Reply) -> Error {
    let parsed: crate::control::ops::ServiceErrorWire =
        serde_json::from_slice(&reply.body).unwrap_or_default();
    let detail = parsed.message.as_deref().unwrap_or("").trim().to_string();

    let (kind, explanation) = match reply.status {
        400 => (
            ErrorKind::Platform,
            "ValidationException — the service refused a value. Every constraint the pinned \
             service model states on a member this client sends is checked locally before the \
             call — min as well as max, pattern, and enum, because nothing else validates a \
             request here: this client signs with aws-sigv4 and sends with reqwest, so botocore's \
             VALIDATED_METADATA_ATTRS never applies to it. So reaching this status means a \
             constraint this client does not yet check, and closing it is a guard in \
             microvms-core/src/control plus a line in scripts/check-model-drift.py."
                .to_string(),
        ),
        402 => (
            ErrorKind::Platform,
            "ServiceQuotaExceededException — an account limit, not a request defect.".to_string(),
        ),
        403 if detail.is_empty() => (
            ErrorKind::Credentials,
            "AccessDeniedException with an empty or null message field. Measured 2026-08-07: this \
             is the signature of a region that does not carry MicroVMs, and it is \
             indistinguishable from a genuine IAM denial except that a real denial names the \
             principal and the action (docs/PLATFORM.md, 'Calling an unpriced region returns \
             AccessDeniedException with a null message'). Check the region before the policy."
                .to_string(),
        ),
        403 => (
            ErrorKind::Credentials,
            "AccessDeniedException, with a message — so this is a genuine denial rather than the \
             null-message region trap."
                .to_string(),
        ),
        404 => (
            ErrorKind::Platform,
            "ResourceNotFoundException — the image or MicroVM does not exist.".to_string(),
        ),
        409 => (
            ErrorKind::Platform,
            "ConflictException — the resource is in a state that forbids this call. An image in \
             CREATING refuses deletion, which is why teardown retries."
                .to_string(),
        ),
        429 => (
            ErrorKind::Retryable,
            "ThrottlingException — retry the identical request.".to_string(),
        ),
        status if status >= 500 => (
            ErrorKind::Retryable,
            "InternalServerException — a service fault, retry the identical request.".to_string(),
        ),
        _ => (
            ErrorKind::Platform,
            "an unmodeled status: the service model declares 400, 402, 403, 404, 409, 429, and \
             500 for these operations."
                .to_string(),
        ),
    };

    let said = if detail.is_empty() {
        "the service sent no message".to_string()
    } else {
        format!("the service said {detail:?}")
    };
    Error::new(
        kind,
        format!(
            "{operation} failed with HTTP {}: {said}. {explanation}",
            reply.status
        ),
    )
}

/// The real transport: resolve credentials, sign SigV4, send with reqwest.
pub struct SignedTransport {
    region: Region,
    endpoint: String,
    credentials: aws_credential_types::provider::SharedCredentialsProvider,
    http: reqwest::Client,
}

impl std::fmt::Debug for SignedTransport {
    /// Hand-written because a derived one would print the credentials provider, and the
    /// provider's own `Debug` is not something this crate controls.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedTransport")
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl SignedTransport {
    /// Resolves the default credential chain for `region` and builds the HTTP client.
    ///
    /// # Why the chain rather than explicit keys
    ///
    /// Because the credentials a developer actually has are in SSO, a credential process,
    /// or an instance profile, and re-implementing that resolution is how a client ends up
    /// working only on the machine it was written on. `aws-config`'s default chain is the
    /// whole reason to depend on it at all — the manifest turns off its HTTP client, since
    /// reqwest is the one here, but keeps `sso` and `credentials-process`.
    ///
    /// The provider is held rather than the resolved credentials: instance-profile creds
    /// are temporary, so each request re-resolves and picks up a rotation. That is why
    /// this is a provider field and not a `Credentials` field.
    pub async fn new(region: Region) -> Result<Self, Error> {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.as_str().to_string()))
            .load()
            .await;
        let credentials = config.credentials_provider().ok_or_else(|| {
            Error::new(
                ErrorKind::Credentials,
                "no credentials provider resolved. The default chain looks at environment \
                 variables, the shared config files, SSO, a credential process, then the EC2 \
                 instance metadata service; none of them answered. `aws sts get-caller-identity` \
                 is the cheapest way to see the same failure.",
            )
        })?;

        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            // Generous rather than tight: a control-plane call is not a hot path, and a
            // timeout shorter than the service's own tail latency turns a slow answer into
            // a retry storm against an operation that may not be idempotent.
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|error| {
                Error::new(
                    ErrorKind::Precondition,
                    format!("could not build the HTTP client: {error}"),
                )
                .with_source(error)
            })?;

        Ok(Self {
            endpoint: endpoint_for(&region),
            region,
            credentials,
            http,
        })
    }

    /// The URL this transport would send `call` to. Public for the test that checks a path
    /// against the model without needing credentials.
    fn url(&self, call: &Call) -> String {
        format!("{}{}", self.endpoint, call.path)
    }
}

/// `https://lambda.<region>.amazonaws.com`, from the model's `endpointPrefix`.
///
/// The prefix is `lambda` rather than `lambda-microvms`, which is why this resolves for
/// every AWS region including the ones that answer the null-message denial.
pub fn endpoint_for(region: &Region) -> String {
    format!("https://{SIGNING_NAME}.{}.amazonaws.com", region.as_str())
}

impl Transport for SignedTransport {
    fn send(
        &self,
        call: Call,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Reply, Error>> + Send + '_>>
    {
        Box::pin(async move {
            use aws_credential_types::provider::ProvideCredentials as _;

            // Re-resolved per request so an instance-profile rotation is picked up. The
            // provider caches internally, so this is not a metadata call per request.
            let credentials = self
                .credentials
                .provide_credentials()
                .await
                .map_err(|error| {
                    Error::new(
                        ErrorKind::Credentials,
                        format!(
                            "could not resolve credentials for {}: {error}. Waiting will not fix \
                             this — the identity is wrong or absent.",
                            call.operation
                        ),
                    )
                    .with_source(error)
                })?;

            let url = self.url(&call);
            let body = call.body_bytes().to_vec();

            // Built before signing, because the signature covers the headers that are on
            // it: content-type is signed, and adding one afterwards invalidates it.
            let mut request = http::Request::builder()
                .method(call.method.as_str())
                .uri(&url)
                .header("content-type", "application/json")
                .body(body.clone())
                .map_err(|error| {
                    Error::new(
                        ErrorKind::Unexpected,
                        format!("could not build the {} request: {error}", call.operation),
                    )
                    .with_source(error)
                })?;

            sign_in_place(&mut request, &credentials, &self.region)?;

            let (parts, body) = request.into_parts();
            let request = http::Request::from_parts(parts, reqwest::Body::from(body));
            let request = reqwest::Request::try_from(request).map_err(|error| {
                Error::new(
                    ErrorKind::Unexpected,
                    format!("could not convert the signed request: {error}"),
                )
                .with_source(error)
            })?;

            // A failure here produced no status, so it says nothing about service state:
            // Retryable, matching the daemon transport's own rule.
            let response = self.http.execute(request).await.map_err(|error| {
                Error::new(
                    ErrorKind::Retryable,
                    format!(
                        "the {} request to {url} did not complete: {error}",
                        call.operation
                    ),
                )
                .with_source(error)
            })?;

            let status = response.status().as_u16();
            let body = response
                .bytes()
                .await
                .map_err(|error| {
                    Error::new(
                        ErrorKind::Retryable,
                        format!(
                            "could not read the {} response body: {error}",
                            call.operation
                        ),
                    )
                    .with_source(error)
                })?
                .to_vec();

            Ok(Reply { status, body })
        })
    }
}

/// Signs `request` in place with SigV4 for `region`.
///
/// Split out so the signing step is one readable unit and so the test below can sign a
/// request with static credentials and inspect the headers, which is the only way to check
/// the signing name and region without a live call.
fn sign_in_place(
    request: &mut http::Request<Vec<u8>>,
    credentials: &aws_credential_types::Credentials,
    region: &Region,
) -> Result<(), Error> {
    use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
    use aws_sigv4::sign::v4;

    let identity = credentials.clone().into();
    let params: aws_sigv4::http_request::SigningParams = v4::SigningParams::builder()
        .identity(&identity)
        .region(region.as_str())
        .name(SIGNING_NAME)
        .time(SystemTime::now())
        .settings(SigningSettings::default())
        .build()
        .map_err(|error| {
            Error::new(
                ErrorKind::Unexpected,
                format!("could not build SigV4 signing parameters: {error}"),
            )
            .with_source(error)
        })?
        .into();

    // The headers view has to borrow from `request`, so the signable view is built and
    // consumed before the mutable borrow that applies the signature.
    let instructions = {
        let signable = SignableRequest::new(
            request.method().as_str(),
            request.uri().to_string(),
            request.headers().iter().filter_map(|(name, value)| {
                // A header whose value is not ASCII cannot be part of a canonical
                // request. This client sets only content-type, so the filter is a
                // guard against a future header rather than a live case — and dropping
                // one is correct: an unsigned header is still sent, it is just not
                // covered.
                value.to_str().ok().map(|value| (name.as_str(), value))
            }),
            // `Bytes`, never `UnsignedPayload`: non-S3 services reject an unsigned
            // payload, and an empty body still needs its empty-payload SHA256 signed.
            SignableBody::Bytes(request.body()),
        )
        .map_err(|error| {
            Error::new(
                ErrorKind::Unexpected,
                format!("could not build the signable request: {error}"),
            )
            .with_source(error)
        })?;

        sign(signable, &params)
            .map_err(|error| {
                Error::new(
                    ErrorKind::Credentials,
                    format!("SigV4 signing failed: {error}"),
                )
                .with_source(error)
            })?
            .into_parts()
            .0
    };

    // `http1x` rather than `http0x`: aws-sigv4's default features are sign-http + http1,
    // which is the version reqwest 0.13 consumes through TryFrom. Mixing the two http
    // versions would mean converting on every call.
    instructions.apply_to_request_http1x(request);
    Ok(())
}

/// The operation paths, one function per `http.requestUri` in the model.
///
/// Here rather than inlined at each call site so a path is written once and can be read
/// against the model in one place. Every URI parameter is percent-encoded, because an
/// image identifier may be an ARN — which contains `:` and `/` — and an unencoded ARN in a
/// path segment is a different resource.
pub mod paths {
    use super::API_PATH_VERSION;

    /// Percent-encodes a URI path segment.
    ///
    /// Hand-rolled against RFC 3986's unreserved set rather than pulling a dependency for
    /// it. **`/` is encoded**, which is the whole point: an image ARN's slashes would
    /// otherwise split into extra path segments and address something else. `:` is
    /// encoded too — legal unencoded in a path per the RFC, but AWS's own SDKs encode it
    /// in labels and the signature must match whatever is sent.
    pub fn encode_segment(segment: &str) -> String {
        let mut encoded = String::with_capacity(segment.len());
        for byte in segment.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    encoded.push(byte as char);
                }
                other => {
                    use std::fmt::Write as _;
                    let _ = write!(encoded, "%{other:02X}");
                }
            }
        }
        encoded
    }

    /// `POST /2025-09-09/microvm-images`
    pub fn microvm_images() -> String {
        format!("/{API_PATH_VERSION}/microvm-images")
    }

    /// `GET /2025-09-09/microvm-images`, with the listing's query members.
    ///
    /// `nameFilter` is the model's server-side **substring** filter — it narrows the
    /// listing, it does not answer "the image named X", so an exact-match comparison
    /// still happens in the caller. `nextToken` is the pagination cursor and is opaque,
    /// which is why both values go through [`encode_segment`]: an opaque token may carry
    /// `+` or `=`, and an unencoded one desynchronises the SigV4 canonical query from
    /// the query actually sent.
    pub fn microvm_images_list(name_filter: Option<&str>, next_token: Option<&str>) -> String {
        let mut query: Vec<String> = Vec::new();
        if let Some(filter) = name_filter {
            query.push(format!("nameFilter={}", encode_segment(filter)));
        }
        if let Some(token) = next_token {
            query.push(format!("nextToken={}", encode_segment(token)));
        }
        if query.is_empty() {
            return microvm_images();
        }
        format!("{}?{}", microvm_images(), query.join("&"))
    }

    /// `GET|DELETE /2025-09-09/microvm-images/{imageIdentifier}`
    pub fn microvm_image(image: &str) -> String {
        format!(
            "/{API_PATH_VERSION}/microvm-images/{}",
            encode_segment(image)
        )
    }

    /// One `nextToken=` query member, appended to `path`, or `path` unchanged.
    ///
    /// Shared by the three listings that take nothing else, so the encoding decision is
    /// made once. The value goes through [`encode_segment`] for the reason
    /// [`microvm_images_list`] states: a pagination token is **opaque**, so it may carry
    /// `+`, `/`, or `=`, and an unencoded one desynchronises the SigV4 canonical query
    /// from the query actually sent — a rejection that reads like bad credentials rather
    /// than like a malformed URL.
    fn with_next_token(path: String, next_token: Option<&str>) -> String {
        match next_token {
            Some(token) => format!("{path}?nextToken={}", encode_segment(token)),
            None => path,
        }
    }

    /// `GET /2025-09-09/microvm-images/{imageIdentifier}/versions`, with the pagination
    /// cursor when there is one.
    ///
    /// `maxResults` is not sent: the model caps it at 50 and the service's own default is
    /// what a caller reading every page wants, so naming a page size would only be a
    /// second number to keep right. [`image_versions_paged`] is the one caller that does
    /// name one, and says why.
    pub fn image_versions(image: &str, next_token: Option<&str>) -> String {
        image_versions_paged(image, next_token, None)
    }

    /// [`image_versions`] with an explicit `maxResults`, which **no production path sends**.
    ///
    /// # Why a page size exists at all when nothing ships one
    ///
    /// Because without it the cursor encoding is unfalsifiable against the real service,
    /// and that encoding is the one place in this module where being wrong reads as a
    /// *credentials* failure rather than a URL one. A real `nextToken` is opaque base64
    /// carrying `+`, `/`, and `=`; an unencoded one desynchronises the SigV4 canonical
    /// query and the service answers 403. Every test that could catch it went through an
    /// injected transport, so the fake minted the token and the signer never saw it.
    ///
    /// Forcing a real cursor needs a listing with more items than one page, and the
    /// service's default page is larger than any image's version count here — so the only
    /// way to make the real service mint a real cursor for a resource that already exists
    /// is to ask for a smaller page. That is what this is for, and
    /// `microvms-core/tests/live_pagination.rs` is its caller.
    ///
    /// # Why it is not simply a parameter on [`image_versions`]
    ///
    /// A page size on the production path is a second number to keep right, and a wrong
    /// one is invisible: reading every page with `maxResults=1` is correct and slow, which
    /// is the kind of defect that survives. Keeping it to a separately named function means
    /// the production call sites cannot acquire one by accident, and a reader of
    /// [`image_versions`] sees no page size to wonder about.
    ///
    /// `max_results` is clamped to the model's `1..=50`, because a value outside it is a
    /// `ValidationException` and a caller of this function is trying to observe pagination
    /// rather than to discover the bound.
    pub fn image_versions_paged(
        image: &str,
        next_token: Option<&str>,
        max_results: Option<u32>,
    ) -> String {
        let mut query: Vec<String> = Vec::new();
        if let Some(size) = max_results {
            query.push(format!("maxResults={}", size.clamp(1, 50)));
        }
        if let Some(token) = next_token {
            query.push(format!("nextToken={}", encode_segment(token)));
        }
        let path = format!("{}/versions", microvm_image(image));
        if query.is_empty() {
            return path;
        }
        format!("{path}?{}", query.join("&"))
    }

    /// `GET|PATCH|DELETE /2025-09-09/microvm-images/{imageIdentifier}/versions/{imageVersion}`
    ///
    /// One path for three operations, because the model gives them one `requestUri` and the
    /// method is what distinguishes them: `GetMicrovmImageVersion` reads it,
    /// `UpdateMicrovmImageVersion` patches it, `DeleteMicrovmImageVersion` deletes it. The
    /// method lives on the [`Call`] rather than in three path functions, so a reader checking
    /// this against `service-2.json` compares one string.
    ///
    /// Built from [`microvm_image`] rather than from [`image_versions`], because that one
    /// now takes a cursor and none of the three may carry a query member.
    pub fn image_version(image: &str, version: &str) -> String {
        format!(
            "{}/versions/{}",
            microvm_image(image),
            encode_segment(version)
        )
    }

    /// `GET /2025-09-09/microvm-images/{imageIdentifier}/versions/{imageVersion}/builds/{buildId}`
    ///
    /// The list path plus one segment. `buildId` is percent-encoded like every other URI
    /// parameter even though a real one is a UUID: the model types it `NonBlankString`, so it
    /// is not the encoder's business to assume the shape of a value the service mints.
    pub fn image_build(image: &str, version: &str, build_id: &str) -> String {
        format!(
            "{}/builds/{}",
            image_version(image, version),
            encode_segment(build_id)
        )
    }

    /// `GET /2025-09-09/managed-microvm-images`, with the pagination cursor when there is one.
    ///
    /// A **different route** from [`microvm_images`], not a filter on it: the managed bases
    /// live under `managed-microvm-images` and their summaries carry a different shape
    /// (`ManagedMicrovmImageSummary`, three members). Reading the account's own image listing
    /// would never return one.
    pub fn managed_microvm_images(next_token: Option<&str>) -> String {
        with_next_token(
            format!("/{API_PATH_VERSION}/managed-microvm-images"),
            next_token,
        )
    }

    /// `GET /2025-09-09/managed-microvm-images/{imageIdentifier}/versions`, with the cursor.
    ///
    /// `imageIdentifier` here is the managed base's **full ARN**, not its name: measured
    /// 2026-08-16, `al2023-1` alone answers `ValidationException: Invalid ARN format:
    /// al2023-1`. That is why [`crate::control::BaseImage::arn`] is what the caller passes —
    /// and why the wrapper refuses a non-ARN locally, since the rejection names the value
    /// without saying which member wanted an ARN.
    pub fn managed_image_versions(image: &str, next_token: Option<&str>) -> String {
        with_next_token(
            format!(
                "/{API_PATH_VERSION}/managed-microvm-images/{}/versions",
                encode_segment(image)
            ),
            next_token,
        )
    }

    /// `GET /2025-09-09/microvm-images/{imageIdentifier}/versions/{imageVersion}/builds`,
    /// with the pagination cursor when there is one.
    pub fn image_builds(image: &str, version: &str, next_token: Option<&str>) -> String {
        with_next_token(
            format!("{}/builds", image_version(image, version)),
            next_token,
        )
    }

    /// `POST /2025-09-09/microvms`
    ///
    /// The bare collection path, which is what `RunMicrovm` posts to. `ListMicrovms` reads
    /// the same route and lives in [`microvms_list`] rather than here, so a launch cannot
    /// accidentally carry a `nextToken` — the same split [`microvm_images`] and
    /// [`microvm_images_list`] already make.
    pub fn microvms() -> String {
        format!("/{API_PATH_VERSION}/microvms")
    }

    /// `GET /2025-09-09/microvms`, with the pagination cursor when there is one.
    pub fn microvms_list(next_token: Option<&str>) -> String {
        with_next_token(microvms(), next_token)
    }

    /// `GET|DELETE /2025-09-09/microvms/{microvmIdentifier}`
    pub fn microvm(id: &str) -> String {
        format!("/{API_PATH_VERSION}/microvms/{}", encode_segment(id))
    }

    /// `POST /2025-09-09/microvms/{microvmIdentifier}/suspend`
    pub fn suspend(id: &str) -> String {
        format!("{}/suspend", microvm(id))
    }

    /// `POST /2025-09-09/microvms/{microvmIdentifier}/resume`
    pub fn resume(id: &str) -> String {
        format!("{}/resume", microvm(id))
    }

    /// `POST /2025-09-09/microvms/{microvmIdentifier}/auth-token`
    pub fn auth_token(id: &str) -> String {
        format!("{}/auth-token", microvm(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression that blocked the whole live tier: with aws-config's
    /// `default-https-client` off, `load()` panics with "a http_client is
    /// required" before any credential question is even asked. Constructing
    /// through the real chain must yield a `Result` — either outcome is fine
    /// here (this host may or may not have credentials); a panic is the bug.
    /// IMDS is the only endpoint this can touch: link-local, free, and fast.
    #[tokio::test]
    async fn constructing_the_real_transport_returns_a_result_rather_than_panicking() {
        let _ = SignedTransport::new(Region::UsEast1).await;
    }

    /// The endpoint, from the model's `endpointPrefix`. `lambda` and not
    /// `lambda-microvms` — which is the fact that makes TRAP-6 possible, since this
    /// resolves for every AWS region.
    #[test]
    fn the_endpoint_uses_the_models_lambda_prefix_for_every_region() {
        assert_eq!(
            endpoint_for(&Region::UsEast1),
            "https://lambda.us-east-1.amazonaws.com"
        );
        assert_eq!(
            endpoint_for(&Region::ApNortheast1),
            "https://lambda.ap-northeast-1.amazonaws.com"
        );
        // Including one that does not carry MicroVMs, which is the point.
        assert_eq!(
            endpoint_for(&Region::unlisted("eu-central-1")),
            "https://lambda.eu-central-1.amazonaws.com"
        );
    }

    /// Every path against the model's `http.requestUri`, as literals. Transcribed from
    /// `service-2.json` rather than built from a template, because a template shared with
    /// the code under test would agree with a wrong template.
    #[test]
    fn every_path_matches_the_models_request_uri() {
        assert_eq!(paths::microvm_images(), "/2025-09-09/microvm-images");
        assert_eq!(
            paths::microvm_image("img"),
            "/2025-09-09/microvm-images/img"
        );
        assert_eq!(
            paths::image_versions("img", None),
            "/2025-09-09/microvm-images/img/versions"
        );
        assert_eq!(
            paths::image_version("img", "1"),
            "/2025-09-09/microvm-images/img/versions/1"
        );
        assert_eq!(
            paths::image_builds("img", "1", None),
            "/2025-09-09/microvm-images/img/versions/1/builds"
        );
        assert_eq!(
            paths::image_build("img", "1", "build-abc"),
            "/2025-09-09/microvm-images/img/versions/1/builds/build-abc"
        );
        assert_eq!(
            paths::managed_microvm_images(None),
            "/2025-09-09/managed-microvm-images"
        );
        assert_eq!(
            paths::managed_image_versions(
                "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
                None
            ),
            "/2025-09-09/managed-microvm-images/\
             arn%3Aaws%3Alambda%3Aus-east-1%3Aaws%3Amicrovm-image%3Aal2023-1/versions"
        );
        assert_eq!(paths::microvms(), "/2025-09-09/microvms");
        assert_eq!(paths::microvms_list(None), "/2025-09-09/microvms");
        assert_eq!(paths::microvm("mvm-1"), "/2025-09-09/microvms/mvm-1");
        assert_eq!(
            paths::suspend("mvm-1"),
            "/2025-09-09/microvms/mvm-1/suspend"
        );
        assert_eq!(paths::resume("mvm-1"), "/2025-09-09/microvms/mvm-1/resume");
        assert_eq!(
            paths::auth_token("mvm-1"),
            "/2025-09-09/microvms/mvm-1/auth-token"
        );
    }

    /// The listing path carries its query members in the model's spelling, percent-encoded,
    /// and a bare listing is the plain collection path with no `?`.
    ///
    /// An opaque `nextToken` may carry `+`, `/`, or `=`; unencoded, those desynchronise the
    /// SigV4 canonical query from the query actually sent, and the rejection reads like bad
    /// credentials.
    #[test]
    fn the_image_listing_path_encodes_its_query_members() {
        assert_eq!(
            paths::microvm_images_list(None, None),
            "/2025-09-09/microvm-images"
        );
        assert_eq!(
            paths::microvm_images_list(Some("my-image"), None),
            "/2025-09-09/microvm-images?nameFilter=my-image"
        );
        assert_eq!(
            paths::microvm_images_list(Some("my-image"), Some("a+b/c=")),
            "/2025-09-09/microvm-images?nameFilter=my-image&nextToken=a%2Bb%2Fc%3D"
        );
        assert_eq!(
            paths::microvm_images_list(None, Some("token")),
            "/2025-09-09/microvm-images?nextToken=token"
        );
    }

    /// The other three paginated listings encode their cursor the same way, and emit no
    /// `?` at all without one.
    ///
    /// Written as literals per listing rather than as a loop over a helper, because the
    /// bug this closes is a path that forgets the cursor entirely — and a loop over the
    /// helper would pass against three call sites that never pass one.
    ///
    /// **Falsification** — drop the [`paths::encode_segment`] call from
    /// `with_next_token` and every `%2B`/`%2F`/`%3D` assertion below goes red, which is
    /// the SigV4 canonical-query desynchronisation this encoder exists to prevent.
    #[test]
    fn the_other_paginated_listings_encode_their_cursor_too() {
        assert_eq!(
            paths::image_versions("img", Some("a+b/c=")),
            "/2025-09-09/microvm-images/img/versions?nextToken=a%2Bb%2Fc%3D"
        );
        assert_eq!(
            paths::image_builds("img", "1", Some("a+b/c=")),
            "/2025-09-09/microvm-images/img/versions/1/builds?nextToken=a%2Bb%2Fc%3D"
        );
        assert_eq!(
            paths::microvms_list(Some("a+b/c=")),
            "/2025-09-09/microvms?nextToken=a%2Bb%2Fc%3D"
        );

        // No cursor means no query string, not an empty one: `?nextToken=` is a member
        // present and blank, which is a different request from a member absent.
        for path in [
            paths::image_versions("img", None),
            paths::image_builds("img", "1", None),
            paths::microvms_list(None),
        ] {
            assert!(
                !path.contains('?'),
                "an absent cursor emits no query: {path}"
            );
        }
    }

    /// The version listing's explicit page size: ordered before the cursor, clamped to the
    /// model's `1..=50`, and absent from the path when nothing asks for one.
    ///
    /// The clamp is asserted at both ends because a value outside the bound is a
    /// `ValidationException`, and the only caller of this function
    /// (`tests/live_pagination.rs`) is trying to *observe* a real cursor — a run that
    /// failed on the page size instead would prove nothing about the encoding it came for.
    ///
    /// **Falsification** — remove the `.clamp(1, 50)` and the `0` and `999` cases go red
    /// with `maxResults=0` and `maxResults=999`, both of which the service refuses.
    #[test]
    fn the_version_listings_explicit_page_size_is_ordered_and_clamped() {
        assert_eq!(
            paths::image_versions_paged("img", None, Some(1)),
            "/2025-09-09/microvm-images/img/versions?maxResults=1"
        );
        // Both members, and `maxResults` first — the order the query is built in, which is
        // asserted so a reordering is a visible change rather than a silent one.
        assert_eq!(
            paths::image_versions_paged("img", Some("a+b/c="), Some(2)),
            "/2025-09-09/microvm-images/img/versions?maxResults=2&nextToken=a%2Bb%2Fc%3D"
        );
        assert_eq!(
            paths::image_versions_paged("img", None, Some(0)),
            "/2025-09-09/microvm-images/img/versions?maxResults=1",
            "the model's min is 1; a 0 would be refused"
        );
        assert_eq!(
            paths::image_versions_paged("img", None, Some(999)),
            "/2025-09-09/microvm-images/img/versions?maxResults=50",
            "the model's max is 50; a larger value would be refused"
        );

        // And the production spelling is unchanged by the new one existing: `image_versions`
        // delegates here with no page size, so it must still emit exactly what it did.
        assert_eq!(
            paths::image_versions_paged("img", None, None),
            paths::image_versions("img", None)
        );
        assert!(
            !paths::image_versions("img", Some("t")).contains("maxResults"),
            "no production path names a page size: {}",
            paths::image_versions("img", Some("t"))
        );
    }

    /// The **managed** routes are a different collection from the account's own, and their
    /// cursor is encoded the same way.
    ///
    /// Asserted as its own case because the plausible mistake is treating the managed listing
    /// as a filter on `microvm-images` — it is a separate `requestUri` in the model with a
    /// different response shape, so a path built from [`paths::microvm_images`] would address
    /// the account's images and answer 200 with the wrong items.
    ///
    /// **Falsification** — drop the `managed-` prefix from either function and the two
    /// literals below go red while every request still succeeds against the real service,
    /// which is exactly why these are literals transcribed from the model rather than a
    /// shared template.
    #[test]
    fn the_managed_routes_are_their_own_collection_and_encode_their_cursor() {
        assert!(
            !paths::managed_microvm_images(None).contains('?'),
            "an absent cursor emits no query member"
        );
        assert_eq!(
            paths::managed_microvm_images(Some("a+b/c=")),
            "/2025-09-09/managed-microvm-images?nextToken=a%2Bb%2Fc%3D"
        );
        assert_eq!(
            paths::managed_image_versions("al2023-1", Some("a+b/c=")),
            "/2025-09-09/managed-microvm-images/al2023-1/versions?nextToken=a%2Bb%2Fc%3D"
        );

        // Not a filter on the account's own listing: the two collections are different path
        // segments, and reading one for the other answers 200 with the wrong items.
        assert_ne!(
            paths::managed_microvm_images(None),
            paths::microvm_images_list(None, None)
        );
        assert!(
            !paths::microvm_images().contains("managed"),
            "the account's own collection must not acquire the managed prefix"
        );
    }

    /// A `GET` of one build is the list path plus one encoded segment, and it carries no query
    /// member.
    ///
    /// The `buildId` is percent-encoded even though a real one is a UUID: the model types it
    /// `NonBlankString`, so the encoder does not get to assume the shape of a value the
    /// service mints.
    #[test]
    fn one_builds_path_is_the_listing_plus_an_encoded_segment() {
        let path = paths::image_build("img", "1", "4a4c5e30-811f-47fa-9893-260ea6a37a8f");
        assert_eq!(
            path,
            "/2025-09-09/microvm-images/img/versions/1/builds/4a4c5e30-811f-47fa-9893-260ea6a37a8f"
        );
        assert!(!path.contains('?'), "{path}");
        assert!(
            path.starts_with(&paths::image_builds("img", "1", None)),
            "the get path must extend the list path rather than be a second route: {path}"
        );
        // And an identifier with a separator in it cannot split into extra segments.
        assert_eq!(
            paths::image_build("img", "1", "a/b"),
            "/2025-09-09/microvm-images/img/versions/1/builds/a%2Fb"
        );
    }

    /// A `DELETE` of one version carries no query member, even though its sibling
    /// [`paths::image_versions`] now takes a cursor. A version path with `?nextToken=`
    /// appended addresses nothing.
    #[test]
    fn deleting_one_version_carries_no_pagination_cursor() {
        let path = paths::image_version("img", "1");
        assert_eq!(path, "/2025-09-09/microvm-images/img/versions/1");
        assert!(!path.contains('?'), "{path}");
        assert!(!path.contains("nextToken"), "{path}");
    }

    /// A **real** image ARN in a URI parameter is percent-encoded, every colon included.
    /// `MicrovmImageIdentifier` is documented as "ARN or ID", so this is the ordinary case
    /// rather than an edge one.
    ///
    /// The literal is the shape the service actually returns, measured 2026-08-15 in
    /// us-east-1: `microvm-image:<name>` with a **colon**, which is also the only form the
    /// model's `TaggableResource` pattern admits. This test used to hold
    /// `microvm-image/img`, and the slash was not a harmless stand-in — `GetMicrovmImage`
    /// on that form answers `AccessDeniedException`, because IAM evaluates a malformed ARN
    /// as a resource with no matching policy.
    #[test]
    fn a_real_arn_identifier_is_percent_encoded_colons_and_all() {
        let arn = "arn:aws:lambda:us-east-1:123456789012:microvm-image:img";
        let path = paths::microvm_image(arn);
        assert_eq!(
            path,
            "/2025-09-09/microvm-images/arn%3Aaws%3Alambda%3Aus-east-1%3A123456789012%3Amicrovm-image%3Aimg"
        );
        assert!(
            !path["/2025-09-09/microvm-images/".len()..].contains(':'),
            "an unencoded colon in a path segment is a signature mismatch: {path}"
        );
    }

    /// The encoder still encodes `/`, which is the property that matters even though a real
    /// image ARN carries none.
    ///
    /// Kept as its own case rather than folded into the test above, because the input here
    /// is a *shape the service does not use* and labelling it "an ARN" is what let the slash
    /// form look load-bearing for twelve fakes. An unencoded slash would split into extra
    /// path segments addressing something else, and other identifiers this encoder sees
    /// (`imageVersion`, a `nextToken` cursor) genuinely can contain one.
    #[test]
    fn the_segment_encoder_encodes_slashes_wherever_they_come_from() {
        assert_eq!(paths::encode_segment("a/b"), "a%2Fb");
        let path = paths::microvm_image("has/a/slash");
        assert_eq!(path, "/2025-09-09/microvm-images/has%2Fa%2Fslash");
        assert!(
            !path["/2025-09-09/microvm-images/".len()..].contains('/'),
            "an encoded identifier contributes no path separators: {path}"
        );
    }

    /// The API version in the path is the constant the drift gate checks, not a second
    /// literal. Two copies of `2025-09-09` are two things to update, and the one that gets
    /// missed is the one nothing compiles against.
    #[test]
    fn the_path_version_is_the_pinned_model_version() {
        assert_eq!(API_PATH_VERSION, crate::constants::MODEL_API_VERSION);
        assert!(paths::microvms().starts_with("/2025-09-09/"));
    }

    /// The signing name is `lambda`, from the model's `signingName`. A signature computed
    /// for `lambda-microvms` is rejected in a way that reads like bad credentials.
    #[test]
    fn the_signing_name_is_lambda_not_lambda_microvms() {
        assert_eq!(SIGNING_NAME, "lambda");
    }

    /// Signing puts an `authorization` header on the request naming the region and the
    /// signing name, and an `x-amz-security-token` when the credentials are temporary.
    ///
    /// Static credentials rather than a resolved chain, so this runs with no AWS
    /// configuration and no network — the signature is a pure function of the inputs.
    #[test]
    fn signing_names_the_region_and_the_service_in_the_credential_scope() {
        let credentials = aws_credential_types::Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            Some("session-token".to_string()),
            None,
            "test",
        );
        let mut request = http::Request::builder()
            .method("POST")
            .uri("https://lambda.eu-west-1.amazonaws.com/2025-09-09/microvms")
            .header("content-type", "application/json")
            .body(b"{}".to_vec())
            .expect("builds");

        sign_in_place(&mut request, &credentials, &Region::EuWest1).expect("signs");

        let headers = request.headers();
        let authorization = headers
            .get("authorization")
            .expect("a signature was applied")
            .to_str()
            .expect("ascii");
        assert!(
            authorization.starts_with("AWS4-HMAC-SHA256 "),
            "{authorization}"
        );
        assert!(
            authorization.contains("/eu-west-1/lambda/aws4_request"),
            "the credential scope must name the request region and the signing name: \
             {authorization}"
        );
        assert!(
            headers.contains_key("x-amz-date"),
            "a signed date is required"
        );
        assert!(
            headers.contains_key("x-amz-security-token"),
            "temporary credentials carry a session token in the canonical request"
        );
    }

    /// A `GET` with no body still gets signed, with the empty-payload SHA256 —
    /// `SignableBody::Bytes(&[])` rather than `UnsignedPayload`, which non-S3 services
    /// reject.
    #[test]
    fn a_bodyless_get_is_signed_rather_than_sent_unsigned() {
        let credentials =
            aws_credential_types::Credentials::new("AKIDEXAMPLE", "secret", None, None, "test");
        let mut request = http::Request::builder()
            .method("GET")
            .uri("https://lambda.us-east-1.amazonaws.com/2025-09-09/microvms/mvm-1")
            .body(Vec::new())
            .expect("builds");

        sign_in_place(&mut request, &credentials, &Region::UsEast1).expect("signs");
        assert!(request.headers().contains_key("authorization"));
        assert!(
            !request.headers().contains_key("x-amz-security-token"),
            "static credentials carry no session token"
        );
    }

    /// TRAP-6's diagnostic, on the exact response shape the trap produces: 403 with a
    /// null message. The message has to name the region cause *and* say the field was
    /// null, because "AccessDeniedException" alone is what sends a reader to the IAM
    /// console.
    #[test]
    fn a_null_message_access_denied_names_the_region_trap() {
        let reply = Reply {
            status: 403,
            body: br#"{"message": null}"#.to_vec(),
        };
        let error = classify_failure("ListMicrovms", &reply);
        assert_eq!(error.kind(), ErrorKind::Credentials);
        let message = error.to_string();
        assert!(message.contains("null"), "{message}");
        assert!(message.contains("region"), "{message}");
        assert!(message.contains("does not carry MicroVMs"), "{message}");
        assert!(
            message.contains("Check the region before the policy"),
            "the remedy has to be the region, not the policy: {message}"
        );
    }

    /// A 403 *with* a message is a genuine denial and says so, so the two are not
    /// conflated. A classifier that named the region trap on every 403 would be as
    /// misleading in the other direction.
    #[test]
    fn a_403_with_a_message_is_reported_as_a_genuine_denial() {
        let reply = Reply {
            status: 403,
            body: br#"{"message": "User: arn:aws:iam::1:user/u is not authorized"}"#.to_vec(),
        };
        let error = classify_failure("RunMicrovm", &reply);
        assert_eq!(error.kind(), ErrorKind::Credentials);
        let message = error.to_string();
        assert!(message.contains("not authorized"), "{message}");
        assert!(
            message.contains("genuine denial"),
            "must distinguish itself from the null-message case: {message}"
        );
        assert!(
            !message.contains("does not carry MicroVMs"),
            "must not blame the region when the service named a principal: {message}"
        );
    }

    /// Only the throttle and the 5xx family are retryable. A retried 409 would spin
    /// against an image in CREATING, and a retried 400 would send the same rejected value
    /// five more times.
    #[test]
    fn only_throttles_and_server_faults_are_retryable() {
        let cases = [
            (400, false),
            (402, false),
            (403, false),
            (404, false),
            (409, false),
            (429, true),
            (500, true),
            (502, true),
            (503, true),
        ];
        for (status, retryable) in cases {
            let reply = Reply {
                status,
                body: br#"{"message": "detail"}"#.to_vec(),
            };
            let error = classify_failure("GetMicrovm", &reply);
            assert_eq!(error.retryable(), retryable, "HTTP {status}");
        }
    }

    /// A control-plane failure carries **no** `WireKind`: nothing reached the in-VM
    /// daemon, so there is no daemon-chosen status to report and the CLI's `data.kind`
    /// must be absent rather than invented.
    #[test]
    fn a_control_plane_failure_carries_no_daemon_wire_kind() {
        for status in [400, 403, 404, 409, 429, 500] {
            let reply = Reply {
                status,
                body: Vec::new(),
            };
            assert_eq!(
                classify_failure("GetMicrovm", &reply).wire_kind(),
                None,
                "HTTP {status} never reached the daemon"
            );
        }
    }

    /// An unparseable error body still classifies by status rather than failing. A service
    /// answering an HTML error page or a truncated body must not turn a 429 into an
    /// unexpected error, which would stop the retry.
    #[test]
    fn an_unparseable_error_body_still_classifies_by_status() {
        let reply = Reply {
            status: 429,
            body: b"<html>Too Many Requests</html>".to_vec(),
        };
        let error = classify_failure("RunMicrovm", &reply);
        assert!(error.retryable(), "a throttle is a throttle");
        assert!(error.to_string().contains("no message"), "{error}");
    }

    /// A body-parse failure names the operation and shows the body, because a rest-json
    /// parse error with the text withheld cannot distinguish a renamed member from a
    /// truncated response.
    #[test]
    fn a_response_that_does_not_parse_reports_the_body() {
        let reply = Reply {
            status: 200,
            body: br#"{"microvmId": 42}"#.to_vec(),
        };
        let error = reply
            .json::<crate::control::ops::MicrovmResponseWire>("GetMicrovm")
            .expect_err("microvmId is a string shape");
        let message = error.to_string();
        assert!(message.contains("GetMicrovm"), "{message}");
        assert!(message.contains("microvmId"), "{message}");
        assert_eq!(error.kind(), ErrorKind::Platform);
    }

    /// A `POST` with no body members sends `{}` rather than nothing: an empty POST and a
    /// POST of `{}` are different canonical requests, and rest-json services answer the
    /// former inconsistently.
    #[test]
    fn a_bodyless_post_sends_an_empty_json_object() {
        let call = Call::post_empty("SuspendMicrovm", paths::suspend("mvm-1"));
        assert_eq!(call.body.as_deref(), Some(&b"{}"[..]));
        assert_eq!(call.method, Method::Post);
    }

    /// A `GET` and a `DELETE` carry no body at all, and the signed empty payload is what
    /// `body_bytes` yields.
    #[test]
    fn a_get_and_a_delete_carry_no_body() {
        let get = Call::get("GetMicrovm", paths::microvm("mvm-1"));
        assert_eq!(get.body, None);
        assert_eq!(get.body_bytes(), b"");

        let delete = Call::delete("TerminateMicrovm", paths::microvm("mvm-1"));
        assert_eq!(delete.body, None);
        assert_eq!(delete.method, Method::Delete);
    }

    /// The methods, spelled as the canonical request needs them: uppercase.
    #[test]
    fn the_methods_are_the_ones_the_model_uses_spelled_uppercase() {
        assert_eq!(Method::Get.as_str(), "GET");
        assert_eq!(Method::Post.as_str(), "POST");
        assert_eq!(Method::Patch.as_str(), "PATCH");
        assert_eq!(Method::Delete.as_str(), "DELETE");
    }

    /// A `PATCH` carries its serialized body and is **not** a `POST`.
    ///
    /// The distinction is load-bearing rather than cosmetic: `UpdateMicrovmImageVersion` is the
    /// model's only `PATCH`, and a `POST` to the version path is a route the service does not
    /// declare — so the failure would be a 404 that reads as a missing version rather than as a
    /// wrong method. The body assertion is here too, because a `PATCH` whose body was dropped
    /// would be a request with no `status` member at all.
    ///
    /// **Falsification** — set `method: Method::Post` in `patch_json` and the method assertion
    /// goes red; the path and body assertions would still pass, which is why the method is
    /// asserted separately.
    #[test]
    fn a_patch_carries_its_body_and_is_not_a_post() {
        let call = Call::patch_json(
            "UpdateMicrovmImageVersion",
            paths::image_version("img", "2.0"),
            &crate::control::ops::UpdateImageVersionWire {
                status: crate::control::ops::VersionStatus::Inactive,
            },
        )
        .expect("serialises");

        assert_eq!(call.method, Method::Patch);
        assert_ne!(
            call.method,
            Method::Post,
            "a POST to the version path is a route the model does not declare"
        );
        assert_eq!(call.path, "/2025-09-09/microvm-images/img/versions/2.0");
        assert_eq!(
            call.body.as_deref(),
            Some(&br#"{"status":"INACTIVE"}"#[..]),
            "the one member the request has must reach the body"
        );
        assert_eq!(call.operation, "UpdateMicrovmImageVersion");
    }

    /// A `PATCH` gets signed like every other method, with the empty-payload rule not applying
    /// because it has a body.
    ///
    /// Worth its own case because the signing path reads the method as a string for the
    /// canonical request: a method the enum spells and the signer does not is a signature over
    /// a different request than the one sent, which reads as bad credentials.
    #[test]
    fn a_patch_is_signed_with_its_method_in_the_canonical_request() {
        let credentials =
            aws_credential_types::Credentials::new("AKIDEXAMPLE", "secret", None, None, "test");
        let mut request = http::Request::builder()
            .method(Method::Patch.as_str())
            .uri(
                "https://lambda.us-east-1.amazonaws.com/2025-09-09/microvm-images/img/versions/2.0",
            )
            .header("content-type", "application/json")
            .body(br#"{"status":"INACTIVE"}"#.to_vec())
            .expect("builds");

        sign_in_place(&mut request, &credentials, &Region::UsEast1).expect("signs");
        assert_eq!(request.method().as_str(), "PATCH");
        assert!(request.headers().contains_key("authorization"));
    }
}

// SPDX-License-Identifier: Apache-2.0
//! The one door to AWS, as a trait — which is what makes CLI-2's behavioral guard possible.
//!
//! # Why a trait, where the Python had monkeypatching
//!
//! `cli.py` names two module-level factories, `open_sandbox` (`cli.py:645`) and
//! `attach_session` (`cli.py:661`), and its behavioral guard patches both to raise. Its own
//! comment says why the *names* matter rather than just the failure: a handler that
//! constructed its own `Sandbox(...)` would still fail the patched test — the library's
//! client factory is patched too — so "it failed" and even "it failed with the sentinel"
//! both stay true while the seam has been bypassed. What distinguishes them is whether the
//! CLI-level seam was *entered*, which is what `test_cli.py:398` asserts.
//!
//! Rust has no monkeypatching, so the seam is a trait every handler takes by `&dyn`. That
//! is strictly better than the thing it replaces: the fake is passed in rather than swapped
//! into a module, there is no global mutable state, and a handler that wanted to reach
//! around it would have to name a core constructor — which the static guard forbids by
//! asserting that `ControlPlane::new`, `Sandbox::new`, and `Session::direct` appear nowhere
//! outside this file.
//!
//! # There are exactly three methods, and each is a whole capability
//!
//! `control_plane`, `open_sandbox`, `attach_session`. Not one method per operation: a seam
//! with twenty methods is a seam a handler can be half-way through, and the guard's
//! question is "did this command reach AWS through the library at all". Three, because they
//! are the three shapes — a plane for the read-only listing calls, a sandbox for the
//! lifecycle, a session for a VM this invocation did not launch.
//!
//! # `Region` resolution lives here too
//!
//! Because it must agree with the SDK the library calls: `AWS_REGION` then
//! `AWS_DEFAULT_REGION` then `us-east-1`, which is boto3's own order and
//! `aws-config`'s. A CLI that resolved differently would derive a connector ARN for one
//! region and a client for another, and the failure reads as a malformed connector rather
//! than as a region disagreement.
//!
//! (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)

use std::path::PathBuf;
use std::sync::Arc;

use futures_util_shim::BoxFuture;
use microvms_core::control::ControlPlane;
use microvms_core::sandbox::Sandbox;
use microvms_core::session::Session;
use microvms_core::{Error, ErrorKind, Region};

/// A boxed future, spelled locally rather than by depending on `futures-util`.
///
/// One type alias against one more direct dependency in a manifest whose exact contents are
/// the requirement under test. `futures-util` is harmless, but the guard asserts an exact
/// set and every addition to that set has to be worth writing a paragraph about — this one
/// is not.
pub mod futures_util_shim {
    /// A pinned, boxed, `Send` future — the shape a `&dyn` trait method must return.
    pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
}

/// Which of the three doors a command went through, for the guard's third assertion.
///
/// `cfg(test)`: production has no reason to ask which door it went through, and a field on
/// [`AwsSeam`] recording it would be state the shipped binary maintains for a test's benefit.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Door {
    ControlPlane,
    OpenSandbox,
    AttachSession,
}

#[cfg(test)]
impl Door {
    pub fn as_str(self) -> &'static str {
        match self {
            Door::ControlPlane => "control_plane",
            Door::OpenSandbox => "open_sandbox",
            Door::AttachSession => "attach_session",
        }
    }
}

/// What a command needs to attach to a VM it did not launch.
///
/// A struct rather than four parameters because three of the four are opaque identifiers of
/// the same shape, and `attach_session(a, b, c)` is how an endpoint ends up where a token
/// was meant.
#[derive(Clone, Debug)]
pub struct Attach {
    pub endpoint: String,
    pub agent_token: String,
    pub microvm_id: String,
    pub port: Option<u16>,
}

/// The one door to AWS.
///
/// `&dyn` rather than a generic parameter, so a handler signature does not change shape
/// between production and the guard — a monomorphised handler is a second compiled handler,
/// and then the guard is testing a copy.
pub trait CoreSeam: Send + Sync {
    /// A control plane for the read-only calls (`ls --remote`, a state read).
    fn control_plane(&self, region: Region) -> BoxFuture<'_, Result<ControlPlane, Error>>;

    /// A sandbox with nothing launched, for the lifecycle commands.
    fn open_sandbox(
        &self,
        region: Region,
        port: Option<u16>,
    ) -> BoxFuture<'_, Result<Sandbox, Error>>;

    /// A session for a VM this invocation did not launch.
    fn attach_session(
        &self,
        region: Region,
        attach: Attach,
    ) -> BoxFuture<'_, Result<Session, Error>>;

    /// Puts `bytes` at `uri`, or explains why it cannot.
    ///
    /// # The one capability core does not have, and why this is not a workaround
    ///
    /// `CreateMicrovmImage` names an artifact that must already be in S3, and
    /// `microvms-core` cannot put it there: `control/mod.rs:230` says so in as many words —
    /// "This client does not upload — S3 is not in this crate's dependency set". An
    /// `aws-sdk-s3` in *this* crate's manifest would be a second path to AWS inside the CLI,
    /// which is the thing CLI-2 forbids and the thinness guard would fail on.
    ///
    /// So the production implementation shells out to the `aws` CLI. That is a deliberate
    /// choice with a property worth stating: the `aws` binary cannot reach the control plane
    /// on this path, because the only argv this crate ever builds is `s3 cp` and the static
    /// guard forbids the operation strings an `aws lambda …` reach-around would have to
    /// name. `doctor` reports whether the binary is present, so the failure is diagnosed
    /// before a build rather than during one.
    ///
    /// A caller who has already uploaded passes `--artifact-uri` and never reaches this.
    fn put_artifact(&self, uri: &str, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), Error>>;
}

/// The production seam.
pub struct AwsSeam;

impl CoreSeam for AwsSeam {
    fn control_plane(&self, region: Region) -> BoxFuture<'_, Result<ControlPlane, Error>> {
        Box::pin(async move { ControlPlane::new(region).await })
    }

    fn open_sandbox(
        &self,
        region: Region,
        port: Option<u16>,
    ) -> BoxFuture<'_, Result<Sandbox, Error>> {
        Box::pin(async move {
            let mut plane = ControlPlane::new(region).await?;
            if let Some(port) = port {
                plane = plane.with_port(port);
            }
            Ok(Sandbox::with_control_plane(plane))
        })
    }

    fn attach_session(
        &self,
        region: Region,
        attach: Attach,
    ) -> BoxFuture<'_, Result<Session, Error>> {
        Box::pin(async move {
            // The minter goes through the same `ControlPlane` the launch path uses, so an
            // attached session mints proxy tokens exactly as a launched one does (TRAP-9).
            let mut plane = ControlPlane::new(region).await?;
            if let Some(port) = attach.port {
                plane = plane.with_port(port);
            }
            let port = plane.port();
            let minter = Arc::new(PlaneMinter {
                control: Arc::new(plane),
                microvm_id: attach.microvm_id,
            });
            microvms_core::session::Session::builder(attach.endpoint, attach.agent_token)
                .with_minter(minter)
                .with_port(port)
                .build()
        })
    }

    fn put_artifact(&self, uri: &str, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), Error>> {
        let uri = uri.to_string();
        Box::pin(async move { put_via_aws_cli(&uri, bytes).await })
    }
}

/// Mints proxy tokens for one MicroVM through the control plane.
///
/// The same bridge `sandbox.rs:333` builds for a launched VM. Duplicated rather than
/// exported from core, because core's is private to `sandbox.rs` and asking another lane to
/// widen it for the attach path is a change to a file this task must not touch — recorded
/// as the smaller of the two costs.
struct PlaneMinter {
    control: Arc<ControlPlane>,
    microvm_id: String,
}

impl microvms_core::session::TokenMinter for PlaneMinter {
    fn mint(&self) -> BoxFuture<'_, Result<microvms_core::session::ProxyToken, Error>> {
        Box::pin(async move {
            let minted = self.control.mint_auth_token(&self.microvm_id).await?;
            Ok(minted.into())
        })
    }
}

/// `aws s3 cp - <uri>`, with the artifact on the child's stdin.
///
/// Stdin rather than a temporary file: the artifact carries the daemon binary, and a
/// world-readable temp file holding the thing that will run inside the VM is a worse default
/// than a pipe. Every failure names the URI and the remedy, because "upload failed" without
/// the bucket is unactionable.
async fn put_via_aws_cli(uri: &str, bytes: Vec<u8>) -> Result<(), Error> {
    use tokio::io::AsyncWriteExt as _;
    use tokio::process::Command;

    let mut child = Command::new("aws")
        .args(["s3", "cp", "-", uri])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            Error::new(
                ErrorKind::Precondition,
                format!(
                    "could not run `aws s3 cp - {uri}`: {error}. microvms-core builds the \
                     artifact bytes but cannot upload them — S3 is deliberately absent from its \
                     dependency set, and adding an S3 client to this CLI would give it a second \
                     path to AWS. Either install the AWS CLI, or upload the artifact yourself \
                     and pass --artifact-uri."
                ),
            )
            .with_source(error)
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&bytes).await.map_err(|error| {
            Error::new(
                ErrorKind::Platform,
                format!("could not write the artifact to `aws s3 cp - {uri}`: {error}"),
            )
            .with_source(error)
        })?;
        stdin.shutdown().await.ok();
    }

    let output = child.wait_with_output().await.map_err(|error| {
        Error::new(
            ErrorKind::Platform,
            format!("`aws s3 cp - {uri}` did not complete: {error}"),
        )
        .with_source(error)
    })?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(Error::new(
        ErrorKind::Platform,
        format!(
            "`aws s3 cp - {uri}` exited {}: {detail}. The artifact has to be in S3 before \
             CreateMicrovmImage, and the service's own rejection would arrive *after* the \
             upload — which is why this is checked here.",
            output.status.code().unwrap_or(-1),
        ),
    ))
}

/// The region every ARN in a request is derived for.
///
/// Flags win over the environment, and the environment order is `AWS_REGION` then
/// `AWS_DEFAULT_REGION` — boto3's and aws-config's own. See the module docs on why agreeing
/// with the SDK matters more than picking the better order.
pub fn resolve_region(
    flag: Option<Region>,
    unlisted: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Region, Error> {
    if let Some(region) = flag {
        return Ok(region);
    }
    if let Some(name) = unlisted {
        // The escape hatch, and it normalises a supported name back to its own variant, so
        // `--unlisted-region us-east-1` is not a second spelling of a listed region.
        return Ok(Region::unlisted(name));
    }
    match env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION")) {
        // Parsed rather than accepted: an environment variable is a string, and this is the
        // boundary where the enum cannot help. A region the client has not seen carry
        // MicroVMs is refused with the null-message finding attached, and the remedy named
        // in the message is the flag that opts in.
        Some(name) => name.parse::<Region>().map_err(|error| {
            Error::invalid_arg(format!(
                "{error} It arrived from $AWS_REGION or $AWS_DEFAULT_REGION rather than from a \
                 flag; pass --unlisted-region {name:?} to opt in explicitly."
            ))
        }),
        None => Ok(Region::UsEast1),
    }
}

/// The three account-specific values, resolved before any call.
///
/// A missing bucket discovered halfway through a build has already spent the caller's
/// attention. `doctor` reports the same three.
#[derive(Clone, Debug, Default)]
pub struct Infra {
    pub bucket: Option<String>,
    pub build_role_arn: Option<String>,
    pub execution_role_arn: Option<String>,
}

impl Infra {
    /// Flags win over the environment.
    pub fn resolve(
        bucket: Option<String>,
        build_role_arn: Option<String>,
        execution_role_arn: Option<String>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        Self {
            bucket: bucket.or_else(|| env("MICROVM_BUCKET")),
            build_role_arn: build_role_arn.or_else(|| env("MICROVM_BUILD_ROLE_ARN")),
            execution_role_arn: execution_role_arn.or_else(|| env("MICROVM_EXECUTION_ROLE_ARN")),
        }
    }

    /// Rejects a command that cannot possibly succeed, naming **every** gap at once.
    ///
    /// Every gap rather than the first, because these arrive together from one Terraform
    /// apply and reporting them one per attempt costs the caller three round trips to learn
    /// one fact.
    pub fn require(&self, names: &[&str]) -> Result<(), Error> {
        let missing: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| self.get(name).is_none())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let flags: Vec<String> = missing
            .iter()
            .map(|name| format!("--{} (or ${})", name.replace('_', "-"), env_var_for(name)))
            .collect();
        Err(Error::new(
            ErrorKind::Precondition,
            format!(
                "missing required infrastructure: {}. \
                 `terraform -chdir=conformance/infra output` prints all three, and `microvm \
                 doctor` checks them alongside credentials and the daemon binary.",
                flags.join(", ")
            ),
        ))
    }

    fn get(&self, name: &str) -> Option<&str> {
        let value = match name {
            "bucket" => self.bucket.as_deref(),
            "build_role_arn" => self.build_role_arn.as_deref(),
            "execution_role_arn" => self.execution_role_arn.as_deref(),
            _ => None,
        };
        value.filter(|text| !text.is_empty())
    }
}

/// The environment variable each infrastructure value falls back to.
fn env_var_for(name: &str) -> &'static str {
    match name {
        "bucket" => "MICROVM_BUCKET",
        "build_role_arn" => "MICROVM_BUILD_ROLE_ARN",
        "execution_role_arn" => "MICROVM_EXECUTION_ROLE_ARN",
        _ => "",
    }
}

/// Reads the process environment. The `&dyn Fn` the resolvers take, in production.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Where the run ledger lives: `$MICROVM_STATE_DIR`, else `~/.microvm/runs`.
pub fn state_dir(flag: Option<PathBuf>, env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(path) = flag {
        return path;
    }
    if let Some(dir) = env("MICROVM_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = env("HOME").unwrap_or_else(|| ".".to_string());
    PathBuf::from(home).join(".microvm").join("runs")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name: &str| {
            owned
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    /// `AWS_REGION` before `AWS_DEFAULT_REGION`, which is boto3's own order.
    ///
    /// Asserted with *both* set, because that is the only case that distinguishes the two
    /// orders — and the consequence of getting it backwards is a connector ARN for one
    /// region and a client for another, reported as a malformed connector.
    #[test]
    fn aws_region_wins_over_aws_default_region() {
        let env = env_of(&[
            ("AWS_REGION", "us-west-2"),
            ("AWS_DEFAULT_REGION", "eu-west-1"),
        ]);
        assert_eq!(
            resolve_region(None, None, &env).expect("resolves"),
            Region::UsWest2
        );
    }

    /// A flag wins over both, and the documented default is us-east-1.
    #[test]
    fn a_flag_wins_and_the_default_is_us_east_one() {
        let env = env_of(&[("AWS_REGION", "us-west-2")]);
        assert_eq!(
            resolve_region(Some(Region::EuWest1), None, &env).expect("resolves"),
            Region::EuWest1
        );
        let empty = env_of(&[]);
        assert_eq!(
            resolve_region(None, None, &empty).expect("resolves"),
            Region::UsEast1
        );
    }

    /// An unsupported region in the environment is refused, with the finding and the flag
    /// that opts in.
    ///
    /// The environment is the one place a region arrives as a free string, so it is the one
    /// place the enum cannot help — and letting it through silently is exactly the
    /// null-message trap, which the message has to name because "region is invalid" sends
    /// someone to check their spelling.
    #[test]
    fn an_unsupported_region_from_the_environment_is_refused_naming_the_escape_hatch() {
        let env = env_of(&[("AWS_REGION", "eu-central-1")]);
        let error = resolve_region(None, None, &env).expect_err("eu-central-1 carries no MicroVMs");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        let message = error.to_string();
        assert!(message.contains("null message"), "{message}");
        assert!(message.contains("$AWS_REGION"), "{message}");
        assert!(message.contains("--unlisted-region"), "{message}");
    }

    /// The escape hatch normalises a supported name back to its variant.
    ///
    /// Otherwise `--unlisted-region us-east-1` would produce a second spelling of a listed
    /// region and every `is_supported` check downstream would answer wrongly.
    #[test]
    fn the_escape_hatch_normalises_a_supported_name() {
        let empty = env_of(&[]);
        assert_eq!(
            resolve_region(None, Some("us-east-1"), &empty).expect("resolves"),
            Region::UsEast1
        );
        let unlisted = resolve_region(None, Some("eu-central-1"), &empty).expect("opted in");
        assert!(!unlisted.is_supported());
        assert_eq!(unlisted.as_str(), "eu-central-1");
    }

    /// Every missing infrastructure value is named at once, with its flag and its variable.
    ///
    /// One per attempt would cost the caller three round trips to learn one fact — they come
    /// from one `terraform apply`.
    #[test]
    fn every_missing_infrastructure_value_is_named_in_one_message() {
        let infra = Infra::default();
        let error = infra
            .require(&["bucket", "build_role_arn", "execution_role_arn"])
            .expect_err("nothing is set");
        assert_eq!(error.kind(), ErrorKind::Precondition);
        let message = error.to_string();
        for expected in [
            "--bucket",
            "$MICROVM_BUCKET",
            "--build-role-arn",
            "$MICROVM_BUILD_ROLE_ARN",
            "--execution-role-arn",
            "$MICROVM_EXECUTION_ROLE_ARN",
        ] {
            assert!(message.contains(expected), "{expected} missing: {message}");
        }
    }

    /// An empty string counts as unset.
    ///
    /// `MICROVM_BUCKET=` in a sourced env file is the shape this catches, and it would
    /// otherwise pass `require` and fail as a malformed S3 URI three minutes into a build.
    #[test]
    fn an_empty_infrastructure_value_counts_as_missing() {
        let env = env_of(&[("MICROVM_BUCKET", "")]);
        let infra = Infra::resolve(None, None, None, &env);
        assert!(infra.require(&["bucket"]).is_err());
    }

    /// A flag wins over the environment for each of the three.
    #[test]
    fn infrastructure_flags_win_over_the_environment() {
        let env = env_of(&[
            ("MICROVM_BUCKET", "from-env"),
            ("MICROVM_BUILD_ROLE_ARN", "arn:env"),
        ]);
        let infra = Infra::resolve(Some("from-flag".into()), None, None, &env);
        assert_eq!(infra.bucket.as_deref(), Some("from-flag"));
        assert_eq!(infra.build_role_arn.as_deref(), Some("arn:env"));
        assert_eq!(infra.execution_role_arn, None);
        infra
            .require(&["bucket", "build_role_arn"])
            .expect("both present");
    }

    /// The state directory follows the flag, then `$MICROVM_STATE_DIR`, then `~/.microvm/runs`.
    #[test]
    fn the_state_directory_follows_the_flag_then_the_variable_then_the_home_default() {
        let env = env_of(&[("MICROVM_STATE_DIR", "/tmp/ledgers"), ("HOME", "/home/x")]);
        assert_eq!(
            state_dir(Some(PathBuf::from("/flag")), &env),
            PathBuf::from("/flag")
        );
        assert_eq!(state_dir(None, &env), PathBuf::from("/tmp/ledgers"));
        let home_only = env_of(&[("HOME", "/home/x")]);
        assert_eq!(
            state_dir(None, &home_only),
            PathBuf::from("/home/x/.microvm/runs")
        );
    }

    /// Every door has a stable name, which is what the behavioral guard asserts on.
    #[test]
    fn the_three_doors_have_stable_names() {
        use super::Door;
        assert_eq!(Door::ControlPlane.as_str(), "control_plane");
        assert_eq!(Door::OpenSandbox.as_str(), "open_sandbox");
        assert_eq!(Door::AttachSession.as_str(), "attach_session");
    }
}

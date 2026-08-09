// SPDX-License-Identifier: Apache-2.0
//! `doctor`: check every prerequisite and say which one is wrong.
//!
//! The command that saves an hour on a first attempt, and the only one that must run with
//! **nothing** configured at all — its whole job is reporting what is missing, so a version
//! that refused without credentials would refuse exactly when it is needed.
//!
//! # The ELF check is the one worth the file read
//!
//! MicroVMs are ARM64-only. A daemon built for the host produces an image whose CMD cannot
//! exec, and that surfaces as a **run-hook timeout** — 45 minutes into a build, saying nothing
//! about architecture. Twenty bytes of header answers it before the build starts.
//!
//! Read directly rather than shelled out to `file(1)`, which is not installed everywhere and
//! whose output is prose.

use microvms_core::Region;

use crate::cli::DoctorArgs;
use crate::commands::{Ctx, Rendered, response_type};
use crate::exit::{CliError, Exit};
use crate::render::{Check, healthy, render_doctor};
use crate::seam::resolve_region;

/// `EM_AARCH64`, from the ELF specification.
///
/// The single most common first-attempt failure is a host-architecture binary, and this
/// constant is what turns it from a 45-minute mystery into a line of `doctor` output.
pub const REQUIRED_ELF_MACHINE: u16 = 0xB7;

/// Runs every check and reports which one is wrong.
pub async fn doctor<O: std::io::Write, E: std::io::Write>(
    ctx: &mut Ctx<'_, O, E>,
    args: &DoctorArgs,
) -> Result<Rendered, CliError> {
    let mut checks: Vec<Check> = Vec::new();

    // The region first among the AWS-adjacent ones, because it is the value a connector ARN is
    // interpolated into and a wrong one produces a null-message denial that reads as IAM.
    checks.push(check_region(
        args.region.region.map(|r| r.region()),
        args.region.unlisted_region.as_deref(),
        ctx.env,
    ));
    // Then whether credentials resolve at all. Through the seam, so this command is covered by
    // the behavioral guard like every other AWS-touching one — and it is genuinely the
    // cheapest question that proves the chain resolves rather than that a file exists.
    checks.push(check_credentials(ctx).await);
    checks.extend(check_infra(ctx));
    checks.push(check_terraform(
        args.infra_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("conformance/infra")),
    ));
    // The binary last, only because it is the one failure that costs a full build cycle rather
    // than a call — so it is the one a reader should still see after the others pass.
    checks.push(check_binary(args.binary.as_deref()));

    let ok = healthy(&checks);
    let mut data = serde_json::Map::new();
    data.insert(
        "checks".into(),
        serde_json::json!(checks.iter().map(Check::to_json).collect::<Vec<_>>()),
    );
    data.insert("ok".into(), serde_json::json!(ok));

    let (kind, _) = response_type("doctor");
    let rendered = Rendered::ok(
        kind,
        data,
        render_doctor(&checks, false),
        render_doctor(&checks, true),
    );
    if !ok {
        // A **success** envelope with `ok: false`, because the check succeeded — it found what
        // was wrong, which is the command's whole job. The exit code is what a script branches
        // on, and `ERR_PRECONDITION` is what it means.
        return Ok(rendered.reporting(Exit::Precondition));
    }
    Ok(rendered)
}

/// Whether the region is one this client has seen carry MicroVMs.
///
/// **Advisory**, not fatal: AWS adds regions faster than a constant is re-read, and a hard
/// failure here would block a caller who is right while we are stale. The remedy names the
/// five, so the reader can tell "typo" from "genuinely new".
fn check_region(
    flag: Option<Region>,
    unlisted: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Check {
    match resolve_region(flag, unlisted, env) {
        Ok(region) if region.is_supported() => {
            Check::pass("region", format!("{region} is a known MicroVMs region"))
        }
        Ok(region) => Check::fail(
            "region",
            format!("{region} is not in this client's list of MicroVMs regions"),
            format!("known: {}", known_regions()),
        )
        .advisory(),
        // A region that will not even resolve is the environment holding a name the parser
        // refuses — reported here rather than raised, because `doctor` must run with a broken
        // environment. That is the whole point of the command.
        Err(error) => Check::fail(
            "region",
            error.to_string(),
            format!("known: {}", known_regions()),
        )
        .advisory(),
    }
}

fn known_regions() -> String {
    microvms_core::region::MICROVM_REGIONS
        .iter()
        .map(Region::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether the SDK can resolve an identity for the resolved region.
///
/// Constructing a [`microvms_core::control::ControlPlane`] is the cheapest question that proves
/// the credential chain resolves — `ControlPlane::new` fails with `ErrorKind::Credentials` and
/// names every source the chain looked at when it cannot. It spends no API call, unlike the
/// Python's `get_caller_identity`, which is a straight improvement: `doctor` should not be able
/// to fail on a throttle.
async fn check_credentials<O: std::io::Write, E: std::io::Write>(ctx: &mut Ctx<'_, O, E>) -> Check {
    let region = match resolve_region(None, None, ctx.env) {
        Ok(region) => region,
        // Already reported by the region check; there is nothing to resolve credentials for.
        Err(_) => Region::UsEast1,
    };
    match ctx.seam.control_plane(region.clone()).await {
        Ok(_) => Check::pass(
            "credentials",
            format!("the default chain resolved a provider for {region}"),
        ),
        Err(error) => Check::fail(
            "credentials",
            error.to_string(),
            "`aws sso login`, or set AWS_PROFILE / AWS_ACCESS_KEY_ID",
        ),
    }
}

/// The three Terraform outputs, each reported by name.
///
/// Separate checks rather than one, because "infrastructure missing" sends someone to re-read a
/// whole stack while "no execution role" sends them to one output.
fn check_infra<O: std::io::Write, E: std::io::Write>(ctx: &Ctx<'_, O, E>) -> Vec<Check> {
    [
        (
            "bucket",
            ctx.infra.bucket.as_deref(),
            "MICROVM_BUCKET",
            "s3_bucket",
        ),
        (
            "build-role",
            ctx.infra.build_role_arn.as_deref(),
            "MICROVM_BUILD_ROLE_ARN",
            "build_role_arn",
        ),
        (
            "execution-role",
            ctx.infra.execution_role_arn.as_deref(),
            "MICROVM_EXECUTION_ROLE_ARN",
            "execution_role_arn",
        ),
    ]
    .into_iter()
    .map(
        |(name, value, variable, output)| match value.filter(|v| !v.is_empty()) {
            Some(value) => Check::pass(name, value.to_string()),
            None => Check::fail(
                name,
                format!("unset (${variable})"),
                format!("terraform -chdir=conformance/infra output -raw {output}"),
            ),
        },
    )
    .collect()
}

/// Whether the conformance stack is applied, asked of Terraform rather than of a file.
///
/// A `terraform.tfstate` on disk is not a stack that exists: a destroyed stack leaves the file
/// behind with an empty resource list, which is precisely the state that produces "bucket does
/// not exist" three minutes into a build. `terraform output` answers the real question and
/// needs no credentials.
///
/// Advisory throughout, because the stack may legitimately live elsewhere and the three values
/// can be passed as flags.
fn check_terraform(infra_dir: std::path::PathBuf) -> Check {
    if !infra_dir.exists() {
        return Check::fail(
            "terraform-stack",
            format!("{} does not exist", infra_dir.display()),
            "the stack may live elsewhere; pass the three values as flags",
        )
        .advisory();
    }
    let output = std::process::Command::new("terraform")
        .arg(format!("-chdir={}", infra_dir.display()))
        .args(["output", "-json"])
        .output();
    let Ok(output) = output else {
        return Check::fail(
            "terraform-stack",
            "terraform is not on PATH, so the stack state is unknown",
            "mise install, or pass --bucket/--build-role-arn/--execution-role-arn directly",
        )
        .advisory();
    };
    if !output.status.success() {
        return Check::fail(
            "terraform-stack",
            format!(
                "terraform output exited {}: {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim(),
            ),
            "terraform -chdir=conformance/infra init",
        )
        .advisory();
    }
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null);
    let wanted = ["s3_bucket", "build_role_arn", "execution_role_arn"];
    let missing: Vec<&str> = wanted
        .into_iter()
        .filter(|key| parsed.get(*key).is_none())
        .collect();
    if missing.is_empty() {
        return Check::pass(
            "terraform-stack",
            format!(
                "applied: {}",
                parsed["s3_bucket"]["value"].as_str().unwrap_or("?")
            ),
        );
    }
    Check::fail(
        "terraform-stack",
        format!(
            "stack is not applied (missing outputs: {})",
            missing.join(", ")
        ),
        "mise run live:infra",
    )
    .advisory()
}

/// Whether the daemon binary is a real aarch64 ELF.
///
/// Fatal when a path was given and is wrong; advisory when none was, because most invocations
/// of `doctor` are checking credentials rather than a build.
fn check_binary(binary: Option<&std::path::Path>) -> Check {
    let Some(path) = binary else {
        return Check::fail(
            "daemon-binary",
            "no --binary given, so the architecture could not be checked",
            "pass --binary target/aarch64-unknown-linux-musl/release/agentd",
        )
        .advisory();
    };
    if !path.exists() {
        return Check::fail(
            "daemon-binary",
            format!("{} does not exist", path.display()),
            "cargo build --release -p agentd --target aarch64-unknown-linux-musl",
        );
    }
    match elf_machine(path) {
        None => Check::fail(
            "daemon-binary",
            format!("{} is not an ELF binary", path.display()),
            "the image CMD must be a static aarch64 ELF, not a script or a wrapper",
        ),
        Some(machine) if machine != REQUIRED_ELF_MACHINE => Check::fail(
            "daemon-binary",
            format!(
                "{} is ELF machine 0x{machine:x}, not aarch64 (0x{REQUIRED_ELF_MACHINE:x})",
                path.display()
            ),
            "MicroVMs are ARM64-only. Rebuild for aarch64-unknown-linux-musl — a host-arch \
             binary fails as a run-hook timeout, which says nothing about architecture.",
        ),
        Some(_) => Check::pass(
            "daemon-binary",
            format!("{} is aarch64 ELF", path.display()),
        ),
    }
}

/// The `e_machine` field of an ELF header, or `None` if this is not an ELF file.
///
/// Twenty bytes: the four-byte magic, then `EI_DATA` at offset 5 deciding the byte order, then
/// the two-byte `e_machine` at 18. Reading the endianness rather than assuming little is not
/// pedantry — a big-endian cross-compiled binary would otherwise report machine `0xB700` and be
/// rejected with a number nobody can look up.
pub fn elf_machine(path: &std::path::Path) -> Option<u16> {
    use std::io::Read as _;

    let mut header = [0u8; 20];
    let mut file = std::fs::File::open(path).ok()?;
    file.read_exact(&mut header).ok()?;
    if &header[..4] != b"\x7fELF" {
        return None;
    }
    let bytes = [header[18], header[19]];
    Some(if header[5] == 1 {
        u16::from_le_bytes(bytes)
    } else {
        u16::from_be_bytes(bytes)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a file and removes it on drop.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(label: &str, bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "microvm-doctor-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::write(&path, bytes).expect("writes");
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            // A leaked temp file in a test is noise, and a panic inside Drop
            // during unwind is an abort — swallowing is the only correct choice.
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A 20-byte ELF header for `machine`, little-endian.
    fn elf_header(machine: u16) -> Vec<u8> {
        let mut header = vec![0u8; 20];
        header[..4].copy_from_slice(b"\x7fELF");
        header[5] = 1; // little-endian
        header[18..20].copy_from_slice(&machine.to_le_bytes());
        header
    }

    /// **The check that saves the 45-minute build.** A host-architecture binary is named as
    /// such, with the reason the failure would otherwise be unrecognisable.
    ///
    /// `0x3E` is `EM_X86_64`, which is what a `cargo build` on a developer laptop produces.
    #[test]
    fn a_host_architecture_binary_is_named_along_with_why_it_would_be_a_mystery() {
        let binary = TempFile::new("x86", &elf_header(0x3E));
        let check = check_binary(Some(&binary.0));
        assert!(!check.ok);
        assert!(check.fatal, "a wrong architecture is not advisory");
        assert!(check.detail.contains("0x3e"), "{check:?}");
        assert!(check.detail.contains("aarch64 (0xb7)"), "{check:?}");
        assert!(
            check.remedy.contains("run-hook timeout"),
            "the reason it is otherwise invisible has to be named: {check:?}"
        );
    }

    /// An aarch64 binary passes.
    ///
    /// The positive case, so the check is a comparison rather than a blanket refusal of every
    /// binary — which would pass the test above while being useless.
    #[test]
    fn an_aarch64_binary_passes() {
        let binary = TempFile::new("arm", &elf_header(REQUIRED_ELF_MACHINE));
        let check = check_binary(Some(&binary.0));
        assert!(check.ok, "{check:?}");
        assert_eq!(elf_machine(&binary.0), Some(0xB7));
    }

    /// A big-endian header is read big-endian.
    ///
    /// Otherwise a cross-compiled binary reports `0xB700` and is rejected with a number that
    /// appears in no reference — which is a worse failure than the one this check exists for.
    #[test]
    fn the_byte_order_is_read_from_the_header_rather_than_assumed() {
        let mut header = vec![0u8; 20];
        header[..4].copy_from_slice(b"\x7fELF");
        header[5] = 2; // big-endian
        header[18..20].copy_from_slice(&REQUIRED_ELF_MACHINE.to_be_bytes());
        let binary = TempFile::new("be", &header);
        assert_eq!(elf_machine(&binary.0), Some(REQUIRED_ELF_MACHINE));
    }

    /// A shell script is not an ELF file, and says so.
    ///
    /// The plausible mistake this catches: baking a wrapper script as the image CMD, which the
    /// platform cannot exec at all.
    #[test]
    fn a_shell_script_is_reported_as_not_an_elf_binary() {
        let script = TempFile::new("script", b"#!/bin/sh\nexec agentd\n");
        let check = check_binary(Some(&script.0));
        assert!(!check.ok);
        assert!(check.detail.contains("not an ELF binary"), "{check:?}");
        assert_eq!(elf_machine(&script.0), None);
    }

    /// A file too short to hold a header is not an ELF file either, rather than a panic.
    #[test]
    fn a_truncated_file_does_not_panic() {
        let short = TempFile::new("short", b"\x7fELF");
        assert_eq!(elf_machine(&short.0), None);
        assert!(!check_binary(Some(&short.0)).ok);
    }

    /// No `--binary` is advisory, not fatal.
    ///
    /// Most `doctor` runs are checking credentials, and failing them over an unpassed flag would
    /// teach people to ignore the exit code.
    #[test]
    fn a_missing_binary_flag_is_advisory() {
        let check = check_binary(None);
        assert!(!check.ok);
        assert!(!check.fatal, "{check:?}");
        assert!(healthy(&[check]), "an advisory must not fail the run");
    }

    /// A nonexistent path *is* fatal: the caller named a file and it is not there.
    #[test]
    fn a_named_but_absent_binary_is_fatal() {
        let check = check_binary(Some(std::path::Path::new("/definitely/not/here/agentd")));
        assert!(!check.ok);
        assert!(check.fatal);
        assert!(!healthy(&[check]));
    }

    /// An unlisted region is advisory and the remedy names the five.
    ///
    /// Advisory because AWS adds regions faster than this constant is re-read; the list is in
    /// the remedy so a reader can tell a typo from something genuinely new.
    #[test]
    fn an_unlisted_region_is_advisory_and_the_remedy_names_the_five() {
        let env = |_: &str| None;
        let check = check_region(None, Some("eu-central-1"), &env);
        assert!(!check.ok);
        assert!(!check.fatal, "{check:?}");
        assert!(check.remedy.contains("us-east-1"), "{check:?}");
        assert!(check.remedy.contains("ap-northeast-1"), "{check:?}");

        // And a listed one passes.
        let listed = check_region(Some(Region::UsWest2), None, &env);
        assert!(listed.ok, "{listed:?}");
    }

    /// A region the environment holds that the parser refuses is reported, not raised.
    ///
    /// `doctor`'s whole job is running with a broken environment, so this is the case where
    /// raising would make the command useless exactly when it is needed.
    #[test]
    fn an_unparseable_region_from_the_environment_is_reported_rather_than_raised() {
        let env = |name: &str| (name == "AWS_REGION").then(|| "not-a-region".to_string());
        let check = check_region(None, None, &env);
        assert!(!check.ok);
        assert!(!check.fatal);
        assert!(check.detail.contains("not-a-region"), "{check:?}");
    }

    /// Each missing infrastructure value is its own check with its own Terraform output.
    ///
    /// "infrastructure missing" sends someone to re-read a whole stack; "execution-role unset"
    /// sends them to one output.
    #[test]
    fn each_missing_infrastructure_value_names_its_own_terraform_output() {
        let mut out = crate::envelope::Output::new(
            crate::envelope::Format::Plain,
            false,
            Vec::new(),
            Vec::new(),
        );
        let env = |_: &str| None;
        let seam = NoAws;
        let ctx = Ctx {
            seam: &seam,
            out: &mut out,
            infra: crate::seam::Infra::default(),
            env: &env,
        };
        let checks = check_infra(&ctx);
        assert_eq!(checks.len(), 3);
        let by_name: Vec<(&str, String)> = checks
            .iter()
            .map(|check| (check.name, check.remedy.clone()))
            .collect();
        assert_eq!(by_name[0].0, "bucket");
        assert!(by_name[0].1.contains("-raw s3_bucket"), "{by_name:?}");
        assert!(by_name[1].1.contains("-raw build_role_arn"), "{by_name:?}");
        assert!(
            by_name[2].1.contains("-raw execution_role_arn"),
            "{by_name:?}"
        );
        assert!(checks.iter().all(|check| check.fatal && !check.ok));
    }

    struct NoAws;

    impl crate::seam::CoreSeam for NoAws {
        fn control_plane(
            &self,
            _region: Region,
        ) -> crate::seam::futures_util_shim::BoxFuture<
            '_,
            Result<microvms_core::control::ControlPlane, microvms_core::Error>,
        > {
            panic!("this test reaches no seam")
        }

        fn open_sandbox(
            &self,
            _region: Region,
            _port: Option<u16>,
        ) -> crate::seam::futures_util_shim::BoxFuture<
            '_,
            Result<microvms_core::sandbox::Sandbox, microvms_core::Error>,
        > {
            panic!("this test reaches no seam")
        }

        fn attach_session(
            &self,
            _region: Region,
            _attach: crate::seam::Attach,
        ) -> crate::seam::futures_util_shim::BoxFuture<
            '_,
            Result<microvms_core::session::Session, microvms_core::Error>,
        > {
            panic!("this test reaches no seam")
        }

        fn put_artifact(
            &self,
            _uri: &str,
            _bytes: Vec<u8>,
        ) -> crate::seam::futures_util_shim::BoxFuture<'_, Result<(), microvms_core::Error>>
        {
            panic!("this test reaches no seam")
        }
    }
}

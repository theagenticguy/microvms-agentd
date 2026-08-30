// SPDX-License-Identifier: Apache-2.0
//! Self-provisioning of the `agentd` daemon binary (issues #75/#76 follow-through).
//!
//! # Why the CLI fetches its own daemon
//!
//! The daemon is this product's own component, versioned in this workspace and shipped from
//! this repository's releases — yet the original headline command made every first-time
//! caller download it by hand and pass its filesystem path as a positional
//! (`microvm run ./agentd`). No polished CLI hands its user a path to its own plumbing:
//! `docker run` takes an image name, and Dagger's CLI provisions its matching-version
//! engine itself. So `run`/`build` with no binary resolve one, in this order:
//!
//! 1. the typed positional or `binary` in `microvm.toml` (never reaches this module),
//! 2. `$MICROVM_AGENTD` — a path, for the caller who manages the binary themselves,
//! 3. the version-matched cache under the state directory,
//! 4. a fetch from this repository's GitHub release for the CLI's **own** version.
//!
//! Version-matched deliberately: the daemon and this binary share one workspace version, so
//! `v{CARGO_PKG_VERSION}` is the one tag whose protocol this CLI is proven against. A
//! "latest" fetch would reintroduce exactly the skew the shared version exists to prevent.
//!
//! # The fetch goes through a subprocess, and CLI-2 is why
//!
//! `tests/thinness.rs` forbids this crate every HTTP client by name — an HTTP client here
//! would be a second path to AWS, and the denylist cannot tell GitHub from a signed
//! endpoint. The sanctioned pattern is the one `seam.rs` already uses for the S3 upload
//! (`aws s3 cp -`): a subprocess the caller can see in `ps`. Two tools, in preference
//! order:
//!
//! - **`gh`**, because `gh attestation verify` proves the bytes came from this
//!   repository's release workflow — real provenance, not just integrity. A verification
//!   failure after a successful download is a **hard stop**, never a fallthrough: bytes
//!   that exist but do not verify are the one state a fallback must not launder.
//! - **`curl`**, because `gh` refuses to run unauthenticated even against a public repo.
//!   This path verifies the download against the release's `SHA256SUMS` asset, hashed
//!   in-process — integrity against corruption, weaker than provenance, and the progress
//!   line says which of the two the caller got. No `SHA256SUMS` on the release (every tag
//!   before v0.5.0) fails closed with the manual command as the remedy.
//!
//! # What is verified no matter where the bytes came from
//!
//! A fetched binary passes the same twenty-byte ELF check `doctor` runs before it is
//! installed: MicroVMs are ARM64-only, and a wrong asset baked into an image fails 45
//! minutes later as a run-hook timeout that says nothing about architecture. The install
//! itself is write-to-partial-then-rename, so an interrupted download can never leave a
//! truncated binary where the next invocation trusts the cache.
//!
//! # The trait exists for the same reason `CoreSeam` does
//!
//! [`Fetch`] is the seam: the shipped binary carries [`SubprocessFetch`], and the
//! behavioral guards script it, so no test can open a socket to GitHub — the same
//! arrangement that keeps them off AWS.

use std::path::{Path, PathBuf};

use crate::exit::{CliError, Exit};

/// The repository whose releases carry the daemon asset.
pub const RELEASE_REPO: &str = "theagenticguy/microvms-agentd";

/// The release asset's name — a literal, because the README's `--pattern agentd` and the
/// checksum lookup below both match it exactly.
pub const ASSET: &str = "agentd";

/// The environment variable that short-circuits every fetch: a path to a binary the caller
/// manages themselves.
pub const ENV_OVERRIDE: &str = "MICROVM_AGENTD";

/// How a fetched binary's bytes were proven, reported on the envelope so an agent reading
/// the run can tell provenance from integrity without re-deriving which tool was on PATH.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verification {
    /// `gh attestation verify`: the bytes are signed as built by this repository's release
    /// workflow.
    Attestation,
    /// The release's `SHA256SUMS` entry matched, hashed in-process: integrity against a
    /// corrupted or truncated download, not provenance.
    Checksum,
}

impl Verification {
    pub fn as_str(self) -> &'static str {
        match self {
            Verification::Attestation => "attestation",
            Verification::Checksum => "checksum",
        }
    }
}

/// Where the resolved binary came from, reported on the envelope beside the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `$MICROVM_AGENTD`.
    Env,
    /// Already installed under the state directory by an earlier fetch.
    Cache,
    /// Fetched from the GitHub release during this invocation.
    Fetched(Verification),
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::Cache => "cache",
            Source::Fetched(_) => "fetched",
        }
    }
}

/// A resolved daemon binary: the path the build will read, and the story of how it got there.
#[derive(Debug)]
pub struct Resolved {
    pub path: PathBuf,
    pub source: Source,
}

/// The fetch seam. The shipped binary carries [`SubprocessFetch`]; the guards script this,
/// which is what keeps `cargo test` off the network the way `CoreSeam` keeps it off AWS.
pub trait Fetch {
    /// Download release `tag`'s daemon asset to `dest` and prove the bytes, reporting
    /// progress lines as it goes. The error is prose: the caller owns the exit-code row and
    /// the remedies.
    fn fetch(
        &self,
        tag: &str,
        dest: &Path,
        progress: &mut dyn FnMut(&str),
    ) -> Result<Verification, String>;
}

/// A [`Fetch`] that panics, for tests whose invocation must never need one — the
/// [`crate::seam::PanickingSeam`] arrangement, applied to the second kind of egress.
/// `cfg(test)` for that struct's own reason: nothing in the shipped binary refuses a
/// fetch on purpose.
#[cfg(test)]
pub struct PanickingFetch;

#[cfg(test)]
impl Fetch for PanickingFetch {
    fn fetch(&self, tag: &str, _: &Path, _: &mut dyn FnMut(&str)) -> Result<Verification, String> {
        panic!("this invocation must not fetch (asked for {tag})");
    }
}

/// The cache path for `version`, under the CLI's state directory.
///
/// Versioned by directory rather than by filename, so the binary itself keeps the name the
/// Dockerfile stanza and every error message call it.
pub fn cache_path(state_dir: &Path, version: &str) -> PathBuf {
    state_dir
        .join("agentd")
        .join(format!("v{version}"))
        .join(ASSET)
}

/// Resolve a daemon binary for `version`, walking the chain the module docs name.
///
/// `state_dir` is the same directory the run ledger and name registry use — resolved by the
/// caller through [`crate::seam::state_dir`], so `--state-dir` and `$MICROVM_STATE_DIR`
/// move the cache with everything else.
pub fn resolve(
    state_dir: &Path,
    version: &str,
    env: &dyn Fn(&str) -> Option<String>,
    fetch: &dyn Fetch,
    progress: &mut dyn FnMut(&str),
) -> Result<Resolved, CliError> {
    // The override first: a caller who set it manages the binary themselves, and a cache
    // hit that silently outranked their variable would run a daemon they did not choose.
    if let Some(path) = env(ENV_OVERRIDE) {
        let path = PathBuf::from(path);
        if !path.exists() {
            return Err(CliError::new(
                Exit::Precondition,
                format!(
                    "${ENV_OVERRIDE} names {}, which does not exist. The variable \
                     short-circuits provisioning, so a stale path here blocks the fetch \
                     that would otherwise have worked.",
                    path.display()
                ),
            )
            .suggest(format!(
                "unset {ENV_OVERRIDE} to let the CLI provision the daemon"
            ))
            .suggest("or point it at a real aarch64 agentd binary"));
        }
        progress(&format!("using ${ENV_OVERRIDE}: {}", path.display()));
        return Ok(Resolved {
            path,
            source: Source::Env,
        });
    }

    let cached = cache_path(state_dir, version);
    if cached.exists() {
        progress(&format!(
            "using cached agentd v{version}: {}",
            cached.display()
        ));
        return Ok(Resolved {
            path: cached,
            source: Source::Cache,
        });
    }

    let tag = format!("v{version}");
    progress(&format!(
        "no agentd given — fetching the release asset for this CLI's own version ({tag})"
    ));
    let dir = cached.parent().expect("the cache path has a parent");
    std::fs::create_dir_all(dir).map_err(|error| io_error(dir, "create", &error))?;
    // A partial name in the same directory, so the final `rename` is atomic on the same
    // filesystem: an interrupted download leaves a `.partial` nothing trusts, never a
    // truncated `agentd` the next invocation reads as a cache hit.
    let partial = dir.join(format!(".{ASSET}.partial-{}", std::process::id()));
    let outcome = fetch.fetch(&tag, &partial, progress);
    let verification = match outcome {
        Ok(verification) => verification,
        Err(reason) => {
            let _ = std::fs::remove_file(&partial);
            return Err(fetch_error(&tag, &reason));
        }
    };

    // The same twenty-byte gate `doctor` runs, on bytes this module chose: a wrong asset
    // baked into an image is a 45-minute run-hook-timeout mystery, and twenty bytes now is
    // the whole cost of never finding out.
    match crate::commands::doctor::elf_machine(&partial) {
        Some(machine) if machine == crate::commands::doctor::REQUIRED_ELF_MACHINE => {}
        other => {
            let _ = std::fs::remove_file(&partial);
            return Err(CliError::new(
                Exit::Precondition,
                format!(
                    "the fetched {tag} asset is not an aarch64 ELF binary ({}), so it was \
                     discarded rather than cached — a wrong-architecture daemon fails as a \
                     run-hook timeout 45 minutes into a build.",
                    match other {
                        Some(machine) => format!("ELF machine 0x{machine:x}"),
                        None => "not an ELF header at all".to_string(),
                    }
                ),
            )
            .suggest("this is a release-asset defect worth reporting, not a local mistake"));
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| io_error(&partial, "chmod", &error))?;
    }
    std::fs::rename(&partial, &cached).map_err(|error| io_error(&cached, "install", &error))?;
    progress(&format!(
        "fetched and verified agentd {tag} ({}); cached at {}",
        verification.as_str(),
        cached.display()
    ));
    Ok(Resolved {
        path: cached,
        source: Source::Fetched(verification),
    })
}

/// A filesystem failure under the cache directory, as the `ERR_PRECONDITION` row: the
/// remedy is local (permissions, disk), and no AWS call has been spent.
fn io_error(path: &Path, verb: &str, error: &std::io::Error) -> CliError {
    CliError::new(
        Exit::Precondition,
        format!("could not {verb} {}: {error}", path.display()),
    )
    .suggest("the failure is on this machine's filesystem; the platform was not involved")
}

/// A failed fetch, with every way out named: the two tools, the manual download, and the
/// override variable. This is the error a fresh machine with no `gh` and no network sees,
/// so it carries the whole story rather than pointing at a doc.
fn fetch_error(tag: &str, reason: &str) -> CliError {
    CliError::new(
        Exit::Precondition,
        format!("could not provision the agentd daemon binary for {tag}: {reason}"),
    )
    .suggest(format!(
        "manual download: `gh release download {tag} --repo {RELEASE_REPO} --pattern {ASSET}` \
         (then `gh attestation verify {ASSET} --repo {RELEASE_REPO}`), and pass the path as \
         the positional or ${ENV_OVERRIDE}"
    ))
    .suggest("a self-built daemon works too: cargo build --release -p agentd --target aarch64-unknown-linux-musl")
}

/// The shipped fetcher: `gh` for provenance, `curl` + `SHA256SUMS` for integrity.
pub struct SubprocessFetch;

impl Fetch for SubprocessFetch {
    fn fetch(
        &self,
        tag: &str,
        dest: &Path,
        progress: &mut dyn FnMut(&str),
    ) -> Result<Verification, String> {
        // `gh` first. Any failure to *download* falls through to curl — `gh` refuses to run
        // unauthenticated even against a public repository, and that refusal must not cost
        // an unauthenticated machine the feature.
        match run_tool(&gh_download_args(tag, dest)) {
            Ok(()) => {
                progress(&format!(
                    "downloaded {ASSET} {tag} via gh; verifying provenance"
                ));
                // A verification failure after a successful download is the one hard stop:
                // falling through to curl here would launder bytes that failed provenance
                // into a weaker check that cannot see what was wrong with them.
                return match run_tool(&gh_verify_args(dest)) {
                    Ok(()) => Ok(Verification::Attestation),
                    Err(reason) => {
                        let _ = std::fs::remove_file(dest);
                        Err(format!(
                            "`gh attestation verify` refused the downloaded asset: {reason}. \
                             The bytes were discarded — do not retry with verification off."
                        ))
                    }
                };
            }
            Err(gh_reason) => {
                progress(&format!("gh could not download ({gh_reason}); trying curl"));
            }
        }

        run_tool(&curl_args(&asset_url(tag, ASSET), dest)).map_err(|curl_reason| {
            format!(
                "neither tool could download {ASSET} {tag} from {RELEASE_REPO} — gh and curl \
                 both failed, most recently: {curl_reason}"
            )
        })?;
        // Integrity, fail-closed: a release without SHA256SUMS (every tag before v0.5.0)
        // refuses rather than trusting TLS alone, and the remedy names the gh path that
        // still works against those tags.
        let sums_dest = dest.with_extension("sums");
        let sums = run_tool(&curl_args(&asset_url(tag, "SHA256SUMS"), &sums_dest))
            .and_then(|()| std::fs::read_to_string(&sums_dest).map_err(|error| error.to_string()));
        let _ = std::fs::remove_file(&sums_dest);
        let sums = sums.map_err(|reason| {
            format!(
                "downloaded {ASSET} {tag} via curl, but could not fetch the release's \
                 SHA256SUMS to verify it ({reason}). curl alone proves nothing about the \
                 bytes, so this fails closed. Releases before v0.5.0 ship no SHA256SUMS — \
                 for those, authenticate `gh` and retry, or download and verify manually."
            )
        })?;
        let bytes = std::fs::read(dest).map_err(|error| error.to_string())?;
        verify_sha256(&sums, ASSET, &bytes)?;
        progress(&format!(
            "downloaded {ASSET} {tag} via curl; SHA256SUMS entry matched"
        ));
        Ok(Verification::Checksum)
    }
}

/// The public download URL for `asset` on release `tag` — what curl gets, since it cannot
/// speak the release API without a token.
fn asset_url(tag: &str, asset: &str) -> String {
    format!("https://github.com/{RELEASE_REPO}/releases/download/{tag}/{asset}")
}

/// `gh release download` argv. A function so the exact invocation is testable without a
/// subprocess, and greppable against the README's manual spelling.
fn gh_download_args(tag: &str, dest: &Path) -> Vec<String> {
    vec![
        "gh".into(),
        "release".into(),
        "download".into(),
        tag.into(),
        "--repo".into(),
        RELEASE_REPO.into(),
        "--pattern".into(),
        ASSET.into(),
        "--output".into(),
        dest.display().to_string(),
        "--clobber".into(),
    ]
}

/// `gh attestation verify` argv — the provenance check the Install docs tell humans to run.
fn gh_verify_args(dest: &Path) -> Vec<String> {
    vec![
        "gh".into(),
        "attestation".into(),
        "verify".into(),
        dest.display().to_string(),
        "--repo".into(),
        RELEASE_REPO.into(),
    ]
}

/// curl argv: fail on HTTP errors, follow the release redirect to the CDN, HTTPS only, and
/// a ceiling so a stalled transfer is an error rather than a hang.
fn curl_args(url: &str, dest: &Path) -> Vec<String> {
    vec![
        "curl".into(),
        "-sSfL".into(),
        "--proto".into(),
        "=https".into(),
        "--max-time".into(),
        "300".into(),
        "--output".into(),
        dest.display().to_string(),
        url.into(),
    ]
}

/// Runs an argv, folding a spawn failure and a non-zero exit into one prose reason.
fn run_tool(argv: &[String]) -> Result<(), String> {
    let output = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|error| format!("`{}` did not run: {error}", argv[0]))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    Err(format!(
        "`{}` exited {}: {}",
        argv.join(" "),
        output.status.code().unwrap_or(-1),
        if detail.is_empty() {
            "(no stderr)"
        } else {
            detail
        },
    ))
}

/// Checks `bytes` against `asset`'s entry in a `SHA256SUMS` body (`<64 hex>  <name>` per
/// line, sha256sum's own format). In-process with the same `sha2` the artifact hash uses,
/// because `sha256sum` the tool does not exist on Windows.
fn verify_sha256(sums: &str, asset: &str, bytes: &[u8]) -> Result<(), String> {
    let expected = sums
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let digest = parts.next()?;
            let name = parts.next()?;
            // sha256sum marks a binary-mode entry with a leading `*`.
            (name.trim_start_matches('*') == asset).then(|| digest.to_ascii_lowercase())
        })
        .next()
        .ok_or_else(|| {
            format!(
                "the release's SHA256SUMS has no entry for {asset}, so nothing to verify against"
            )
        })?;
    use sha2::{Digest as _, Sha256};
    let actual = const_hex::encode(Sha256::digest(bytes));
    if actual == expected {
        return Ok(());
    }
    Err(format!(
        "SHA256 mismatch for {asset}: the release says {expected}, the download hashed to \
         {actual}. The bytes were not installed — retry, and if it repeats, treat the \
         mismatch as the finding rather than the obstacle."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A scripted fetch: writes `bytes` to the destination and counts invocations.
    struct Scripted {
        bytes: Vec<u8>,
        calls: Cell<usize>,
    }

    impl Scripted {
        fn elf() -> Self {
            Self {
                bytes: elf_header(crate::commands::doctor::REQUIRED_ELF_MACHINE),
                calls: Cell::new(0),
            }
        }
    }

    impl Fetch for Scripted {
        fn fetch(
            &self,
            _: &str,
            dest: &Path,
            _: &mut dyn FnMut(&str),
        ) -> Result<Verification, String> {
            self.calls.set(self.calls.get() + 1);
            std::fs::write(dest, &self.bytes).expect("writes");
            Ok(Verification::Attestation)
        }
    }

    /// A fetch that always fails, for the error-path assertions.
    struct Failing;

    impl Fetch for Failing {
        fn fetch(
            &self,
            _: &str,
            _: &Path,
            _: &mut dyn FnMut(&str),
        ) -> Result<Verification, String> {
            Err("no network in tests".into())
        }
    }

    /// A 20-byte little-endian ELF header for `machine` — `doctor`'s fixture, restated.
    fn elf_header(machine: u16) -> Vec<u8> {
        let mut header = vec![0u8; 20];
        header[..4].copy_from_slice(b"\x7fELF");
        header[5] = 1;
        header[18..20].copy_from_slice(&machine.to_le_bytes());
        header
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn sink() -> impl FnMut(&str) {
        |_: &str| {}
    }

    /// **A cache miss fetches once; the next resolve is a cache hit.** The property the
    /// whole module exists for: one download per version per machine.
    #[test]
    fn a_cache_miss_fetches_once_and_the_next_resolve_reads_the_cache() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let fetch = Scripted::elf();
        let mut progress = sink();

        let first = resolve(dir.path(), "9.9.9", &no_env, &fetch, &mut progress).expect("resolves");
        assert_eq!(first.source.as_str(), "fetched");
        assert_eq!(fetch.calls.get(), 1);
        assert!(first.path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&first.path)
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o111,
                0o111,
                "the installed binary must be executable"
            );
        }

        let second =
            resolve(dir.path(), "9.9.9", &no_env, &fetch, &mut progress).expect("resolves");
        assert_eq!(second.source.as_str(), "cache");
        assert_eq!(fetch.calls.get(), 1, "a cache hit must not fetch again");
        assert_eq!(second.path, first.path);
    }

    /// **`$MICROVM_AGENTD` outranks the cache.** A caller who set the variable manages the
    /// binary themselves, and a cache hit that outranked it would run a daemon they did not
    /// choose.
    #[test]
    fn the_environment_override_outranks_the_cache_and_never_fetches() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let own = dir.path().join("my-agentd");
        std::fs::write(&own, b"caller-managed").expect("writes");
        // A populated cache, which must lose.
        let cached = cache_path(dir.path(), "9.9.9");
        std::fs::create_dir_all(cached.parent().unwrap()).expect("mkdir");
        std::fs::write(&cached, b"cached").expect("writes");

        let own_str = own.display().to_string();
        let env = move |name: &str| (name == ENV_OVERRIDE).then(|| own_str.clone());
        let resolved =
            resolve(dir.path(), "9.9.9", &env, &PanickingFetch, &mut sink()).expect("resolves");
        assert_eq!(resolved.source.as_str(), "env");
        assert_eq!(resolved.path, own);
    }

    /// A `$MICROVM_AGENTD` pointing at nothing is an error naming the variable, not a
    /// silent fallthrough to a fetch the caller opted out of.
    #[test]
    fn a_stale_environment_override_is_an_error_rather_than_a_fallthrough() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let env = |name: &str| (name == ENV_OVERRIDE).then(|| "/definitely/not/here".to_string());
        let failure = resolve(dir.path(), "9.9.9", &env, &PanickingFetch, &mut sink())
            .expect_err("a stale override must refuse");
        assert_eq!(failure.exit, Exit::Precondition);
        assert!(
            failure.message.contains(ENV_OVERRIDE),
            "{}",
            failure.message
        );
    }

    /// **Fetched bytes that are not an aarch64 ELF are discarded, and the cache stays
    /// empty.** Caching them would turn one bad download into a persistent 45-minute
    /// run-hook-timeout mystery on every later invocation.
    #[test]
    fn a_fetched_non_arm_binary_is_refused_and_not_cached() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let fetch = Scripted {
            bytes: elf_header(0x3E),
            calls: Cell::new(0),
        }; // EM_X86_64
        let failure = resolve(dir.path(), "9.9.9", &no_env, &fetch, &mut sink())
            .expect_err("an x86 asset must refuse");
        assert!(failure.message.contains("0x3e"), "{}", failure.message);
        assert!(
            !cache_path(dir.path(), "9.9.9").exists(),
            "nothing may be cached"
        );

        let garbage = Scripted {
            bytes: b"#!/bin/sh".to_vec(),
            calls: Cell::new(0),
        };
        let failure = resolve(dir.path(), "9.9.9", &no_env, &garbage, &mut sink())
            .expect_err("a non-ELF asset must refuse");
        assert!(
            failure.message.contains("not an ELF"),
            "{}",
            failure.message
        );
    }

    /// A failed fetch is `ERR_PRECONDITION` carrying the tag and every way out: the manual
    /// `gh` spelling, the override variable, and the self-build.
    #[test]
    fn a_failed_fetch_names_the_tag_and_every_way_out() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let failure =
            resolve(dir.path(), "9.9.9", &no_env, &Failing, &mut sink()).expect_err("no network");
        assert_eq!(failure.exit, Exit::Precondition);
        assert!(failure.message.contains("v9.9.9"), "{}", failure.message);
        let remedies = failure.suggestions.join("\n");
        assert!(remedies.contains("gh release download"), "{remedies}");
        assert!(remedies.contains(ENV_OVERRIDE), "{remedies}");
        assert!(
            remedies.contains("cargo build --release -p agentd"),
            "{remedies}"
        );
    }

    /// The SHA256SUMS check: a matching entry passes, a mismatch refuses with both digests,
    /// and a missing entry refuses rather than passing vacuously.
    #[test]
    fn the_checksum_verification_matches_mismatches_and_refuses_a_missing_entry() {
        // sha256("agentd-bytes")
        use sha2::{Digest as _, Sha256};
        let digest = const_hex::encode(Sha256::digest(b"agentd-bytes"));

        let sums = format!("{digest}  agentd\nother  microvm-x86_64.tar.gz\n");
        assert!(verify_sha256(&sums, "agentd", b"agentd-bytes").is_ok());
        // The binary-mode `*` spelling is the same entry.
        let starred = format!("{digest} *agentd\n");
        assert!(verify_sha256(&starred, "agentd", b"agentd-bytes").is_ok());

        let mismatch = verify_sha256(&sums, "agentd", b"tampered").expect_err("must refuse");
        assert!(mismatch.contains(&digest), "{mismatch}");
        assert!(mismatch.contains("not installed"), "{mismatch}");

        let missing =
            verify_sha256("abc  something-else\n", "agentd", b"x").expect_err("must refuse");
        assert!(missing.contains("no entry"), "{missing}");
    }

    /// The argv builders spell the exact commands the docs teach humans, so the two cannot
    /// drift apart silently.
    #[test]
    fn the_subprocess_argv_matches_the_documented_manual_commands() {
        let dest = Path::new("/tmp/agentd");
        let gh = gh_download_args("v0.5.0", dest).join(" ");
        assert_eq!(
            gh,
            "gh release download v0.5.0 --repo theagenticguy/microvms-agentd \
             --pattern agentd --output /tmp/agentd --clobber"
        );
        let verify = gh_verify_args(dest).join(" ");
        assert_eq!(
            verify,
            "gh attestation verify /tmp/agentd --repo theagenticguy/microvms-agentd"
        );
        let curl = curl_args(&asset_url("v0.5.0", ASSET), dest).join(" ");
        assert!(
            curl.starts_with("curl -sSfL --proto =https --max-time 300"),
            "{curl}"
        );
        assert!(
            curl.ends_with(
                "https://github.com/theagenticguy/microvms-agentd/releases/download/v0.5.0/agentd"
            ),
            "{curl}"
        );
    }
}

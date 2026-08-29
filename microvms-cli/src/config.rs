// SPDX-License-Identifier: Apache-2.0
//! `microvm.toml`: the per-project config file, and the one merge that decides precedence.
//!
//! # Flags win over the file, and the file wins over the built-in defaults
//!
//! Every knob in the file already exists as a `run` flag; the file adds no capability, only
//! persistence — `microvm run` in a configured project needs zero flags. The precedence is
//! decided per knob by [`pick`], applied in exactly one place —
//! `commands::lifecycle::merge_config` — and reported in the envelope's `resolvedConfig` so
//! a caller never has to re-derive which source won: each knob carries its value *and* the
//! source it came from (`flag`, `config`, `env`, or `default`).
//!
//! # "The flag was given" is read off the parse, not the struct
//!
//! Five of `run`'s knobs carry clap defaults (`--memory`, `--max-idle-sec`,
//! `--suspended-sec`, `--max-duration-sec`, and `--timeout`), so their parsed fields cannot
//! say whether the caller typed them. [`crate::cli::RunArgs::explicit`] answers that from
//! `clap::ArgMatches::value_source` — `CommandLine` means typed — which is why `main.rs`
//! parses through `FromArgMatches` rather than `Parser::parse`. A merge keyed on "differs
//! from the default" would make `--memory 2048` unable to override a file that says `4096`.
//!
//! # Unknown keys are refused, and the refusal is the point
//!
//! `deny_unknown_fields`: a typo'd key silently ignored is a config the caller believes is
//! applied and is not — `memroy = 4096` launching a 2 GB VM is the failure this closes. The
//! same reasoning as CLI-5's closed sets, at the file boundary instead of the parser.
//!
//! # A malformed file is `ERR_CONFIG`, its own row
//!
//! Not `ERR_INVALID_ARG`: the remedies differ. An invalid argument is fixed by editing the
//! command line; a malformed config file is fixed by editing (or `--no-config` bypassing) a
//! file the invocation may never have named. The refusal is local, before any billable
//! call, and `microvm doctor` validates the same file with the same loader.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The file `run` and `doctor` look for beside the invocation when `--config` is not given.
pub const DEFAULT_FILE: &str = "microvm.toml";

/// The file, parsed. Every field optional: the file persists only what the project cares
/// to pin, and an absent key means "the flag or the built-in default decides".
///
/// Field names are the flag names with `-` for `_` (serde's `kebab-case`), so the file
/// reads like the command line it replaces. `env` is the one table: launch environment,
/// merged per key with `--launch-env` winning on a shared key.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProjectConfig {
    /// `run --image`: launch this existing image instead of building one.
    pub image: Option<String>,
    /// `run [BINARY]`: the daemon binary to bake in when building. A relative path here
    /// resolves against the *config file's* directory, not the process cwd — `--config
    /// /repo/microvm.toml` exists precisely for the invoke-from-elsewhere case, and a
    /// `target/agentd` that resolves against wherever the caller happens to stand is
    /// either a miss or, worse, a different binary that happens to share the name.
    pub binary: Option<PathBuf>,
    /// `run --exec`: the shell command to run in the VM.
    pub exec: Option<String>,
    /// `run --memory`: baseline MiB. Validated against the same closed set as the flag —
    /// the file must not be a side door past CLI-5.
    pub memory: Option<u32>,
    /// `run --region`: any region string; an unlisted one costs the null-message
    /// diagnostic exactly as `--unlisted-region` does, and [`load`] refuses nothing here —
    /// resolution happens where the flag's does.
    pub region: Option<String>,
    /// `run --egress`: give the VM outbound network.
    pub egress: Option<bool>,
    /// `run --max-idle-sec`: suspend after this much inbound-traffic idleness.
    pub max_idle_sec: Option<u32>,
    /// `run --suspended-sec`: terminate after this long suspended.
    pub suspended_sec: Option<u32>,
    /// `run --max-duration-sec`: hard ceiling on the VM's life.
    pub max_duration_sec: Option<u32>,
    /// `run --launch-env`, as a table. Merged per key: a `--launch-env` pair on the same
    /// key wins, because the flag is the specific thing happening now.
    pub env: Option<BTreeMap<String, String>>,
    /// Artifact globs for `run <DIR>` (issue #72): which files to bring back from the
    /// VM's synced working directory. Declared here (or nowhere — there is deliberately
    /// no flag spelling for a list this shape), validated by `doctor` and by `run` before
    /// any billable call.
    pub artifacts: Option<Vec<String>>,
}

/// Why a config file could not be used. One variant per remedy.
#[derive(Debug)]
pub enum ConfigError {
    /// `--config <PATH>` named a file that is not there. The implicit `./microvm.toml`
    /// never produces this: absence of the default file means "no config", not an error.
    Missing(PathBuf),
    /// The file exists and does not parse, or parses with a key this schema refuses.
    /// The message is toml's own, which names the line and column.
    Invalid(PathBuf, String),
    /// A value parsed but is outside the domain the matching flag enforces —
    /// `memory = 1500`, or an artifact glob that will not compile.
    Domain(PathBuf, String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Missing(path) => {
                write!(f, "--config named {} and it does not exist", path.display())
            }
            ConfigError::Invalid(path, reason) => {
                write!(f, "{} does not parse: {reason}", path.display())
            }
            ConfigError::Domain(path, reason) => write!(f, "{}: {reason}", path.display()),
        }
    }
}

/// The config for this invocation: the parsed file and where it was found, or `None`.
///
/// The three-way decision in one place, so `run` and `doctor` cannot disagree about which
/// file an invocation reads:
/// - `--no-config` → `Ok(None)`, whatever is on disk;
/// - `--config <PATH>` → that file, and its absence is [`ConfigError::Missing`] — a path
///   the caller typed and got wrong must not silently become "no config";
/// - neither → `<project_dir>/microvm.toml` if present, else `Ok(None)`.
///
/// `project_dir` is where the implicit default is looked for — the process's own
/// directory in production (`Path::new(".")`), injected rather than read from
/// `current_dir()` so a test never has to change the process-global cwd to exercise the
/// implicit path.
pub fn load(
    config_flag: Option<&Path>,
    no_config: bool,
    project_dir: &Path,
) -> Result<Option<(PathBuf, ProjectConfig)>, ConfigError> {
    if no_config {
        return Ok(None);
    }
    let (path, explicit) = match config_flag {
        Some(path) => (path.to_path_buf(), true),
        None => (project_dir.join(DEFAULT_FILE), false),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return if explicit {
                Err(ConfigError::Missing(path))
            } else {
                Ok(None)
            };
        }
        Err(error) => return Err(ConfigError::Invalid(path, error.to_string())),
    };
    let mut config: ProjectConfig = toml::from_str(&text)
        .map_err(|error| ConfigError::Invalid(path.clone(), error.to_string()))?;
    validate(&path, &config)?;
    // A relative `binary` means "relative to this file", so it resolves here — the one
    // place that knows where the file is — rather than against wherever the process
    // happens to stand. `--config /repo/microvm.toml` from another directory is the
    // flag's flagship case, and cwd-relative resolution would break exactly that one.
    // `is_relative` here means *genuinely* relative: `validate` above already refused
    // the two Windows shapes (`/x`, `C:x`) that answer `is_relative()` while meaning
    // something `join` would rewrite (issue #87).
    if let (Some(binary), Some(parent)) = (&config.binary, path.parent())
        && binary.is_relative()
    {
        config.binary = Some(parent.join(binary));
    }
    Ok(Some((path, config)))
}

/// The domain checks the matching flags would have applied.
///
/// The file must not be a side door: a value the parser would refuse on the command line is
/// refused here with the same vocabulary. Every violation is reported at once rather than
/// first-wins — the file arrives as a unit, and a caller fixing it one refusal per attempt
/// is the failure `Infra::require` already refuses to inflict.
fn validate(path: &Path, config: &ProjectConfig) -> Result<(), ConfigError> {
    let mut problems: Vec<String> = Vec::new();
    if let Some(memory) = config.memory
        && crate::cli::memory_from_mib(memory).is_none()
    {
        problems.push(format!(
            "memory = {memory} is not a documented size class baseline (512, 1024, 2048, \
             4096, 8192)"
        ));
    }
    if let Some(region) = config.region.as_deref()
        && crate::cli::RegionArg::from_name(region).is_none()
    {
        problems.push(format!(
            "region = {region:?} is not a region this client has seen carry MicroVMs; the \
             listed ones are us-east-1, us-east-2, us-west-2, eu-west-1, ap-northeast-1. An \
             unlisted region costs the null-message diagnostic, so it stays a spelled-out \
             opt-in: pass --unlisted-region {region} on the command line"
        ));
    }
    if let Some(duration) = config.max_duration_sec
        && !(1..=28800).contains(&duration)
    {
        problems.push(format!(
            "max-duration-sec = {duration} is outside 1..=28800 (eight hours is the \
             platform's hard ceiling on any single VM's life)"
        ));
    }
    if let Some(env) = &config.env {
        for key in env.keys() {
            if key.is_empty() {
                problems.push("[env] holds an empty key, which no shell can read back".into());
            } else if key.contains('=') {
                // A shape `--launch-env`'s parser can never produce: the flag splits on the
                // first `=`, so a key containing one is unreachable from the command line
                // and would smuggle a variable no shell can read back.
                problems.push(format!(
                    "[env] key {key:?} contains `=`, which no shell can read back"
                ));
            }
        }
    }
    if let Some(globs) = &config.artifacts {
        for glob in globs {
            if let Err(error) = globset::Glob::new(glob) {
                problems.push(format!("artifacts glob {glob:?} does not compile: {error}"));
            }
        }
    }
    // The two Windows path shapes that mean two things at once (issue #87). Windows
    // parses both as *relative*, so [`load`]'s file-directory join would fire on them
    // and silently rewrite what the caller wrote:
    //
    // - rooted with no drive (`/opt/agentd`): `join` keeps the joining directory's
    //   drive, so the value becomes `C:/opt/agentd` — a path the caller never wrote,
    //   reported by `resolvedConfig` as if the file had said it, possibly naming a
    //   different binary that happens to exist there.
    // - a drive with no root (`C:agentd`): `join` discards the directory entirely and
    //   the value resolves against that drive's *current directory* at spawn time.
    //
    // Refused rather than guessed, because the remedies differ per intent — write the
    // drive letter, or write a genuinely relative path — and only the caller knows
    // which they meant. Same posture as the unknown-key refusal: a legal-looking value
    // must not quietly mean something else. On Unix neither shape exists (a rooted
    // path is absolute, and no prefix component is ever parsed), so this never fires
    // there and `/opt/agentd` passes through as the absolute path it is.
    if let Some(binary) = &config.binary
        && binary.is_relative()
    {
        if binary.has_root() {
            problems.push(format!(
                "binary = {binary:?} is rooted but names no drive, which Windows reads \
                 as relative — resolving it against this file's directory would \
                 silently re-anchor it onto the file's drive. Write the drive letter \
                 if you meant an absolute path, or drop the leading separator if you \
                 meant a path relative to this file"
            ));
        } else if matches!(
            binary.components().next(),
            Some(std::path::Component::Prefix(_))
        ) {
            problems.push(format!(
                "binary = {binary:?} names a drive but no root, which resolves against \
                 that drive's current directory at spawn time. Write the root after \
                 the drive if you meant an absolute path, or drop the drive if you \
                 meant a path relative to this file"
            ));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(ConfigError::Domain(path.to_path_buf(), problems.join("; ")))
}

/// Where a resolved knob's value came from. Serialized into `resolvedConfig`.
///
/// `Env` exists for the one knob with an environment layer in its chain: the region
/// (flag, then config, then `$AWS_REGION`/`$AWS_DEFAULT_REGION`, then the built-in).
/// A report that said `default` while the launch went where the environment pointed
/// would be the report telling the one lie it exists to prevent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Flag,
    Config,
    Env,
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Flag => "flag",
            Source::Config => "config",
            Source::Env => "env",
            Source::Default => "default",
        }
    }
}

/// One knob after the merge: what won, and which source it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Knob<T> {
    pub value: T,
    pub source: Source,
}

/// Picks flag over config over default for one knob.
///
/// `explicit` is whether the *caller typed* the flag — for an `Option` flag that is
/// `is_some()`, and for a clap-defaulted flag it is the `value_source` answer
/// [`crate::cli::RunArgs::explicit`] carries. The flag's parsed value is passed even when
/// not explicit, because for a defaulted flag it *is* the built-in default.
pub fn pick<T: Clone>(explicit: bool, flag: T, config: Option<T>) -> Knob<T> {
    if explicit {
        return Knob {
            value: flag,
            source: Source::Flag,
        };
    }
    match config {
        Some(value) => Knob {
            value,
            source: Source::Config,
        },
        None => Knob {
            value: flag,
            source: Source::Default,
        },
    }
}

/// The launch environment after the per-key merge: config keys first, flag pairs
/// overwriting on a shared key.
///
/// Per key rather than all-or-nothing, deliberately: a project file pinning `RUST_LOG`
/// must not be discarded because the caller passed `--launch-env CI=1`. The flag pair wins
/// a shared key for the same reason `exec --env` beats the launch env — the flag is the
/// specific thing happening now.
pub fn merge_env(
    flag_pairs: &[(String, String)],
    config_env: Option<&BTreeMap<String, String>>,
) -> Vec<(String, String)> {
    let mut merged: Vec<(String, String)> = config_env
        .map(|env| {
            env.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    for (key, value) in flag_pairs {
        merged.retain(|(existing, _)| existing != key);
        merged.push((key.clone(), value.clone()));
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a file and removes it on drop. The second field is the `tempfile`
    /// guard that owns the deletion; `.0` stays a plain path for the tests.
    struct TempFile(PathBuf, #[allow(dead_code)] tempfile::TempPath);

    impl TempFile {
        fn new(label: &str, text: &str) -> Self {
            let file = tempfile::Builder::new()
                .prefix(&format!("microvm-config-{label}-"))
                .suffix(".toml")
                .tempfile()
                .expect("a temp file");
            std::fs::write(file.path(), text).expect("writes");
            let path = file.into_temp_path();
            Self(path.to_path_buf(), path)
        }
    }

    /// A full file round-trips into the struct, kebab-case keys and the env table included.
    #[test]
    fn a_full_file_parses_with_kebab_case_keys() {
        let file = TempFile::new(
            "full",
            r#"
image = "ci-image"
exec = "pytest -q"
memory = 4096
region = "us-west-2"
egress = true
max-idle-sec = 120
suspended-sec = 300
max-duration-sec = 7200
artifacts = ["dist/**", "*.log"]

[env]
RUST_LOG = "debug"
CI = "1"
"#,
        );
        let (path, config) = load(Some(&file.0), false, Path::new("."))
            .expect("loads")
            .expect("present");
        assert_eq!(path, file.0);
        assert_eq!(config.image.as_deref(), Some("ci-image"));
        assert_eq!(config.exec.as_deref(), Some("pytest -q"));
        assert_eq!(config.memory, Some(4096));
        assert_eq!(config.region.as_deref(), Some("us-west-2"));
        assert_eq!(config.egress, Some(true));
        assert_eq!(config.max_idle_sec, Some(120));
        assert_eq!(config.suspended_sec, Some(300));
        assert_eq!(config.max_duration_sec, Some(7200));
        assert_eq!(
            config.artifacts.as_deref(),
            Some(&["dist/**".to_string(), "*.log".to_string()][..])
        );
        let env = config.env.expect("an env table");
        assert_eq!(env.get("RUST_LOG").map(String::as_str), Some("debug"));
        assert_eq!(env.get("CI").map(String::as_str), Some("1"));
    }

    /// An unknown key is a refusal naming it, not a setting silently ignored.
    ///
    /// The failure this closes: `memroy = 4096` launching a 2 GB VM while the caller
    /// believes they configured 4 GB.
    #[test]
    fn an_unknown_key_is_refused_by_name() {
        let file = TempFile::new("typo", "memroy = 4096\n");
        let error = load(Some(&file.0), false, Path::new(".")).expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("memroy"), "{message}");
        assert!(
            matches!(error, ConfigError::Invalid(_, _)),
            "a schema violation is Invalid, not Domain: {error:?}"
        );
    }

    /// An off-table memory value is refused with the flag's own vocabulary.
    ///
    /// The file must not be a side door past CLI-5: `--memory 1500` cannot parse, so
    /// `memory = 1500` cannot load.
    #[test]
    fn an_off_table_memory_is_refused_like_the_flag_would() {
        let file = TempFile::new("memory", "memory = 1500\n");
        let error = load(Some(&file.0), false, Path::new(".")).expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("1500"), "{message}");
        assert!(message.contains("512, 1024, 2048, 4096, 8192"), "{message}");
    }

    /// An unlisted region in the file is refused at load with the flag's own remedy.
    ///
    /// At *load*, deliberately: doctor validates through this same loader, so a domain
    /// check that lived past it would be one doctor passes and `run` refuses — the exact
    /// disagreement `check_config` exists to prevent.
    #[test]
    fn an_unlisted_region_is_refused_at_load_with_the_opt_in_named() {
        let file = TempFile::new("region", "region = \"mars-central-1\"\n");
        let message = load(Some(&file.0), false, Path::new("."))
            .expect_err("refused")
            .to_string();
        assert!(message.contains("mars-central-1"), "{message}");
        assert!(message.contains("--unlisted-region"), "{message}");
    }

    /// A duration past the platform's eight-hour ceiling is refused at load, not after a
    /// credential resolution and an opened sandbox.
    #[test]
    fn an_over_ceiling_duration_is_refused_at_load() {
        let file = TempFile::new("duration", "max-duration-sec = 999999\n");
        let message = load(Some(&file.0), false, Path::new("."))
            .expect_err("refused")
            .to_string();
        assert!(message.contains("999999"), "{message}");
        assert!(message.contains("28800"), "{message}");
    }

    /// An env key containing `=` is refused: `--launch-env`'s parser can never produce
    /// one, and the file must not smuggle a variable no shell can read back.
    #[test]
    fn an_env_key_containing_equals_is_refused_at_load() {
        let file = TempFile::new("env-eq", "[env]\n\"A=B\" = \"x\"\n");
        let message = load(Some(&file.0), false, Path::new("."))
            .expect_err("refused")
            .to_string();
        assert!(message.contains("A=B"), "{message}");
    }

    /// A relative `binary` resolves against the file's directory, not the process cwd.
    ///
    /// `--config /repo/microvm.toml` from another directory is the flag's flagship case;
    /// cwd-relative resolution would miss the binary — or find a different one that
    /// happens to share the name under the caller's cwd.
    #[test]
    fn a_relative_binary_resolves_against_the_files_directory() {
        let file = TempFile::new("relbin", "binary = \"target/agentd\"\n");
        let (_, config) = load(Some(&file.0), false, Path::new("."))
            .expect("loads")
            .expect("present");
        let expected = file.0.parent().expect("a parent").join("target/agentd");
        assert_eq!(config.binary.as_deref(), Some(expected.as_path()));

        // A platform-absolute path, because "/opt/agentd" is not absolute on Windows —
        // no drive letter makes it rooted-but-relative there, so `join` keeps the temp
        // drive and CI read `C:/opt/agentd`. TOML's literal (single-quoted) string
        // spares the backslashes any escaping.
        let absolute_path = std::env::temp_dir().join("agentd-absolute");
        let absolute = TempFile::new(
            "absbin",
            &format!("binary = '{}'\n", absolute_path.display()),
        );
        let (_, config) = load(Some(&absolute.0), false, Path::new("."))
            .expect("loads")
            .expect("present");
        assert_eq!(
            config.binary.as_deref(),
            Some(absolute_path.as_path()),
            "an absolute path passes through untouched"
        );
    }

    /// The two ambiguous Windows shapes are refused at load, before the join could
    /// rewrite them (issue #87).
    ///
    /// `cfg(windows)` because the shapes only parse as ambiguous there — on Unix
    /// `/opt/agentd` is simply absolute and `C:agentd` is an ordinary relative
    /// component. CI's windows-latest leg runs this crate's tests, so this executes on
    /// every PR rather than existing as a comment about a platform nobody checks.
    ///
    /// **Falsification** — delete the ambiguity checks in `validate` and the first
    /// assertion reads back a join result instead of a refusal. Done on 2026-08-29 on
    /// the windows-latest CI leg (this test cannot execute on a dev Linux box): PR #89
    /// carried the deletion, and its windows job failed on exactly this test while
    /// ubuntu and macos stayed green. Failed as stated; the deletion was never merged.
    #[cfg(windows)]
    #[test]
    fn an_ambiguous_windows_binary_shape_is_refused_at_load() {
        // Rooted, no drive: the shape CI caught re-anchoring onto the temp drive.
        let rooted = TempFile::new("rooted", "binary = '/opt/agentd'\n");
        let message = load(Some(&rooted.0), false, Path::new("."))
            .expect_err("refused")
            .to_string();
        assert!(message.contains("/opt/agentd"), "{message}");
        assert!(message.contains("drive"), "{message}");

        // Drive, no root: joins by *replacing* the file's directory entirely, then
        // resolves against that drive's current directory at spawn time.
        let drive_relative = TempFile::new("driverel", "binary = 'C:agentd'\n");
        let message = load(Some(&drive_relative.0), false, Path::new("."))
            .expect_err("refused")
            .to_string();
        assert!(message.contains("C:agentd"), "{message}");
        assert!(message.contains("current directory"), "{message}");

        // The unambiguous neighbours still load: fully absolute passes through
        // untouched, and genuinely relative resolves against the file's directory.
        let absolute = TempFile::new("winabs", "binary = 'C:\\opt\\agentd'\n");
        let (_, config) = load(Some(&absolute.0), false, Path::new("."))
            .expect("loads")
            .expect("present");
        assert_eq!(
            config.binary.as_deref(),
            Some(Path::new("C:\\opt\\agentd")),
            "a drive-plus-root path is not ambiguous and passes through"
        );
        let relative = TempFile::new("winrel", "binary = 'target\\agentd'\n");
        let (_, config) = load(Some(&relative.0), false, Path::new("."))
            .expect("loads")
            .expect("present");
        assert_eq!(
            config.binary.as_deref(),
            Some(
                relative
                    .0
                    .parent()
                    .expect("a parent")
                    .join("target\\agentd")
                    .as_path()
            ),
            "a genuinely relative path still resolves against the file's directory"
        );
    }

    /// On Unix the issue-#87 refusal never fires: `/opt/agentd` is absolute there, and
    /// there is no such thing as a drive prefix to be missing a root.
    ///
    /// The guard this pins: the ambiguity check runs unconditionally (no `cfg` in
    /// `validate`), so a predicate mistake that made it fire on Unix would refuse
    /// every absolute `binary` on the platforms the daemon actually ships to.
    #[cfg(unix)]
    #[test]
    fn a_rooted_binary_on_unix_is_absolute_and_passes_through() {
        let file = TempFile::new("unixabs", "binary = '/opt/agentd'\n");
        let (_, config) = load(Some(&file.0), false, Path::new("."))
            .expect("loads: no ambiguity exists on unix")
            .expect("present");
        assert_eq!(
            config.binary.as_deref(),
            Some(Path::new("/opt/agentd")),
            "an absolute path passes through untouched"
        );
    }

    /// A glob that will not compile is refused at load, before any billable call.
    #[test]
    fn a_bad_artifact_glob_is_refused_at_load() {
        let file = TempFile::new("glob", r#"artifacts = ["dist/[oops"]"#);
        let error = load(Some(&file.0), false, Path::new(".")).expect_err("refused");
        assert!(error.to_string().contains("dist/[oops"), "{}", error);
    }

    /// Every domain violation is reported at once, not first-wins.
    ///
    /// The file arrives as a unit; a caller fixing it one refusal per attempt pays three
    /// round trips to learn one fact — `Infra::require` names every gap for the same
    /// reason.
    #[test]
    fn every_domain_violation_is_named_in_one_refusal() {
        let file = TempFile::new("multi", "memory = 1500\nartifacts = [\"dist/[oops\"]\n");
        let message = load(Some(&file.0), false, Path::new("."))
            .expect_err("refused")
            .to_string();
        assert!(message.contains("1500"), "{message}");
        assert!(message.contains("dist/[oops"), "{message}");
    }

    /// `--no-config` wins over everything, including a broken file on disk.
    #[test]
    fn no_config_skips_even_a_broken_file() {
        let file = TempFile::new("skipped", "this is not toml at all [");
        assert!(
            load(Some(&file.0), true, Path::new("."))
                .expect("skipped")
                .is_none()
        );
    }

    /// A `--config` path that is not there is an error; the implicit default's absence is
    /// not.
    ///
    /// The two absences mean different things: one is a path the caller typed and got
    /// wrong, the other is a project that has no config — and conflating them either
    /// fails every unconfigured project or silently ignores a typo'd `--config`.
    #[test]
    fn an_explicit_missing_path_fails_and_the_implicit_default_does_not() {
        let missing = std::path::Path::new("/definitely/not/here/microvm.toml");
        let error =
            load(Some(missing), false, Path::new(".")).expect_err("a typed path must exist");
        assert!(matches!(error, ConfigError::Missing(_)), "{error:?}");

        // The implicit default: a project directory with no microvm.toml.
        let elsewhere = std::env::temp_dir().join(format!(
            "microvm-config-absent-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&elsewhere).expect("a temp dir");
        let result = load(None, false, &elsewhere);
        let _ = std::fs::remove_dir_all(&elsewhere);
        assert!(result.expect("no config is not an error").is_none());
    }

    /// The merge picks flag over config over default, and reports which won.
    #[test]
    fn the_merge_is_flag_then_config_then_default() {
        // Flag typed: it wins even when the config disagrees.
        let flagged = pick(true, 4096u32, Some(8192));
        assert_eq!(flagged.value, 4096);
        assert_eq!(flagged.source, Source::Flag);

        // Flag untyped, config present: the file wins over the built-in default.
        let configured = pick(false, 2048u32, Some(8192));
        assert_eq!(configured.value, 8192);
        assert_eq!(configured.source, Source::Config);

        // Neither: the built-in default, and labelled as such.
        let defaulted = pick(false, 2048u32, None);
        assert_eq!(defaulted.value, 2048);
        assert_eq!(defaulted.source, Source::Default);
    }

    /// The env merge is per key: config keys survive, a flag pair wins its own key.
    ///
    /// All-or-nothing would discard a project's pinned `RUST_LOG` because the caller
    /// passed `--launch-env CI=1`, which is the plausible wrong version.
    #[test]
    fn the_env_merge_is_per_key_with_the_flag_winning_its_own() {
        let config_env: BTreeMap<String, String> = [
            ("RUST_LOG".to_string(), "debug".to_string()),
            ("CI".to_string(), "0".to_string()),
        ]
        .into();
        let merged = merge_env(&[("CI".to_string(), "1".to_string())], Some(&config_env));
        let as_map: BTreeMap<&str, &str> = merged
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(as_map.get("RUST_LOG"), Some(&"debug"), "{merged:?}");
        assert_eq!(
            as_map.get("CI"),
            Some(&"1"),
            "the flag wins its key: {merged:?}"
        );
        assert_eq!(merged.len(), 2, "no duplicate keys: {merged:?}");
    }

    /// An empty file is a valid config with nothing pinned.
    #[test]
    fn an_empty_file_is_a_valid_config_with_nothing_pinned() {
        let file = TempFile::new("empty", "");
        let (_, config) = load(Some(&file.0), false, Path::new("."))
            .expect("loads")
            .expect("present");
        assert_eq!(config, ProjectConfig::default());
    }
}

// SPDX-License-Identifier: Apache-2.0
//! The build artifact: a zip of the daemon binary and a Dockerfile.
//!
//! What `CreateMicrovmImage`'s `codeArtifact.uri` points at (`sandbox.py:523`
//! `build_artifact`). Two entries, and both of them carry a measured constraint.
//!
//! # The execute bit has to be in the zip entry
//!
//! A build that copies a non-executable binary produces an image whose `CMD` fails, and
//! the failure surfaces as a **run-hook timeout** — which says nothing about permissions,
//! and sends the reader to look at the daemon's startup path instead of at the archive.
//! So the `agentd` entry sets mode `0o755` explicitly rather than inheriting whatever the
//! host file had.
//!
//! # The agent token is never in here (TRAP-5's other half)
//!
//! The artifact becomes a **shared image snapshot**: every MicroVM launched from the image
//! sees the same bytes. A per-VM secret in there is a per-VM secret shared with every VM.
//! So the token travels through `runHookPayload` at launch instead, and this function has
//! no parameter that could carry one — see [`crate::control::microvm`]. The test at the
//! bottom of this file scans the produced zip's raw bytes for a token value to prove the
//! path stays closed, which is a byte scan rather than an API review because the leak
//! would be a *value* appearing somewhere, not a parameter being declared.

use std::io::Write as _;
use std::time::Duration;

use crate::error::{Error, ErrorKind};

/// The Dockerfile entry's name, which the build looks for by convention.
const DOCKERFILE_ENTRY: &str = "Dockerfile";

/// The daemon entry's name, matching the `CMD ["/agentd"]` the Dockerfile sets.
const AGENTD_ENTRY: &str = "agentd";

/// The mode the daemon entry carries.
///
/// See the module docs: a non-executable binary here becomes a run-hook timeout later.
const AGENTD_MODE: u32 = 0o755;

/// Zips `binary` with `dockerfile` into the bytes `codeArtifact.uri` will point at.
///
/// `binary` is the daemon's bytes rather than a path, so the caller owns the read and this
/// function has no filesystem behaviour to stub in a test.
pub fn build_artifact(binary: &[u8], dockerfile: &str) -> Result<Vec<u8>, Error> {
    use zip::write::{SimpleFileOptions, ZipWriter};

    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let deflated =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut write_entry = |name: &str, bytes: &[u8], options: SimpleFileOptions| {
        writer
            .start_file(name, options)
            .and_then(|()| writer.write_all(bytes).map_err(Into::into))
            .map_err(|error| {
                Error::new(
                    ErrorKind::Unexpected,
                    format!("could not add {name} to the build artifact: {error}"),
                )
            })
    };

    write_entry(DOCKERFILE_ENTRY, dockerfile.as_bytes(), deflated)?;
    write_entry(AGENTD_ENTRY, binary, deflated.unix_permissions(AGENTD_MODE))?;

    let bytes = writer
        .finish()
        .map_err(|error| {
            Error::new(
                ErrorKind::Unexpected,
                format!("could not finish the build artifact: {error}"),
            )
        })?
        .into_inner();
    Ok(bytes)
}

/// The sha256 of the artifact's two inputs, as lowercase hex.
///
/// # What is hashed, and why not the zip
///
/// The **inputs** — the daemon binary's bytes and the Dockerfile text — rather than the
/// bytes [`build_artifact`] produces. The zip is a container: its byte identity depends on
/// the `zip` crate's version, its compression level, and its header defaults, so an
/// upgraded dependency would silently change every image name and orphan every reuse. The
/// inputs are what a build actually consumes, and two builds with equal inputs produce
/// interchangeable images — which is the property content-addressed reuse rests on.
///
/// Each input is length-prefixed before hashing, so `(binary="ab", dockerfile="c")` and
/// `(binary="a", dockerfile="bc")` are different hashes rather than one concatenation.
pub fn artifact_content_hash(binary: &[u8], dockerfile: &str) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hasher = Sha256::new();
    hasher.update((binary.len() as u64).to_be_bytes());
    hasher.update(binary);
    hasher.update((dockerfile.len() as u64).to_be_bytes());
    hasher.update(dockerfile.as_bytes());
    const_hex::encode(hasher.finalize())
}

/// A base image: the platform ARN, the Dockerfile `FROM` that pairs with it, and whether
/// it declares a `WORKDIR`.
///
/// All three together because the first two **must** agree and used to be able to
/// disagree: the Python client's `DEFAULT_BASE_IMAGE` named the managed base for
/// `baseImageArn` while `default_dockerfile` hardcoded an unrelated registry literal in
/// its `FROM`, so changing either left the other pointing somewhere else
/// (`sandbox.py:410-444`). Pairing them means a caller selects one thing and both fields
/// follow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BaseImage {
    /// Goes into `baseImageArn` — the platform's managed base, not a registry ref.
    pub name: String,
    /// Goes into the Dockerfile `FROM` — the registry ref measured alongside `name`.
    pub docker_ref: String,
    /// What `docker inspect` reports for `WorkingDir`. Empty means it declares none.
    ///
    /// A field rather than a lookup because a caller with a purpose-built image is the
    /// only one who can say what their image declares, and this client cannot read it
    /// without pulling the manifest.
    pub working_dir: String,
}

impl BaseImage {
    /// The managed base every `docs/PLATFORM.md` measurement from 2026-08-06 onward used,
    /// paired with the registry ref those same builds used as `FROM`.
    ///
    /// `working_dir` is empty, and that is measured rather than assumed:
    /// `al2023-minimal`, `python:3.12-slim`, and `node:20-slim` all leave `WorkingDir`
    /// empty (2026-08-05), which is what makes [`require_workdir`] necessary.
    pub fn al2023() -> Self {
        Self {
            name: "al2023-1".to_string(),
            docker_ref: "public.ecr.aws/amazonlinux/amazonlinux:2023-minimal".to_string(),
            working_dir: String::new(),
        }
    }

    /// The `baseImageArn` for this base in `region`.
    ///
    /// # `microvm-image:<name>`, with a colon, and it is not just the managed base
    ///
    /// This was the only place in the repo building the colon form; the fakes and the
    /// encoding test all used `microvm-image/<name>`, so the repo held two beliefs about the
    /// shape of the identifier every image call takes. The model's `TaggableResource` pattern
    /// admits only `(capacity-provider|network-connector|microvm-image):[a-zA-Z0-9-_]+`, and
    /// a live read settled it: `ListMicrovmImages` in us-east-1 returns
    /// `arn:aws:lambda:us-east-1:<account>:microvm-image:coding-agents-on-bedrock` for a
    /// customer image, and `GetMicrovmImage` accepts exactly that (measured 2026-08-15).
    ///
    /// So the colon is right for **both** the managed base and a customer image; there is no
    /// two-form rule, and this comment used to imply there was. The slash form fails as
    /// `AccessDeniedException` rather than as a validation error, because IAM evaluates the
    /// malformed ARN as a resource with no matching policy — a permissions message for a
    /// resource that exists.
    pub fn arn(&self, region: &crate::region::Region) -> String {
        format!(
            "arn:aws:lambda:{}:aws:microvm-image:{}",
            region.as_str(),
            self.name
        )
    }
}

impl Default for BaseImage {
    fn default() -> Self {
        Self::al2023()
    }
}

/// A Dockerfile that makes the daemon the container `CMD`.
///
/// `ENTRYPOINT []` plus `CMD ["/agentd"]` is the deployment invariant the trust boundary
/// rests on: it is what guarantees no task workload runs before the platform's run hook
/// lands. It is also what makes an omitted `cwd` inherit the image `WORKDIR`, since the
/// daemon's own cwd is the image's.
///
/// The `FROM` is derived from `base` rather than written here, so it cannot disagree with
/// the `baseImageArn` the create call sends.
///
/// The invariant is *unenforced*: a base image that starts its own background process
/// before bootstrap breaks it, and enforcing that belongs to whoever builds the image
/// (`docs/PROTOCOL.md`, "Trust boundary").
pub fn default_dockerfile(port: u16, workdir: Option<&str>, base: &BaseImage) -> String {
    let mut lines = vec![
        format!("FROM {}", base.docker_ref),
        "COPY agentd /agentd".to_string(),
        "RUN chmod 0755 /agentd".to_string(),
    ];
    if let Some(workdir) = workdir.filter(|dir| !dir.is_empty()) {
        lines.push(format!("RUN mkdir -p {workdir}"));
        lines.push(format!("WORKDIR {workdir}"));
    }
    lines.extend([
        format!("ENV AGENTD_PORT={port}"),
        "ENV AGENTD_LOG=info".to_string(),
        format!("EXPOSE {port}"),
        "ENTRYPOINT []".to_string(),
        r#"CMD ["/agentd"]"#.to_string(),
        String::new(),
    ]);
    lines.join("\n")
}

/// The image ref in a Dockerfile's first `FROM`, or `None` when it has none.
///
/// Deliberately loose on whitespace and case, and it ignores `--platform=` and `AS name`
/// decoration, because the check exists to catch a base that disagrees rather than to
/// validate Dockerfile syntax.
///
/// Split-and-scan rather than a regex: the pattern is "first token after FROM on a line
/// whose first word is FROM", which `split_whitespace` states more directly than a
/// pattern string would. (`regex-lite` is available if this ever grows.)
pub fn dockerfile_from_ref(dockerfile: &str) -> Option<&str> {
    for line in dockerfile.lines() {
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else { continue };
        if !first.eq_ignore_ascii_case("FROM") {
            continue;
        }
        // Skip flag decoration such as `--platform=linux/arm64`.
        return words.find(|word| !word.starts_with("--"));
    }
    None
}

/// Rejects working-directory inheritance when nothing declares one.
///
/// Measured 2026-08-05: `al2023-minimal`, `python:3.12-slim`, and `node:20-slim` all leave
/// `WorkingDir` empty, so "inherit the image WORKDIR" inherits `/` and every relative path
/// in the caller's commands resolves somewhere they did not mean.
///
/// Rejected rather than warned because the symptom appears in the **guest, one build cycle
/// later**, as commands running in the wrong directory rather than as anything about
/// `WORKDIR`.
pub fn require_workdir(base: &BaseImage, dockerfile: Option<&str>) -> Result<(), Error> {
    if !base.working_dir.is_empty() {
        return Ok(());
    }
    let declares_workdir = dockerfile.is_some_and(|text| {
        text.lines().any(|line| {
            let mut words = line.split_whitespace();
            words
                .next()
                .is_some_and(|first| first.eq_ignore_ascii_case("WORKDIR"))
                && words.next().is_some()
        })
    });
    if declares_workdir {
        return Ok(());
    }
    Err(Error::invalid_arg(format!(
        "inherit_workdir was requested but base image {:?} declares no WorkingDir and the \
         Dockerfile sets none. Most public ARM64 base images leave it empty \
         (docs/PLATFORM.md, 'Most public ARM64 base images have no WORKDIR'), so there is \
         nothing to inherit and every relative path would resolve against `/`. Pass a workdir \
         to default_dockerfile, or set WORKDIR in your own Dockerfile.",
        base.name
    )))
}

/// The port in a Dockerfile's last `ENV AGENTD_PORT=`, or `None` when it sets none.
///
/// Last rather than first: a later `ENV` of the same name wins at build time, so the last
/// one is what the daemon reads. Accepts `ENV AGENTD_PORT=9000` and the legacy
/// `ENV AGENTD_PORT 9000` spelling, since both set the variable and a guard that only
/// understands one form passes the file it cannot parse.
///
/// Split-and-scan for the same reason [`dockerfile_from_ref`] is: `split_whitespace`
/// states the pattern more directly than a regex string would here.
pub fn dockerfile_agentd_port(dockerfile: &str) -> Option<u16> {
    let mut found = None;
    for line in dockerfile.lines() {
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else { continue };
        if !first.eq_ignore_ascii_case("ENV") {
            continue;
        }
        let Some(assignment) = words.next() else {
            continue;
        };
        let value = match assignment.split_once('=') {
            Some(("AGENTD_PORT", value)) => value,
            Some(_) => continue,
            // `ENV AGENTD_PORT 9000`: the value is the next word.
            None if assignment == "AGENTD_PORT" => match words.next() {
                Some(value) => value,
                None => continue,
            },
            None => continue,
        };
        // An unparseable value is not this guard's business: the daemon keeps its default
        // for one (`agentd/src/config.rs:118`), so there is no disagreement to report.
        if let Ok(port) = value.parse() {
            found = Some(port);
        }
    }
    found
}

/// Rejects a Dockerfile whose `AGENTD_PORT` disagrees with the port the create call sends
/// as `hooks.port`.
///
/// The two are set independently — `hooks.port` comes from `ControlPlane::port`, the
/// variable from the caller's own Dockerfile — and the platform calls its build-time
/// `ready`/`validate` hooks on the port in the create call. A guest listening elsewhere
/// answers nothing, so the build fails.
///
/// Rejected rather than warned because the failure points away from its cause: the docker
/// build succeeds, the log group holds a clean build *and* the daemon's own "agentd
/// listening" line, and the image still lands in `CREATE_FAILED`. Neither
/// `GetMicrovmImage` nor the build log names the port — only
/// `GetMicrovmImageVersion`'s `hooks.port` does, compared by hand against the Dockerfile.
///
/// A Dockerfile that sets no `AGENTD_PORT` is checked against
/// [`DEFAULT_AGENT_PORT`](crate::control::DEFAULT_AGENT_PORT) rather than passed, because
/// silence is not neutral: `Config::from_env` keeps its own default for an unset variable
/// (`agentd/src/config.rs:118`, default `agentd/src/config.rs:84`), so the guest listens on
/// 9000 whether or not the Dockerfile mentions a port. Against a client that changed its
/// port, the absent variable produces exactly the measured failure — and produces it for a
/// caller who never typed a port anywhere, which is the harder version to diagnose.
pub fn require_matching_agentd_port(port: u16, dockerfile: &str) -> Result<(), Error> {
    // An unparseable value lands here too, and belongs here: the daemon warns and keeps the
    // same default (`agentd/src/config.rs:174-179`), so both spellings of "the Dockerfile
    // named no usable port" have the same consequence in the guest.
    let Some(found) = dockerfile_agentd_port(dockerfile) else {
        if port == super::DEFAULT_AGENT_PORT {
            return Ok(());
        }
        return Err(Error::invalid_arg(format!(
            "this client sends hooks.port={port} but the Dockerfile sets no usable \
             AGENTD_PORT, so the daemon will listen on its own default of {default}. These \
             must agree: the platform calls the build-time ready/validate hooks on the port \
             in the create call, and a daemon listening on {default} answers none of them — \
             the docker build succeeds, the daemon logs that it is listening, and the image \
             still fails with CREATE_FAILED naming no port. Add ENV AGENTD_PORT={port} to \
             the Dockerfile, or build it with default_dockerfile, which derives the value \
             from the same port.",
            default = super::DEFAULT_AGENT_PORT,
        )));
    };
    if found == port {
        return Ok(());
    }
    Err(Error::invalid_arg(format!(
        "the Dockerfile sets ENV AGENTD_PORT={found} but this client sends hooks.port={port}. \
         These must agree: the platform calls the build-time ready/validate hooks on the port \
         in the create call, and a daemon listening on {found} answers none of them — the \
         docker build succeeds, the daemon logs that it is listening, and the image still \
         fails with CREATE_FAILED naming no port. Set ENV AGENTD_PORT={port} in the \
         Dockerfile, or build it with default_dockerfile, which derives the value from the \
         same port."
    )))
}

/// The seconds in a Dockerfile's last `ENV AGENTD_SSE_KEEPALIVE_SECS=`, or `None` when it
/// sets none.
///
/// Same shape and same reasoning as [`dockerfile_agentd_port`]: last assignment wins, both
/// `ENV` spellings, and an unparseable value reads as absent because the daemon warns and
/// keeps its default for one (`agentd/src/config.rs:174-179`).
pub fn dockerfile_env_u64(dockerfile: &str, key: &str) -> Option<u64> {
    let mut found = None;
    for line in dockerfile.lines() {
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else { continue };
        if !first.eq_ignore_ascii_case("ENV") {
            continue;
        }
        let Some(assignment) = words.next() else {
            continue;
        };
        let value = match assignment.split_once('=') {
            Some((name, value)) if name == key => value,
            Some(_) => continue,
            None if assignment == key => match words.next() {
                Some(value) => value,
                None => continue,
            },
            None => continue,
        };
        if let Ok(parsed) = value.parse() {
            found = Some(parsed);
        }
    }
    found
}

/// Rejects a Dockerfile whose `AGENTD_SSE_KEEPALIVE_SECS` is not shorter than the client's
/// stream idle timeout.
///
/// The fourth pair of this shape, found by sweeping for the other three. The daemon's SSE
/// keepalive interval and the client's tolerance for silence are set in different
/// repositories of truth — the interval in the caller's Dockerfile
/// (`agentd/src/config.rs:139`, default 15s at `:95`), the tolerance in
/// [`DEFAULT_STREAM_IDLE_TIMEOUT`](crate::session::exec::DEFAULT_STREAM_IDLE_TIMEOUT),
/// which is 60s *because* it is four times that 15. Raise the interval past the tolerance
/// and every attached stream treats a healthy connection as dead, reconnects
/// `max_reconnects` times, and fails.
///
/// The failure names both numbers and misattributes one of them: the client reports that the
/// stream "went silent for 60s, longer than the keepalive interval"
/// (`microvms-core/src/session/http.rs:383-387`), where 60 is its own timeout and the
/// keepalive interval is the number it does not know. So a reader is told the keepalive was
/// exceeded by a message that prints the wrong value for it, one build cycle after the
/// Dockerfile that caused it.
///
/// Equality is refused, not just excess: an interval exactly equal to the timeout races.
pub fn require_keepalive_under_idle_timeout(
    idle_timeout: Duration,
    dockerfile: &str,
) -> Result<(), Error> {
    let Some(secs) = dockerfile_env_u64(dockerfile, "AGENTD_SSE_KEEPALIVE_SECS") else {
        // Absent is safe here, unlike the port: the daemon's default of 15s is already
        // under every idle timeout this client will use, and a client that shortens its
        // timeout below 15s is not something a Dockerfile can be blamed for.
        return Ok(());
    };
    if secs < idle_timeout.as_secs() {
        return Ok(());
    }
    Err(Error::invalid_arg(format!(
        "the Dockerfile sets ENV AGENTD_SSE_KEEPALIVE_SECS={secs} but this client treats a \
         stream as dead after {timeout}s of silence. The keepalive must be shorter: the \
         daemon sends nothing between events except that keepalive, so an interval of \
         {secs}s makes a healthy stream look dead, and every attach reconnects until it \
         gives up. The error it raises then reports the client's own {timeout}s as though it \
         were the keepalive interval, so it names neither the Dockerfile nor {secs}. Leave \
         the variable unset for the daemon's 15s default, or keep it under {timeout}.",
        timeout = idle_timeout.as_secs(),
    )))
}

/// The value of a Dockerfile's last `ENTRYPOINT`, or `None` when it sets none.
///
/// Last rather than first for the reason [`dockerfile_agentd_port`] gives: a later
/// instruction of the same name wins at build time, so the last one is what the container
/// runs. The value is the raw rest of the line — this scan finds a disagreement, it does
/// not parse Dockerfile syntax.
pub fn dockerfile_entrypoint(dockerfile: &str) -> Option<&str> {
    last_instruction_value(dockerfile, "ENTRYPOINT")
}

/// The value of a Dockerfile's last `CMD`, or `None` when it sets none.
pub fn dockerfile_cmd(dockerfile: &str) -> Option<&str> {
    last_instruction_value(dockerfile, "CMD")
}

/// The rest of the last line whose first word is `keyword`, trimmed, or `None`.
fn last_instruction_value<'a>(dockerfile: &'a str, keyword: &str) -> Option<&'a str> {
    let mut found = None;
    for line in dockerfile.lines() {
        let trimmed = line.trim_start();
        let Some(first) = trimmed.split_whitespace().next() else {
            continue;
        };
        if first.eq_ignore_ascii_case(keyword) {
            found = Some(trimmed[first.len()..].trim());
        }
    }
    found
}

/// Whether an instruction value is the empty exec form — `[]`, with any spacing.
fn is_empty_exec_form(value: &str) -> bool {
    let mut meaningful = value.chars().filter(|c| !c.is_whitespace());
    meaningful.next() == Some('[') && meaningful.next() == Some(']') && meaningful.next().is_none()
}

/// Rejects a Dockerfile whose image would never run the daemon: no `CMD`, or an
/// `ENTRYPOINT` that swallows it.
///
/// The artifact unconditionally carries the daemon as entry `agentd` ([`build_artifact`]),
/// and the deployment invariant is `ENTRYPOINT []` plus `CMD ["/agentd"]` — see
/// [`default_dockerfile`]. A caller Dockerfile with no `CMD` runs the base's own default
/// instead, and a non-empty `ENTRYPOINT` turns any `CMD` into that entrypoint's arguments
/// rather than the process. Either way the daemon never starts, the build-time
/// `ready`/`validate` hooks go unanswered, and the image lands in `CREATE_FAILED` — or the
/// launch dies as a run-hook timeout — with nothing naming `CMD`, `ENTRYPOINT`, or the
/// artifact entry.
///
/// Weak-form on purpose: it does **not** check that the `CMD` names a path anything was
/// copied to. That would be Dockerfile interpretation rather than agreement checking, and
/// the two mistakes people actually make are the two refused here. The unenforceable half —
/// a base image that starts its own background process before bootstrap — stays with
/// whoever builds the image (`docs/PROTOCOL.md`, "Trust boundary").
pub fn require_daemon_cmd(dockerfile: &str) -> Result<(), Error> {
    if let Some(entrypoint) = dockerfile_entrypoint(dockerfile)
        && !is_empty_exec_form(entrypoint)
    {
        return Err(Error::invalid_arg(format!(
            "the Dockerfile sets ENTRYPOINT {entrypoint}, which makes any CMD its arguments \
             rather than the container's process — so the daemon this client uploads never \
             starts. The build succeeds anyway: the ready/validate hooks just go unanswered \
             and the image fails as CREATE_FAILED, or the launch dies as a run-hook timeout, \
             and neither symptom names ENTRYPOINT. Set `ENTRYPOINT []` alongside \
             `CMD [\"/agentd\"]`, or build with default_dockerfile, which sets both."
        )));
    }
    match dockerfile_cmd(dockerfile) {
        Some(cmd) if !cmd.is_empty() && !is_empty_exec_form(cmd) => Ok(()),
        _ => Err(Error::invalid_arg(
            "the Dockerfile has no CMD, so the container runs the base image's default \
             process and the daemon this client uploads never starts. The build succeeds \
             anyway: the ready/validate hooks just go unanswered and the image fails as \
             CREATE_FAILED, or the launch dies as a run-hook timeout, and neither symptom \
             names CMD. Add `ENTRYPOINT []` and `CMD [\"/agentd\"]`, or build with \
             default_dockerfile, which sets both."
                .to_string(),
        )),
    }
}

/// Rejects a Dockerfile whose `FROM` is not the selected base image.
///
/// The build runs the Dockerfile *on top of* the base named in `baseImageArn`, so the two
/// disagreeing produces an image built from something other than the platform base whose
/// behaviour every `docs/PLATFORM.md` measurement describes — and nothing in the result
/// says so.
pub fn require_matching_from(base: &BaseImage, dockerfile: &str) -> Result<(), Error> {
    let Some(found) = dockerfile_from_ref(dockerfile) else {
        // No FROM at all is not this check's business: the build will say so, and a
        // Dockerfile validator is not what this is.
        return Ok(());
    };
    if found == base.docker_ref {
        return Ok(());
    }
    // The digest-pinned spelling of the same ref: `<docker_ref>@sha256:<64 hex>`. A digest
    // names one specific manifest of the ref this check already accepts, so it is a stricter
    // statement of agreement rather than a disagreement — a supply-chain-conscious caller
    // pins exactly this way, and refusing it would force them to un-pin to pass. The tag
    // must still be present and identical: `alpine@sha256:...` against a docker_ref of
    // `alpine:3` is a different claim and stays refused.
    if let Some(digest) = found
        .strip_prefix(base.docker_ref.as_str())
        .and_then(|rest| rest.strip_prefix("@sha256:"))
        && digest.len() == 64
        && digest.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Ok(());
    }
    Err(Error::invalid_arg(format!(
        "the Dockerfile's FROM is {found:?} but base image {:?} pairs with {:?}. These must \
         agree: baseImageArn and the FROM select the same base, and a mismatch builds against \
         a base none of the measured platform behaviour applies to. Use default_dockerfile \
         with this base, or pass a BaseImage whose docker_ref matches.",
        base.name, base.docker_ref
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The archive holds exactly the two entries the build expects, named as the build
    /// looks for them.
    #[test]
    fn the_artifact_holds_a_dockerfile_and_the_daemon() {
        let bytes = build_artifact(b"\x7fELF fake daemon", "FROM scratch\n").expect("zips");
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("a readable zip");

        let mut names: Vec<String> = (0..archive.len())
            .map(|index| archive.by_index(index).expect("entry").name().to_string())
            .collect();
        names.sort();
        assert_eq!(names, ["Dockerfile", "agentd"]);
    }

    /// The daemon entry carries mode 0755, and the Dockerfile entry does not have to.
    ///
    /// Read back out of the archive rather than asserted on the constant, because the
    /// constant being right and the zip entry carrying it are two different facts — and it
    /// is the second one that turns into a run-hook timeout.
    ///
    /// The permission bits are masked out of the returned mode: `zip` reports the whole
    /// Unix mode, so the entry reads as `0o100755` — `S_IFREG | 0o755`. Comparing the
    /// unmasked value against `0o755` fails while the archive is perfectly correct, which
    /// is a test that would have sent someone to debug the writer.
    #[test]
    fn the_daemon_entry_carries_the_execute_bit() {
        let bytes = build_artifact(b"binary", "FROM scratch\n").expect("zips");
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("a readable zip");
        let mode = archive
            .by_name("agentd")
            .expect("the daemon entry")
            .unix_mode()
            .expect("a mode was recorded");
        assert_eq!(
            mode & 0o777,
            AGENTD_MODE,
            "a non-executable binary surfaces as a run-hook timeout, not a permission error \
             (mode was {mode:o})"
        );
        assert!(
            mode & 0o111 != 0,
            "the execute bit is the whole point: {mode:o}"
        );
    }

    /// The bytes round-trip. A zip that holds a truncated or re-encoded binary produces an
    /// image whose CMD fails for a reason no message names.
    #[test]
    fn the_daemon_bytes_survive_the_round_trip() {
        use std::io::Read as _;

        // Deliberately includes a NUL and a high byte, which is what a real ELF has and
        // what a text-mode write would mangle.
        let binary: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let bytes = build_artifact(&binary, "FROM scratch\n").expect("zips");
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("a readable zip");
        let mut read = Vec::new();
        archive
            .by_name("agentd")
            .expect("entry")
            .read_to_end(&mut read)
            .expect("reads");
        assert_eq!(read, binary);
    }

    /// **AC-2-3, the byte-scan guard.** The agent token must not appear anywhere in the
    /// artifact's raw bytes.
    ///
    /// A byte scan rather than an API review, because the leak this guards against is a
    /// *value* turning up somewhere — in the Dockerfile as an `ENV`, in a baked config
    /// file, in a stray argument — not a parameter being declared. Scanning the compressed
    /// bytes and the decompressed entries both, since deflate would hide a literal from a
    /// naive scan of the archive.
    ///
    /// **Falsification** — add the token to the Dockerfile (`ENV AGENT_TOKEN=…`, the
    /// plausible mistake) and the decompressed scan fails.
    #[test]
    fn the_artifact_never_carries_the_agent_token() {
        use std::io::Read as _;

        let token = "s3cr3t-agent-token-do-not-bake-me";
        let dockerfile = default_dockerfile(9000, Some("/opt/work"), &BaseImage::al2023());
        assert!(
            !dockerfile.contains(token),
            "the default Dockerfile must not mention a token"
        );

        let bytes = build_artifact(b"binary", &dockerfile).expect("zips");
        assert!(
            !bytes
                .windows(token.len())
                .any(|window| window == token.as_bytes()),
            "the token appears in the artifact's raw bytes"
        );

        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("a readable zip");
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).expect("entry");
            let name = entry.name().to_string();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).expect("reads");
            assert!(
                !content
                    .windows(token.len())
                    .any(|window| window == token.as_bytes()),
                "the token appears inside {name} — the image snapshot is shared by every VM \
                 launched from it, so a token here is a token shared with every VM"
            );
        }
    }

    /// The content hash is a pure function of the two inputs: equal inputs agree, either
    /// input changing changes it, and the boundary between the two inputs is part of the
    /// identity.
    ///
    /// The boundary case is the one worth spelling out: without length prefixes,
    /// `("ab", "c")` and `("a", "bc")` hash the same concatenation — and a Dockerfile
    /// edit could then collide with a binary edit, serving a stale image for changed
    /// inputs, which is the exact hazard the hash exists to close.
    #[test]
    fn the_content_hash_follows_the_inputs_and_only_the_inputs() {
        let hash = artifact_content_hash(b"binary-bytes", "FROM scratch\n");
        assert_eq!(
            hash,
            artifact_content_hash(b"binary-bytes", "FROM scratch\n")
        );
        assert_eq!(hash.len(), 64, "sha256 as lowercase hex");
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );

        assert_ne!(
            hash,
            artifact_content_hash(b"other-bytes", "FROM scratch\n")
        );
        assert_ne!(
            hash,
            artifact_content_hash(b"binary-bytes", "FROM scratch\nRUN true\n")
        );
        assert_ne!(
            artifact_content_hash(b"ab", "c"),
            artifact_content_hash(b"a", "bc"),
            "the input boundary is part of the identity"
        );
    }

    /// The Dockerfile's `FROM` comes from the base image, so the two cannot disagree.
    #[test]
    fn the_default_dockerfile_derives_its_from_from_the_base_image() {
        let base = BaseImage::al2023();
        let dockerfile = default_dockerfile(9000, None, &base);
        assert_eq!(
            dockerfile_from_ref(&dockerfile),
            Some(base.docker_ref.as_str())
        );
        assert!(dockerfile.contains(r#"CMD ["/agentd"]"#));
        assert!(
            dockerfile.contains("ENTRYPOINT []"),
            "the trust boundary rests on this line"
        );
        assert!(dockerfile.contains("ENV AGENTD_PORT=9000"));
    }

    /// A workdir is written as both a `mkdir` and a `WORKDIR`, and is absent when none was
    /// asked for. An empty string counts as none, since that is what an unset value looks
    /// like coming from a CLI flag.
    #[test]
    fn a_workdir_is_created_and_set_or_absent_entirely() {
        let base = BaseImage::al2023();
        let with = default_dockerfile(9000, Some("/opt/baked-workdir"), &base);
        assert!(with.contains("RUN mkdir -p /opt/baked-workdir"));
        assert!(with.contains("WORKDIR /opt/baked-workdir"));

        for none in [None, Some("")] {
            let without = default_dockerfile(9000, none, &base);
            assert!(!without.contains("WORKDIR"), "{without}");
        }
    }

    /// `FROM` parsing tolerates the decoration a real Dockerfile carries: lowercase,
    /// leading whitespace, a `--platform` flag, and an `AS` alias.
    #[test]
    fn the_from_parser_ignores_decoration_rather_than_validating_syntax() {
        assert_eq!(dockerfile_from_ref("FROM alpine\n"), Some("alpine"));
        assert_eq!(dockerfile_from_ref("  from alpine:3\n"), Some("alpine:3"));
        assert_eq!(
            dockerfile_from_ref("FROM --platform=linux/arm64 alpine AS build\n"),
            Some("alpine")
        );
        assert_eq!(dockerfile_from_ref("RUN echo from nowhere\n"), None);
        assert_eq!(dockerfile_from_ref(""), None);
    }

    /// A `FROM` that disagrees with the base image is refused, and the message names both
    /// refs — the one found and the one expected — because "these must agree" without both
    /// values leaves the caller to guess which to change.
    #[test]
    fn a_dockerfile_from_that_disagrees_with_the_base_is_refused() {
        let base = BaseImage::al2023();
        let error = require_matching_from(&base, "FROM ubuntu:24.04\nCOPY agentd /agentd\n")
            .expect_err("ubuntu is not the managed base");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        let message = error.to_string();
        assert!(message.contains("ubuntu:24.04"), "{message}");
        assert!(message.contains(&base.docker_ref), "{message}");
        assert!(
            message.contains("none of the measured platform behaviour applies"),
            "{message}"
        );

        require_matching_from(&base, &default_dockerfile(9000, None, &base))
            .expect("the derived Dockerfile agrees with its own base");
    }

    /// A digest-pinned `FROM` of the agreeing ref passes: it names one specific manifest of
    /// the base this check already accepts, which is the spelling a supply-chain-conscious
    /// caller uses. The boundary cases stay refused — a digest on a *different* ref is still
    /// a disagreement, and a malformed digest is not a digest.
    #[test]
    fn a_digest_pinned_from_of_the_agreeing_ref_passes() {
        let base = BaseImage::al2023();
        let digest = "c439fb4994ea7ca529233d6256446d3f8b7b4efb58956073e015303a170011de";
        let pinned = format!(
            "FROM {}@sha256:{digest}\nCOPY agentd /agentd\n",
            base.docker_ref
        );
        require_matching_from(&base, &pinned).expect("a digest pin of the same ref agrees");

        require_matching_from(&base, &format!("FROM ubuntu:24.04@sha256:{digest}\n"))
            .expect_err("a digest does not launder a different ref");
        require_matching_from(&base, &format!("FROM {}@sha256:abc123\n", base.docker_ref))
            .expect_err("a 6-character digest is not a digest");
        require_matching_from(
            &base,
            &format!("FROM {}@sha256:{}\n", base.docker_ref, "g".repeat(64)),
        )
        .expect_err("64 non-hex characters are not a digest");
    }

    /// A Dockerfile with no `FROM` is not this check's business: the build will say so, and
    /// refusing here would be a Dockerfile validator rather than an agreement check.
    #[test]
    fn a_dockerfile_with_no_from_is_left_to_the_build() {
        require_matching_from(&BaseImage::al2023(), "COPY agentd /agentd\n")
            .expect("no FROM is not a disagreement");
    }

    /// The measured case, reproduced from the Dockerfile that spent it: a hand-written
    /// guest Dockerfile carrying `ENV AGENTD_PORT=8080` — the plausible port, and the wrong
    /// one — against a client sending `hooks.port=9000`. The build succeeds, the daemon logs
    /// `agentd listening`, and the image lands in `CREATE_FAILED` naming no port.
    #[test]
    fn a_dockerfile_port_that_disagrees_with_the_hook_port_is_refused() {
        let error = require_matching_agentd_port(
            crate::control::DEFAULT_AGENT_PORT,
            "FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal\n\
             COPY agentd /agentd\n\
             ENV AGENTD_PORT=8080\n\
             EXPOSE 8080\n\
             CMD [\"/agentd\"]\n",
        )
        .expect_err("8080 is not the port the create call sends");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        let message = error.to_string();
        assert!(message.contains("8080"), "{message}");
        assert!(message.contains("9000"), "{message}");
        // The remedy is what a reader acts on, and the diagnostic's value is naming the
        // symptom that points away from the cause.
        assert!(message.contains("CREATE_FAILED"), "{message}");
        assert!(message.contains("ready/validate"), "{message}");

        require_matching_agentd_port(
            crate::control::DEFAULT_AGENT_PORT,
            &default_dockerfile(
                crate::control::DEFAULT_AGENT_PORT,
                Some("/work"),
                &BaseImage::al2023(),
            ),
        )
        .expect("the derived Dockerfile agrees with the port it was derived from");
    }

    /// Both `ENV` spellings set the variable, so a guard reading only `KEY=VALUE` would pass
    /// the legacy form it could not parse. The last assignment wins, as it does at build
    /// time.
    #[test]
    fn the_port_scan_reads_both_env_spellings_and_takes_the_last() {
        assert_eq!(dockerfile_agentd_port("ENV AGENTD_PORT=9000\n"), Some(9000));
        assert_eq!(dockerfile_agentd_port("ENV AGENTD_PORT 8080\n"), Some(8080));
        assert_eq!(dockerfile_agentd_port("env agentd_port=7000\n"), None);
        assert_eq!(
            dockerfile_agentd_port("ENV AGENTD_PORT=8080\nENV AGENTD_PORT=9000\n"),
            Some(9000),
        );
        assert_eq!(dockerfile_agentd_port("ENV AGENTD_LOG=info\n"), None);
        // Neither a missing variable nor an unparseable one is a disagreement: the daemon
        // keeps its own default for both (`agentd/src/config.rs:118`).
        assert_eq!(
            dockerfile_agentd_port("FROM x\nCOPY agentd /agentd\n"),
            None
        );
        assert_eq!(dockerfile_agentd_port("ENV AGENTD_PORT=nine\n"), None);
        require_matching_agentd_port(
            crate::control::DEFAULT_AGENT_PORT,
            "FROM x\nCOPY agentd /agentd\n",
        )
        .expect("no variable agrees with the default, which is what the daemon will use");
    }

    /// The other half of the pair, and the harder one to diagnose: a Dockerfile that names
    /// no port at all against a client that moved off the default. Silence is not neutral —
    /// `Config::from_env` keeps `9000` for an unset variable — so the guest listens on 9000
    /// while the hooks are dialled on the client's port, which is the measured failure with
    /// nothing in the Dockerfile to point at.
    #[test]
    fn a_dockerfile_naming_no_port_is_refused_when_the_client_moved_off_the_default() {
        let error = require_matching_agentd_port(
            8080,
            "FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal\n\
             COPY agentd /agentd\n\
             CMD [\"/agentd\"]\n",
        )
        .expect_err("a silent Dockerfile leaves the daemon on 9000, not on 8080");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        let message = error.to_string();
        assert!(message.contains("8080"), "{message}");
        assert!(
            message.contains(&crate::control::DEFAULT_AGENT_PORT.to_string()),
            "the daemon's default is the value the reader has to learn: {message}"
        );
        assert!(message.contains("CREATE_FAILED"), "{message}");
        assert!(message.contains("ready/validate"), "{message}");

        // An unparseable value has the same consequence in the guest as no value, so it is
        // refused for the same reason rather than passed as "not this guard's business".
        require_matching_agentd_port(8080, "FROM x\nENV AGENTD_PORT=nine\n")
            .expect_err("a value the daemon cannot parse leaves it on its own default");

        // The default-port client is the common case and stays silent: the Dockerfile that
        // says nothing and the client that changed nothing already agree.
        require_matching_agentd_port(
            crate::control::DEFAULT_AGENT_PORT,
            "FROM x\nCOPY agentd /agentd\n",
        )
        .expect("silence agrees with the default");
    }

    /// The fourth pair of the `FROM`/`WORKDIR`/`AGENTD_PORT` shape, found by sweeping for
    /// the others: a keepalive interval the client's silence tolerance is shorter than. The
    /// resulting error prints the client's own timeout as though it were the keepalive, so
    /// the number a reader would search for never appears.
    #[test]
    fn a_keepalive_at_or_over_the_client_idle_timeout_is_refused() {
        let timeout = crate::session::exec::DEFAULT_STREAM_IDLE_TIMEOUT;
        assert_eq!(timeout.as_secs(), 60, "the guard's arithmetic assumes this");

        let error =
            require_keepalive_under_idle_timeout(timeout, "ENV AGENTD_SSE_KEEPALIVE_SECS=90\n")
                .expect_err("90s of scheduled silence exceeds a 60s tolerance");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        let message = error.to_string();
        assert!(message.contains("90"), "{message}");
        assert!(message.contains("60"), "{message}");

        // Equality races rather than passing: the timeout fires on the same tick the
        // keepalive is due.
        require_keepalive_under_idle_timeout(timeout, "ENV AGENTD_SSE_KEEPALIVE_SECS=60\n")
            .expect_err("an interval equal to the tolerance is a race, not a margin");

        require_keepalive_under_idle_timeout(timeout, "ENV AGENTD_SSE_KEEPALIVE_SECS 30\n")
            .expect("30s leaves a margin, in the legacy spelling");
        // The daemon's own default is 15s, so silence is safe for this pair.
        require_keepalive_under_idle_timeout(timeout, "FROM x\nCOPY agentd /agentd\n")
            .expect("an unset keepalive leaves the daemon at 15s");
        require_keepalive_under_idle_timeout(
            timeout,
            &default_dockerfile(
                crate::control::DEFAULT_AGENT_PORT,
                Some("/work"),
                &BaseImage::al2023(),
            ),
        )
        .expect("the derived Dockerfile sets no keepalive");
    }

    /// **Issue #46, both refusable halves.** A Dockerfile with no `CMD` builds an image
    /// that runs the base's default process; a non-empty `ENTRYPOINT` turns the `CMD` into
    /// its arguments. Either way the daemon the artifact carries never starts, and the
    /// failure surfaces as `CREATE_FAILED` or a run-hook timeout naming neither
    /// instruction — so the message must name both the instruction and that symptom.
    #[test]
    fn a_dockerfile_that_never_runs_the_daemon_is_refused_naming_the_symptom() {
        // No CMD at all: the base's default applies, which is not /agentd.
        let error = require_daemon_cmd(
            "FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal\n\
             COPY agentd /agentd\n\
             ENV AGENTD_PORT=9000\n",
        )
        .expect_err("no CMD means the daemon never starts");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        let message = error.to_string();
        assert!(message.contains("no CMD"), "{message}");
        assert!(message.contains("run-hook timeout"), "{message}");
        assert!(message.contains("CREATE_FAILED"), "{message}");

        // A non-empty ENTRYPOINT swallows the CMD as its arguments.
        let error = require_daemon_cmd(
            "FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal\n\
             COPY agentd /agentd\n\
             ENTRYPOINT [\"/bin/sh\", \"-c\"]\n\
             CMD [\"/agentd\"]\n",
        )
        .expect_err("a non-empty ENTRYPOINT makes CMD its arguments");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        let message = error.to_string();
        assert!(message.contains("ENTRYPOINT"), "{message}");
        assert!(message.contains("run-hook timeout"), "{message}");

        // An empty CMD is the same absence in different spelling.
        require_daemon_cmd("FROM x\nCMD []\n").expect_err("CMD [] runs nothing");
    }

    /// The shapes that must pass: the deployment invariant itself, in exec and spaced
    /// spellings, and the invariant's `ENTRYPOINT []` with any interior whitespace. The
    /// derived Dockerfile passes its own guard, which is what keeps the default path
    /// unaffected.
    #[test]
    fn the_deployment_invariant_passes_the_daemon_cmd_guard() {
        require_daemon_cmd(&default_dockerfile(
            9000,
            Some("/work"),
            &BaseImage::al2023(),
        ))
        .expect("the derived Dockerfile is the invariant");
        require_daemon_cmd("FROM x\nENTRYPOINT []\nCMD [\"/agentd\"]\n").expect("the invariant");
        require_daemon_cmd("FROM x\nENTRYPOINT [ ]\nCMD [\"/agentd\"]\n")
            .expect("interior whitespace is still the empty exec form");
        require_daemon_cmd("FROM x\ncmd [\"/agentd\"]\n").expect("case-insensitive, no ENTRYPOINT");
        // Last instruction wins, as at build time: a later empty ENTRYPOINT un-swallows.
        require_daemon_cmd("FROM x\nENTRYPOINT [\"/bin/sh\"]\nENTRYPOINT []\nCMD [\"/agentd\"]\n")
            .expect("the last ENTRYPOINT is the one the build uses");
        // And the reverse ordering is refused for the same reason.
        require_daemon_cmd("FROM x\nENTRYPOINT []\nENTRYPOINT [\"/bin/sh\"]\nCMD [\"/agentd\"]\n")
            .expect_err("the last ENTRYPOINT is non-empty");
    }

    /// Workdir inheritance is refused when neither the base nor the Dockerfile declares
    /// one, and the message says where the symptom would have appeared — in the guest, a
    /// build cycle later.
    #[test]
    fn inheriting_a_workdir_nothing_declares_is_refused() {
        let base = BaseImage::al2023();
        assert!(base.working_dir.is_empty(), "the measured case");

        let error = require_workdir(&base, None).expect_err("nothing declares a workdir");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        let message = error.to_string();
        assert!(message.contains("no WorkingDir"), "{message}");
        assert!(message.contains("nothing to inherit"), "{message}");
        assert!(message.contains("docs/PLATFORM.md"), "{message}");
    }

    /// It is accepted when *either* side declares one — the Dockerfile's `WORKDIR` or the
    /// base image's own `working_dir`. Both directions, because a check that only read the
    /// Dockerfile would refuse a caller on a purpose-built image that declares one.
    #[test]
    fn either_the_dockerfile_or_the_base_may_supply_the_workdir() {
        let base = BaseImage::al2023();
        require_workdir(&base, Some("FROM x\nWORKDIR /srv\n"))
            .expect("the Dockerfile declares one");

        let purpose_built = BaseImage {
            working_dir: "/app".to_string(),
            ..BaseImage::al2023()
        };
        require_workdir(&purpose_built, None).expect("the base image declares one");
    }

    /// A bare `WORKDIR` with no argument does not count as declaring one. It is the shape a
    /// truncated edit leaves behind, and treating it as a declaration would pass the guard
    /// on a Dockerfile that sets nothing.
    #[test]
    fn a_workdir_with_no_argument_does_not_count_as_a_declaration() {
        let error = require_workdir(&BaseImage::al2023(), Some("FROM x\nWORKDIR\n"))
            .expect_err("WORKDIR with no path declares nothing");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
    }

    /// The base image ARN, for the region it is requested in.
    ///
    /// `microvm-image:` with a **colon**, which is the spelling for a managed base *and* for
    /// a customer image alike — measured 2026-08-15 against real ARNs in us-east-1, and the
    /// only form the model's `TaggableResource` pattern admits. The docs above this function
    /// carry the measurement.
    ///
    /// **Falsification** — change the separator to `/` and both assertions go red. That was
    /// the state twelve fakes and the transport encoding test were in, which is why this
    /// test now asserts the separator explicitly rather than only the region.
    #[test]
    fn the_base_image_arn_names_the_request_region_with_a_colon_separator() {
        assert_eq!(
            BaseImage::al2023().arn(&crate::region::Region::UsEast1),
            "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1"
        );
        assert_eq!(
            BaseImage::al2023().arn(&crate::region::Region::ApNortheast1),
            "arn:aws:lambda:ap-northeast-1:aws:microvm-image:al2023-1"
        );
        assert!(
            !BaseImage::al2023()
                .arn(&crate::region::Region::UsEast1)
                .contains("microvm-image/"),
            "the slash form answers AccessDeniedException, not a validation error"
        );
    }
}

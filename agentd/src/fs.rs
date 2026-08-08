//! File transfer: one file at a time, or a streamed tar in either direction.
//!
//! # Why the single-file routes are not confined to a root
//!
//! `PUT /v1/fs/file` writes wherever the caller asks, and `GET` reads from
//! wherever the caller asks. That is deliberate, and it was argued with a
//! reviewer rather than overlooked. The same bearer token authorizes
//! `POST /v1/exec/start`, which runs arbitrary commands as root by design, so a
//! token holder can already reach every byte in the VM with one exec call. A root
//! prefix here would add no security while breaking real behavior: harnesses write
//! credentials into home directories, drop config into `/etc`, and stage scratch
//! files in `/tmp`.
//!
//! The confinement that does matter is on `PUT /v1/fs/tar`, because there the
//! member paths come out of an uploaded archive rather than from a caller who
//! named them. An archive can carry a path its uploader never intended, and that
//! gap is the entire traversal class.
//!
//! # Extraction policy
//!
//! Extraction mirrors the CPython `tarfile` `data` filter. Compatibility is a hard
//! requirement, not a nicety: Harbor's `pack_dir_to_bytes` packs with
//! `follow_symlinks=False` and therefore *preserves* symlinks, and an earlier
//! version of this daemon refused every link member. That refusal would have
//! broken `upload_dir` for any environment, skills tree, agent directory, or
//! verifier test directory containing a single symlink — a worse outcome than the
//! traversal hole it was guarding.
//!
//! The rules, each with a test below, all checked against CPython 3.14's
//! `_get_filtered_attrs` rather than approximated:
//!
//! * An in-tree symlink is created as a symlink, target preserved verbatim.
//! * An absolute link target is refused outright.
//! * A relative link target must resolve under the extraction root.
//! * A symlink resolves relative to *its own directory*; a hard link resolves
//!   against the *archive root*. These really are different bases: a `d/s` symlink
//!   with target `target.txt` extracts dangling, because it means `d/target.txt`,
//!   while `d/h` as a hard link to `target.txt` finds the file at the root.
//! * Resolution is lexical component walking, never `realpath`/`canonicalize`.
//! * Device and FIFO members are refused.
//! * Member count and total uncompressed size are capped.
//! * Modes are applied in a deferred second pass, after all content lands.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::TryStreamExt;
use tar::EntryType;
use tokio_util::io::{ReaderStream, StreamReader};

use crate::disk::{self, CopyError};
use crate::state::AppState;

/// The one typed shape in this module, re-exported from its original path. The
/// bodies here are opaque byte streams, so the query is the whole wire contract.
pub use protocol::fs::FsQuery;

/// The refusal a write gets when the target filesystem is under the configured
/// reserve.
///
/// 507 rather than 500, and the numbers are in the body. A 500 is
/// indistinguishable from a daemon defect, so a client retries it — which is
/// correct for a defect and actively harmful for a full disk. 507 is
/// `Insufficient Storage`, is not in any client's retry set, and naming the actual
/// free space is what turns "it failed" into "the disk is nearly full", which is
/// the whole reason this guard exists. See `disk`.
fn insufficient_storage(path: &Path, reading: disk::Reading) -> Response {
    tracing::warn!(
        path = %path.display(),
        available = reading.available,
        reserve = reading.reserve,
        "refusing a write: the target filesystem is under the disk reserve",
    );
    (
        StatusCode::INSUFFICIENT_STORAGE,
        format!(
            "refusing to write {}: {} bytes available on the target filesystem, \
             below the {} byte reserve",
            path.display(),
            reading.available,
            reading.reserve,
        ),
    )
        .into_response()
}

/// Why an archive or member was refused.
///
/// The refused member's name travels with the refusal so the 400 can name it. A
/// 400 that only says "bad archive" sends the caller re-reading their whole tree.
#[derive(Debug)]
enum Refusal {
    /// A member violated the data-filter contract. 400.
    Member { member: String, reason: String },
    /// The archive exceeded a configured cap. 413.
    TooLarge(String),
    /// The target filesystem crossed the disk reserve partway through. 507. Carries
    /// the path so the response can name the filesystem that filled rather than
    /// just the archive.
    Pressure(PathBuf, disk::Reading),
    /// Filesystem or stream failure. 500.
    Io(io::Error),
}

impl From<io::Error> for Refusal {
    fn from(err: io::Error) -> Self {
        Refusal::Io(err)
    }
}

impl Refusal {
    fn member(member: &Path, reason: impl Into<String>) -> Self {
        Refusal::Member {
            member: member.display().to_string(),
            reason: reason.into(),
        }
    }

    fn into_response(self) -> Response {
        match self {
            Refusal::Member { member, reason } => {
                tracing::warn!(member, reason, "tar member refused");
                (
                    StatusCode::BAD_REQUEST,
                    format!("refused tar member {member}: {reason}"),
                )
                    .into_response()
            }
            Refusal::TooLarge(detail) => {
                tracing::warn!(detail, "archive over cap");
                (StatusCode::PAYLOAD_TOO_LARGE, detail).into_response()
            }
            // 507 rather than 413: the archive is not too big for the *protocol*,
            // it is too big for this filesystem right now. The distinction is
            // actionable — a caller retries a 507 after freeing space, and never
            // retries a 413.
            Refusal::Pressure(path, reading) => insufficient_storage(&path, reading),
            Refusal::Io(err) => {
                tracing::error!(%err, "tar operation failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "tar operation failed").into_response()
            }
        }
    }
}

/// Lexically normalizes `path`, or returns `None` if it escapes.
///
/// "Escapes" means a rooted or prefix component appears anywhere, or a `..` pops
/// past depth zero. Every decision is made on the string: no `canonicalize`, no
/// `stat`, nothing that consults the filesystem.
///
/// That choice is the crux of this module. `realpath` asks the *live* filesystem
/// where a path leads — but during extraction we are the ones building that
/// filesystem. An archive can write `d -> /` as a symlink in member 1 and then
/// `d/etc/passwd` in member 2; a `realpath` check on member 2 follows the symlink
/// we just created and resolves outside the root. Lexical resolution cannot be
/// influenced by anything the archive already wrote, so member 2 is judged as the
/// literal `root/d/etc/passwd` and the redirect is simply not reachable.
fn normalize(path: &Path) -> Option<Vec<std::ffi::OsString>> {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            // A leading `/` or a Windows-style prefix is refused rather than
            // stripped. CPython strips a leading separator from a member *name*,
            // but silently rewriting a path the caller asked for is worse here
            // than refusing it: the caller learns nothing from a rewrite.
            Component::RootDir | Component::Prefix(_) => return None,
            Component::CurDir => {}
            // A lexical pop, so `a/../b` is `b` whether or not `a` is a symlink.
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(part) => parts.push(part.to_os_string()),
        }
    }
    Some(parts)
}

/// Whether a link target stays inside the root, given the depth of the directory
/// it resolves from.
///
/// `base_depth` is how many components deep the target is interpreted from: the
/// link's own parent directory for a symlink, and zero (the archive root) for a
/// hard link. Both bases were confirmed against CPython before being written down.
///
/// A target that lands exactly on the root is in-tree. CPython permits that too;
/// its `target_path == dest_path` guard applies to the member's own *name*, which
/// [`resolve_member`] rejects separately.
fn link_target_is_in_tree(base_depth: usize, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut depth = base_depth;
    for component in target.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => return false,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(next) => depth = next,
                None => return false,
            },
            Component::Normal(_) => depth += 1,
        }
    }
    true
}

/// Where a vetted member lands.
#[derive(Debug, Eq, PartialEq)]
enum Landing {
    /// A path under the root, with its normalized component depth — which a
    /// symlink target needs as its resolution base.
    Under { dest: PathBuf, depth: usize },
    /// The member names the root itself, i.e. `.` or `./`.
    ///
    /// Kept distinct rather than folded into a refusal because
    /// `Builder::append_dir_all(".", root)` — how [`pack_tree`] builds a download,
    /// and how GNU tar packs a tree — emits exactly this member for the top
    /// directory. Refusing it outright would mean a `GET /v1/fs/tar` archive could
    /// not be handed back to `PUT /v1/fs/tar`, which is the one round trip a
    /// harness performs constantly. Whether it is *allowed* still depends on the
    /// member's type; only a directory is harmless here.
    Root,
    /// The path escapes the root.
    Escapes,
}

/// Vets a member path against `root`.
fn resolve_member(root: &Path, path: &Path) -> Landing {
    let Some(parts) = normalize(path) else {
        return Landing::Escapes;
    };
    if parts.is_empty() {
        return Landing::Root;
    }
    let depth = parts.len();
    let mut dest = root.to_path_buf();
    dest.extend(parts);
    Landing::Under { dest, depth }
}

/// Caps applied to one extraction, carried separately from [`crate::Config`] so
/// the engine can be exercised at small bounds in tests without a whole `AppState`.
#[derive(Clone, Copy, Debug)]
struct Caps {
    max_members: u64,
    max_bytes: u64,
}

/// Extracts `archive` into `root`, enforcing the data-filter contract.
///
/// Synchronous on purpose: `tar`'s reader is blocking, so callers hand this to
/// `spawn_blocking` rather than pretending it is async.
fn extract_into(
    root: &Path,
    archive: impl Read,
    caps: Caps,
    guard: &disk::Guard,
) -> Result<u64, Refusal> {
    std::fs::create_dir_all(root)?;

    let mut ar = tar::Archive::new(archive);
    // Ownership is dropped for the same reason CPython's `data` filter drops it:
    // the archive's uid/gid are the packing host's, meaningless here, and honoring
    // them would need privilege we should not spend.
    ar.set_preserve_permissions(false);
    ar.set_preserve_ownerships(false);
    ar.set_unpack_xattrs(false);

    // Deferred modes. A directory packed 0o500 would block every write into it if
    // its mode were applied when the directory was created, so the mode is
    // recorded and replayed after all content has landed. This was a real defect:
    // extraction failed partway through with a permission error on a tree that tar
    // itself unpacks fine.
    let mut deferred: HashMap<PathBuf, u32> = HashMap::new();
    let mut members = 0u64;
    let mut total = 0u64;
    // `max_tar_bytes` defaults to 8 GiB, which is far more than the default reserve,
    // so the size cap does not imply the disk survives the extraction. The pacer
    // re-checks as members land.
    let mut pacer = disk::Pacer::new(*guard);

    for entry in ar.entries()? {
        let mut entry = entry?;

        members += 1;
        if members > caps.max_members {
            return Err(Refusal::TooLarge(format!(
                "archive has more than {} members",
                caps.max_members
            )));
        }

        // `entry.path()`, `entry.link_name()` and `entry.size()` are the correct
        // accessors: they consult GNU long-name/long-link extension members and
        // PAX overrides. Their `entry.header()` counterparts read the fixed 100-
        // byte fields and are silently truncated, and the header's size field is
        // not PAX-aware — a mismatch there is exactly RUSTSEC-2026-0068, where one
        // archive parsed differently across extractors.
        let path = entry.path()?.into_owned();
        let kind = entry.header().entry_type();

        let (dest, depth) = match resolve_member(root, &path) {
            Landing::Under { dest, depth } => (dest, depth),
            Landing::Escapes => {
                return Err(Refusal::member(&path, "path escapes the extraction root"));
            }
            // The top-level `./` that `append_dir_all` and GNU tar both emit. A
            // directory here means "the root", which already exists, so it is a
            // no-op. Anything else — a file or, worse, a link — would replace the
            // destination directory and redirect every later member, which is the
            // case CPython refuses as `target_path == dest_path`.
            Landing::Root => {
                if kind == EntryType::Directory {
                    continue;
                }
                return Err(Refusal::member(
                    &path,
                    "only a directory member may name the extraction root",
                ));
            }
        };

        match kind {
            // Refused for the same reason CPython's `data` filter refuses them: a
            // device node is not data, and creating one needs privilege that
            // extracting an upload has no business exercising.
            EntryType::Char | EntryType::Block | EntryType::Fifo => {
                return Err(Refusal::member(
                    &path,
                    "device and fifo members are not extractable",
                ));
            }

            EntryType::Directory => {
                std::fs::create_dir_all(&dest)?;
                if let Ok(mode) = entry.header().mode() {
                    deferred.insert(dest, mode);
                }
            }

            EntryType::Symlink | EntryType::Link => {
                let Some(target) = entry.link_name()? else {
                    return Err(Refusal::member(&path, "link member has no target"));
                };
                let target = target.into_owned();

                if target.is_absolute() {
                    return Err(Refusal::member(
                        &path,
                        format!("absolute link target {}", target.display()),
                    ));
                }

                // The two bases. A symlink is interpreted by the kernel relative
                // to the directory holding it, so its base is its own parent —
                // `depth - 1`. A hard link's target is an archive-relative member
                // name, so its base is the root, depth 0.
                let base_depth = if kind == EntryType::Symlink {
                    depth - 1
                } else {
                    0
                };
                if !link_target_is_in_tree(base_depth, &target) {
                    return Err(Refusal::member(
                        &path,
                        format!("link target {} resolves outside the root", target.display()),
                    ));
                }

                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // Replacing rather than failing on an existing name: an archive
                // may legitimately overwrite, and a partial extraction that has to
                // be retried should converge.
                let _ = std::fs::remove_file(&dest);

                if kind == EntryType::Symlink {
                    // The target is written verbatim, dangling or not. tar stores
                    // the link, not what it points at, and a tree packed in
                    // dependency-free order can carry a symlink whose target
                    // arrives in a later member — or never, which is still what
                    // the source tree looked like.
                    std::os::unix::fs::symlink(&target, &dest)?;
                } else {
                    let mut source = root.to_path_buf();
                    source.extend(normalize(&target).into_iter().flatten());
                    std::fs::hard_link(&source, &dest)?;
                }
            }

            // Regular, contiguous, and anything else carrying data.
            _ => {
                let size = entry.size();
                total = total.saturating_add(size);
                if total > caps.max_bytes {
                    return Err(Refusal::TooLarge(format!(
                        "archive exceeds {} uncompressed bytes",
                        caps.max_bytes
                    )));
                }

                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut file = std::fs::File::create(&dest)?;
                // Streamed through `Entry`'s `Read` impl. `Entry::unpack` would
                // apply no confinement at all, and `unpack_in` applies a
                // link-target policy that differs from ours — it rejects targets
                // this contract must preserve.
                let written = io::copy(&mut entry, &mut file)?;

                if let Ok(mode) = entry.header().mode() {
                    deferred.insert(dest, mode);
                }

                // Checked after the member landed, so a member that fits is never
                // refused, and the refusal names the extraction root because that is
                // the filesystem that filled. Members already extracted are left in
                // place: extraction is not transactional by design — the module's
                // existing contract is that a partial extraction "should converge"
                // on retry — and deleting a tree the caller may have had other
                // members in is a worse failure than a partial one they can inspect.
                if let Some(reading) = pacer.record(written, root) {
                    return Err(Refusal::Pressure(root.to_path_buf(), reading));
                }
            }
        }
    }

    apply_deferred_modes(deferred);
    Ok(members)
}

/// Replays recorded modes once every member has landed.
///
/// Deepest paths first, so a directory made read-only does not block the chmod of
/// something beneath it. Modes are masked to `0o755` and a failure is logged
/// rather than fatal: the bytes are already correct, and the caller would rather
/// have a tree with a stale mode than a 500 and no tree.
fn apply_deferred_modes(deferred: HashMap<PathBuf, u32>) {
    let mut entries: Vec<(PathBuf, u32)> = deferred.into_iter().collect();
    entries.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));

    for (path, mode) in entries {
        // Masked like CPython's `data` filter: no setuid/setgid/sticky out of an
        // upload, and no group- or other-writable bits.
        let masked = mode & 0o755;
        if let Err(err) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(masked)) {
            tracing::warn!(path = %path.display(), %err, "could not apply archive mode");
        }
    }
}

/// Parses an octal mode string.
///
/// Always base 8, so `644` and `0644` and `0o644` all mean the same thing. A mode
/// that does not parse is 400 — and it is checked *before* any byte is written,
/// because the predecessor validated after writing and left a file on disk with
/// the wrong permissions after answering an error.
fn parse_mode(raw: &str) -> Option<u32> {
    let digits = raw
        .trim()
        .trim_start_matches("0o")
        .trim_start_matches("0O")
        .trim_start_matches('0');
    // An all-zeros input trims to empty and genuinely means mode 0.
    if digits.is_empty() {
        return if raw.trim().chars().all(|c| c == '0') && !raw.trim().is_empty() {
            Some(0)
        } else {
            None
        };
    }
    let mode = u32::from_str_radix(digits, 8).ok()?;
    // Refuse anything above the permission and setuid/setgid/sticky bits: a
    // caller passing a whole `st_mode` including the file-type bits is confused,
    // and honoring it would set bits `chmod` cannot express.
    (mode <= 0o7777).then_some(mode)
}

/// Streams a request body into an anonymous spool file and rewinds it.
///
/// The archive is never held in memory. The predecessor buffered whole archives on
/// a VM whose baseline can be 512 MiB, where an OOM-killed daemon is unrecoverable
/// — the platform forwards no traffic to a dead process and nothing inside the VM
/// restarts it.
///
/// The spool is a `tempfile::tempfile()`, which is unlinked at creation, so a
/// crash mid-upload leaks no path and the kernel reclaims the space.
async fn spool_body(body: Body, guard: &disk::Guard) -> Result<std::fs::File, SpoolError> {
    let stream = body.into_data_stream().map_err(io::Error::other);
    let mut reader = StreamReader::new(stream);

    let spool = tempfile::tempfile()?;
    let mut writer = tokio::fs::File::from_std(spool.try_clone()?);

    // The spool's own filesystem is what this copy fills, and it is not necessarily
    // the extraction root's. `tempfile()` honors TMPDIR and falls back to /tmp, so
    // that is the path the guard measures. Measuring `root` here instead would watch
    // the wrong filesystem run out.
    let spool_dir = std::env::temp_dir();
    disk::copy_guarded(&mut reader, &mut writer, guard, &spool_dir)
        .await
        // The partial spool needs no cleanup: `tempfile()` unlinks at creation, so
        // dropping the handle returns every byte to the filesystem.
        .map_err(|(_written, err)| match err {
            CopyError::Pressure(reading) => SpoolError::Pressure(reading),
            CopyError::Io(err) => SpoolError::Io(err),
        })?;
    tokio::io::AsyncWriteExt::flush(&mut writer).await?;

    let mut spool = spool;
    spool.seek(SeekFrom::Start(0))?;
    Ok(spool)
}

/// Why spooling stopped. Separates "the disk filled" from "the wire failed", which
/// the caller maps to 507 and 400 respectively.
#[derive(Debug)]
enum SpoolError {
    Pressure(disk::Reading),
    Io(io::Error),
}

impl From<io::Error> for SpoolError {
    fn from(err: io::Error) -> Self {
        SpoolError::Io(err)
    }
}

/// Reads one file. 404 only when the path is genuinely absent.
pub async fn read_file(request: Request) -> Response {
    let Ok(query) = axum::extract::Query::<FsQuery>::try_from_uri(request.uri()) else {
        return (StatusCode::BAD_REQUEST, "path query parameter is required").into_response();
    };
    let path = PathBuf::from(&query.path);

    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // Genuinely absent, so 404 is honest here — this is the one place in
            // the module where a client's FileNotFoundError is the right mapping.
            return (StatusCode::NOT_FOUND, "no such file").into_response();
        }
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "cannot open file for read");
            return (StatusCode::INTERNAL_SERVER_ERROR, "cannot read file").into_response();
        }
    };

    // A directory opens successfully and then fails on read, which would surface
    // as a 500 mid-stream after the status line is already sent. Checked up front.
    match file.metadata().await {
        Ok(meta) if meta.is_dir() => {
            return (
                StatusCode::BAD_REQUEST,
                "path is a directory; use /v1/fs/tar",
            )
                .into_response();
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "cannot stat file for read");
            return (StatusCode::INTERNAL_SERVER_ERROR, "cannot read file").into_response();
        }
    }

    // Streamed rather than read into a Vec, for the same memory reason as upload.
    let body = Body::from_stream(ReaderStream::new(file));
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/octet-stream")],
        body,
    )
        .into_response()
}

/// Writes one file. Not confined to a root; see the module comment.
pub async fn write_file(State(state): State<AppState>, request: Request) -> Response {
    let Ok(query) = axum::extract::Query::<FsQuery>::try_from_uri(request.uri()) else {
        return (StatusCode::BAD_REQUEST, "path query parameter is required").into_response();
    };
    let path = PathBuf::from(&query.path);
    let guard = state.disk_guard();

    // Mode is parsed before a single byte lands. The predecessor validated after
    // writing, so a bad mode answered 400 while leaving the file behind.
    let mode = match query.mode.as_deref() {
        None => None,
        Some(raw) => match parse_mode(raw) {
            Some(mode) => Some(mode),
            // 400, not 404: a bad mode is a protocol error and a client that maps
            // 404 to FileNotFoundError would report a phantom absent artifact.
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("mode {raw} is not a valid octal file mode"),
                )
                    .into_response();
            }
        },
    };

    // Checked before the parent directory is created and before the file is
    // opened, so a refused write leaves nothing behind — not even an empty file
    // where the caller's data was supposed to go. An empty file at the target path
    // is worse than no file: it reads as a successful zero-byte transfer.
    if let Some(reading) = guard.preflight(&path) {
        return insufficient_storage(&path, reading);
    }

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && let Err(err) = tokio::fs::create_dir_all(parent).await
    {
        tracing::warn!(path = %path.display(), %err, "cannot create parent directory");
        return (StatusCode::INTERNAL_SERVER_ERROR, "cannot create parent").into_response();
    }

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    if let Some(mode) = mode {
        // Set at open, so the file never exists with a wider mode than asked for
        // even briefly. A separate chmod after writing leaves that window open,
        // which matters when the file is a credential.
        options.mode(mode);
    }

    let mut file = match options.open(&path).await {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "cannot open file for write");
            return (StatusCode::INTERNAL_SERVER_ERROR, "cannot write file").into_response();
        }
    };

    let stream = request
        .into_body()
        .into_data_stream()
        .map_err(io::Error::other);
    let mut reader = StreamReader::new(stream);

    // Guarded rather than a plain copy, because pre-flight alone cannot hold the
    // line: `max_body_bytes` defaults to 512 MiB, so one accepted request can be
    // twice the default reserve and fill the disk on its own.
    if let Err((written, err)) = disk::copy_guarded(&mut reader, &mut file, &guard, &path).await {
        match err {
            CopyError::Pressure(reading) => {
                // The partial file is removed. A truncated file left at the
                // caller's path is the worst of the options: it looks like a
                // complete artifact to anything that reads it later, and the
                // caller has already been told the write failed. Deleting it also
                // returns the bytes, which is the point of refusing. A failure to
                // remove is logged and not escalated — the 507 is still the honest
                // answer to the request.
                if let Err(err) = tokio::fs::remove_file(&path).await {
                    tracing::warn!(
                        path = %path.display(),
                        %err,
                        "cannot remove the partial file after a disk-pressure refusal",
                    );
                }
                tracing::warn!(
                    path = %path.display(),
                    written,
                    "aborted a write mid-stream: the filesystem crossed the disk reserve",
                );
                return insufficient_storage(&path, reading);
            }
            CopyError::Io(err) => {
                tracing::warn!(path = %path.display(), %err, written, "write failed mid-stream");
                return (StatusCode::INTERNAL_SERVER_ERROR, "cannot write file").into_response();
            }
        }
    }

    // An existing file opened with an explicit mode keeps its old permissions,
    // since `OpenOptionsExt::mode` only applies at creation. Reconciling here is
    // what makes the mode mean the same thing on a fresh write and an overwrite.
    if let Some(mode) = mode
        && let Err(err) =
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).await
    {
        tracing::warn!(path = %path.display(), %err, "cannot apply requested mode");
        return (StatusCode::INTERNAL_SERVER_ERROR, "cannot set mode").into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

/// What a pre-flight walk found. Both numbers are collected before anything is
/// allocated, which is the point.
#[derive(Debug)]
struct Estimate {
    members: u64,
    bytes: u64,
}

/// Walks `root` to size the archive we are about to build.
///
/// This runs *before* any buffer or spool file exists. The predecessor built the
/// archive in memory and then measured `buffer.tell()` from inside the gzip `with`
/// block, where the stream is still unflushed: it reported 10 bytes for a 327-byte
/// archive, so the size guard was decorative and the real memory cost was never
/// checked at all.
///
/// Symlinks are not followed. tar stores the link, not its target, so a symlink to
/// a 4 GiB file contributes a header and nothing else — following it would count
/// bytes the archive will never carry and trigger a spurious 413. Not following
/// also means a symlink loop cannot make this walk run forever.
fn estimate_tree(root: &Path, caps: Caps) -> Result<Estimate, Refusal> {
    let mut stack = vec![root.to_path_buf()];
    let mut members = 0u64;
    let mut bytes = 0u64;

    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            // `symlink_metadata`, so a symlink is measured as itself.
            let meta = entry.metadata()?;

            members += 1;
            if members > caps.max_members {
                return Err(Refusal::TooLarge(format!(
                    "tree under {} has more than {} members",
                    root.display(),
                    caps.max_members
                )));
            }

            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                bytes = bytes.saturating_add(meta.len());
                if bytes > caps.max_bytes {
                    return Err(Refusal::TooLarge(format!(
                        "tree under {} exceeds {} bytes",
                        root.display(),
                        caps.max_bytes
                    )));
                }
            }
        }
    }

    Ok(Estimate { members, bytes })
}

/// Packs a tree into an already-open spool file.
fn pack_tree(root: &Path, spool: std::fs::File) -> Result<std::fs::File, Refusal> {
    let mut builder = tar::Builder::new(spool);
    // Matching how Harbor packs: links are preserved as links, which is the
    // producing half of the contract that `write_tar` implements on the consuming
    // half. Following them here would silently change what a round trip means.
    builder.follow_symlinks(false);
    builder.append_dir_all(".", root)?;

    let mut spool = builder.into_inner()?;
    spool.flush()?;
    // Rewound outside any writer, so the position is real. The predecessor's bug
    // was exactly a measurement taken while a wrapper still held buffered bytes.
    spool.seek(SeekFrom::Start(0))?;
    Ok(spool)
}

/// Streams a tar of the tree at `?path=`.
pub async fn read_tar(State(state): State<AppState>, request: Request) -> Response {
    let Ok(query) = axum::extract::Query::<FsQuery>::try_from_uri(request.uri()) else {
        return (StatusCode::BAD_REQUEST, "path query parameter is required").into_response();
    };
    let root = PathBuf::from(&query.path);
    let caps = Caps {
        max_members: state.config().max_tar_members,
        max_bytes: state.config().max_tar_bytes,
    };

    match tokio::fs::metadata(&root).await {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "path is not a directory; use /v1/fs/file",
            )
                .into_response();
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return (StatusCode::NOT_FOUND, "no such directory").into_response();
        }
        Err(err) => {
            tracing::warn!(path = %root.display(), %err, "cannot stat tar root");
            return (StatusCode::INTERNAL_SERVER_ERROR, "cannot read directory").into_response();
        }
    }

    let packed = tokio::task::spawn_blocking(move || {
        let estimate = estimate_tree(&root, caps)?;
        tracing::info!(
            path = %root.display(),
            members = estimate.members,
            bytes = estimate.bytes,
            "packing tree",
        );
        pack_tree(&root, tempfile::tempfile()?)
    })
    .await;

    let spool = match packed {
        Ok(Ok(spool)) => spool,
        Ok(Err(refusal)) => return refusal.into_response(),
        Err(err) => {
            tracing::error!(%err, "pack task panicked");
            return (StatusCode::INTERNAL_SERVER_ERROR, "cannot build archive").into_response();
        }
    };

    let body = Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(spool)));
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/x-tar")],
        body,
    )
        .into_response()
}

/// Extracts an uploaded tar under `?path=`, confined to that root.
///
/// This is the one write path in the module that *is* confined, because the member
/// paths come from the archive rather than from the caller. See the module comment.
pub async fn write_tar(State(state): State<AppState>, request: Request) -> Response {
    let Ok(query) = axum::extract::Query::<FsQuery>::try_from_uri(request.uri()) else {
        return (StatusCode::BAD_REQUEST, "path query parameter is required").into_response();
    };
    let root = PathBuf::from(&query.path);
    if !root.is_absolute() {
        // A relative root would be resolved against the daemon's own working
        // directory, which is the image WORKDIR and not something the caller can
        // see. Refusing is clearer than extracting somewhere surprising.
        return (
            StatusCode::BAD_REQUEST,
            "path must be absolute for extraction",
        )
            .into_response();
    }

    let caps = Caps {
        max_members: state.config().max_tar_members,
        max_bytes: state.config().max_tar_bytes,
    };
    let guard = state.disk_guard();

    // Checked against the extraction root before the body is spooled, so an upload
    // aimed at a full filesystem is refused without first spending the disk and the
    // wire time to receive it.
    if let Some(reading) = guard.preflight(&root) {
        return insufficient_storage(&root, reading);
    }

    let spool = match spool_body(request.into_body(), &guard).await {
        Ok(spool) => spool,
        // The spool is what fills up first on this path: the archive lands there in
        // full before a single member is extracted, so the spool's own filesystem is
        // the one that runs out. It is usually /tmp and need not be the same
        // filesystem as `root`, which is why both are checked.
        Err(SpoolError::Pressure(reading)) => return insufficient_storage(&root, reading),
        Err(SpoolError::Io(err)) => {
            // A body that dies on the wire, including the 413 the body-limit layer
            // injects, arrives here as an io error. 400 rather than 500: nothing on
            // this side failed.
            tracing::warn!(%err, "upload body failed while spooling");
            return (StatusCode::BAD_REQUEST, "malformed or truncated body").into_response();
        }
    };

    let extracted = tokio::task::spawn_blocking(move || {
        extract_into(&root, spool, caps, &guard).map(|n| (root, n))
    })
    .await;

    match extracted {
        Ok(Ok((root, members))) => {
            tracing::info!(path = %root.display(), members, "archive extracted");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Err(refusal)) => refusal.into_response(),
        Err(err) => {
            tracing::error!(%err, "extract task panicked");
            (StatusCode::INTERNAL_SERVER_ERROR, "extraction failed").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tar::{Builder, Header};
    use tempfile::TempDir;

    /// Generous caps, so a test that is not about caps cannot trip one.
    const OPEN: Caps = Caps {
        max_members: 1_000,
        max_bytes: 1 << 20,
    };

    /// Archives are built in memory here rather than spooled: these are a few
    /// hundred bytes each, and building them by hand is what lets a test express a
    /// member no packer would ever produce.
    struct Archive(Builder<Vec<u8>>);

    impl Archive {
        fn new() -> Self {
            Self(Builder::new(Vec::new()))
        }

        fn file(mut self, path: &str, body: &[u8]) -> Self {
            let mut header = Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            self.0.append_data(&mut header, path, body).expect("append");
            self
        }

        fn dir(mut self, path: &str, mode: u32) -> Self {
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Directory);
            header.set_size(0);
            header.set_mode(mode);
            header.set_cksum();
            self.0
                .append_data(&mut header, path, io::empty())
                .expect("append dir");
            self
        }

        /// Writes the name field byte-for-byte, bypassing `Header::set_path`.
        ///
        /// `set_path` refuses `..` and a leading `/` — reasonably, since it is the
        /// packing side — but the traversal tests need to build precisely the
        /// archive that a hostile or buggy packer would produce. This is the only
        /// way to get those bytes into a header.
        fn set_raw_path(header: &mut Header, path: &str) {
            let name = &mut header.as_old_mut().name;
            let bytes = path.as_bytes();
            assert!(
                bytes.len() < name.len(),
                "raw test path must fit in 100 bytes"
            );
            name.fill(0);
            name[..bytes.len()].copy_from_slice(bytes);
        }

        fn raw_file(mut self, path: &str, body: &[u8]) -> Self {
            let mut header = Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            Self::set_raw_path(&mut header, path);
            header.set_cksum();
            self.0.append(&header, body).expect("append raw file");
            self
        }

        fn link(mut self, kind: EntryType, path: &str, target: &str) -> Self {
            let mut header = Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(0);
            header.set_mode(0o777);
            // Both fields are written literally: `set_link_name` refuses an
            // absolute target and `set_path` refuses `..`, and these tests exist to
            // exercise exactly those inputs.
            header.set_link_name_literal(target).expect("link name");
            Self::set_raw_path(&mut header, path);
            header.set_cksum();
            self.0.append(&header, io::empty()).expect("append link");
            self
        }

        fn special(mut self, kind: EntryType, path: &str) -> Self {
            let mut header = Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(0);
            header.set_mode(0o644);
            header.set_cksum();
            self.0
                .append_data(&mut header, path, io::empty())
                .expect("append special");
            self
        }

        fn bytes(self) -> Vec<u8> {
            self.0.into_inner().expect("finish archive")
        }
    }

    /// A probe reporting plenty of space, so no test about extraction semantics can
    /// be perturbed by the host's real free disk. The disk guard has its own tests
    /// in `disk` and one end-to-end test below.
    fn roomy_probe(_path: &Path) -> io::Result<u64> {
        Ok(u64::MAX)
    }

    fn roomy_guard() -> disk::Guard {
        disk::Guard {
            probe: roomy_probe,
            reserve: 1 << 20,
        }
    }

    fn extract(archive: Vec<u8>, caps: Caps) -> (TempDir, Result<u64, Refusal>) {
        extract_with(archive, caps, roomy_guard())
    }

    fn extract_with(
        archive: Vec<u8>,
        caps: Caps,
        guard: disk::Guard,
    ) -> (TempDir, Result<u64, Refusal>) {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("root");
        let result = extract_into(&root, archive.as_slice(), caps, &guard);
        (dir, result)
    }

    fn refusal_reason(result: Result<u64, Refusal>) -> String {
        match result {
            Ok(_) => panic!("expected a refusal, extraction succeeded"),
            Err(Refusal::Member { member, reason }) => format!("{member}: {reason}"),
            Err(other) => panic!("expected a member refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_in_tree_symlink_survives_a_round_trip() {
        // The case the earlier link-rejecting version broke. Harbor packs symlinks
        // deliberately, so this passing is the compatibility requirement.
        let archive = Archive::new()
            .file("target.txt", b"payload")
            .dir("d", 0o755)
            .link(EntryType::Symlink, "d/rel", "../target.txt")
            .link(EntryType::Symlink, "top", "target.txt")
            .bytes();

        let (dir, result) = extract(archive, OPEN);
        assert_eq!(result.expect("extraction accepted"), 4);

        let root = dir.path().join("root");
        for name in ["d/rel", "top"] {
            let link = root.join(name);
            let meta = std::fs::symlink_metadata(&link).expect("link exists");
            assert!(meta.file_type().is_symlink(), "{name} is a symlink");
        }
        // Target preserved verbatim, not rewritten to something absolute.
        assert_eq!(
            std::fs::read_link(root.join("d/rel")).expect("readlink"),
            Path::new("../target.txt"),
        );
        // And it resolves, because `d/rel` means `d/../target.txt`.
        assert_eq!(
            std::fs::read(root.join("d/rel")).expect("read through link"),
            b"payload",
        );
    }

    #[test]
    fn a_symlink_resolves_from_its_own_directory_and_a_hard_link_from_the_root() {
        // Verified against CPython 3.14: `d/s -> target.txt` means `d/target.txt`
        // and extracts dangling, while `d/h -> target.txt` as a hard link finds the
        // file at the archive root. Two different bases, one archive.
        let archive = Archive::new()
            .file("target.txt", b"payload")
            .dir("d", 0o755)
            .link(EntryType::Symlink, "d/s", "target.txt")
            .link(EntryType::Link, "d/h", "target.txt")
            .bytes();

        let (dir, result) = extract(archive, OPEN);
        result.expect("both link forms accepted");
        let root = dir.path().join("root");

        assert!(
            std::fs::symlink_metadata(root.join("d/s"))
                .expect("symlink created")
                .file_type()
                .is_symlink(),
        );
        // Dangling, because the symlink base is `d/`, so it points at
        // `d/target.txt`, which the archive never contained.
        assert!(
            !root.join("d/s").exists(),
            "symlink resolves from its own directory, so it dangles",
        );
        // The hard link resolved against the root and found the real file.
        assert_eq!(
            std::fs::read(root.join("d/h")).expect("hard link content"),
            b"payload",
        );
    }

    #[test]
    fn a_symlink_pointing_out_of_its_own_directory_is_still_bounded_by_the_root() {
        // `d/s -> ../target.txt` is fine (lands at the root), but one more `..`
        // escapes. The boundary is exactly where CPython puts it.
        let inside = Archive::new()
            .file("target.txt", b"x")
            .dir("d", 0o755)
            .link(EntryType::Symlink, "d/s", "../target.txt")
            .bytes();
        extract(inside, OPEN).1.expect("one level up stays inside");

        let outside = Archive::new()
            .dir("d", 0o755)
            .link(EntryType::Symlink, "d/s", "../../etc/passwd")
            .bytes();
        let reason = refusal_reason(extract(outside, OPEN).1);
        assert!(reason.contains("d/s"), "names the member: {reason}");
        assert!(reason.contains("outside the root"), "{reason}");
    }

    #[test]
    fn an_absolute_link_target_is_refused_for_both_link_kinds() {
        for kind in [EntryType::Symlink, EntryType::Link] {
            let archive = Archive::new().link(kind, "escape", "/etc/passwd").bytes();
            let reason = refusal_reason(extract(archive, OPEN).1);
            assert!(reason.contains("escape"), "names the member: {reason}");
            assert!(
                reason.contains("absolute link target"),
                "{kind:?}: {reason}",
            );
        }
    }

    #[test]
    fn a_member_path_escaping_with_dot_dot_is_refused() {
        let archive = Archive::new().raw_file("../escape.txt", b"nope").bytes();
        let reason = refusal_reason(extract(archive, OPEN).1);
        assert!(reason.contains("escapes the extraction root"), "{reason}");

        // A `..` that is absorbed within the tree is fine: `a/../b` is just `b`.
        let benign = Archive::new().raw_file("a/../b.txt", b"ok").bytes();
        let (dir, result) = extract(benign, OPEN);
        result.expect("an absorbed .. is not an escape");
        assert_eq!(
            std::fs::read(dir.path().join("root/b.txt")).expect("landed at b.txt"),
            b"ok",
        );
    }

    #[test]
    fn a_member_naming_the_root_itself_is_refused() {
        // A link at the destination would replace the directory and redirect every
        // later member, so "." is not a writable member name.
        let archive = Archive::new()
            .link(EntryType::Symlink, ".", "somewhere")
            .bytes();
        let reason = refusal_reason(extract(archive, OPEN).1);
        assert!(
            reason.contains("only a directory member may name"),
            "{reason}",
        );

        // A *directory* named `.` is accepted, because that is the top-level member
        // `append_dir_all` and GNU tar both emit, and refusing it would break the
        // download-then-upload round trip.
        let round_trippable = Archive::new().dir(".", 0o755).file("a.txt", b"ok").bytes();
        let (dir, result) = extract(round_trippable, OPEN);
        result.expect("a `.` directory member is a no-op, not a refusal");
        assert_eq!(
            std::fs::read(dir.path().join("root/a.txt")).expect("sibling landed"),
            b"ok",
        );
    }

    #[test]
    fn a_symlink_cannot_redirect_a_later_member() {
        // The test that distinguishes normpath from realpath. Member 1 creates
        // `hop` as a symlink to the extraction root's parent — in-tree by our
        // lexical rule only if it stays in-tree, so it is refused outright. The
        // second archive uses an in-tree symlink and then tries to write *through*
        // it, which is the attack a realpath check would have to catch on member 2.
        let escaping_hop = Archive::new().link(EntryType::Symlink, "hop", "..").bytes();
        let reason = refusal_reason(extract(escaping_hop, OPEN).1);
        assert!(reason.contains("outside the root"), "{reason}");

        // Now the subtle version: `inner` is a legitimate in-tree symlink pointing
        // at a real subdirectory, and a later member goes through it with a `..`
        // that, followed on the live filesystem, would land outside the root.
        // Lexically, `inner/../../oops` normalizes to `../oops` and is refused; a
        // realpath check would instead resolve `inner` to `d/sub`, making
        // `inner/../..` the root and letting the write through.
        let redirect = Archive::new()
            .dir("d", 0o755)
            .dir("d/sub", 0o755)
            .link(EntryType::Symlink, "inner", "d/sub")
            .raw_file("inner/../../oops.txt", b"escaped")
            .bytes();
        let reason = refusal_reason(extract(redirect, OPEN).1);
        assert!(
            reason.contains("escapes the extraction root"),
            "the later member is judged lexically, not through the symlink: {reason}",
        );

        // Proving the guard can fail: the same shape with one fewer `..` is
        // in-tree, and lands where the *lexical* rule says it does — beside `d`,
        // not inside `d/sub`.
        let benign = Archive::new()
            .dir("d", 0o755)
            .dir("d/sub", 0o755)
            .link(EntryType::Symlink, "inner", "d/sub")
            .raw_file("inner/../fine.txt", b"ok")
            .bytes();
        let (dir, result) = extract(benign, OPEN);
        result.expect("an in-tree path through a symlink name is allowed");
        assert_eq!(
            std::fs::read(dir.path().join("root/fine.txt")).expect("lexical landing site"),
            b"ok",
            "landed at root/fine.txt, not root/d/sub/../fine.txt",
        );
    }

    #[test]
    fn device_and_fifo_members_are_refused() {
        for kind in [EntryType::Char, EntryType::Block, EntryType::Fifo] {
            let archive = Archive::new().special(kind, "node").bytes();
            let reason = refusal_reason(extract(archive, OPEN).1);
            assert!(reason.contains("node"), "names the member: {reason}");
            assert!(
                reason.contains("device and fifo members"),
                "{kind:?}: {reason}",
            );
        }
    }

    #[test]
    fn the_member_count_cap_refuses_with_413() {
        let archive = Archive::new()
            .file("a", b"1")
            .file("b", b"2")
            .file("c", b"3")
            .bytes();

        let caps = Caps {
            max_members: 2,
            max_bytes: 1 << 20,
        };
        match extract(archive.clone(), caps).1 {
            Err(Refusal::TooLarge(detail)) => {
                assert!(detail.contains("more than 2 members"), "{detail}");
            }
            other => panic!("expected a cap refusal, got {other:?}"),
        }

        // The guard is not vacuous: at the cap the same archive is accepted.
        let caps = Caps {
            max_members: 3,
            max_bytes: 1 << 20,
        };
        assert_eq!(extract(archive, caps).1.expect("at the cap"), 3);
    }

    #[test]
    fn the_total_size_cap_refuses_with_413() {
        let archive = Archive::new()
            .file("a", &[b'a'; 40])
            .file("b", &[b'b'; 40])
            .bytes();

        // Sized so one member fits and two do not, proving the cap is on the
        // running total rather than on any single member.
        let caps = Caps {
            max_members: 100,
            max_bytes: 60,
        };
        match extract(archive.clone(), caps).1 {
            Err(Refusal::TooLarge(detail)) => {
                assert!(detail.contains("exceeds 60 uncompressed bytes"), "{detail}");
            }
            other => panic!("expected a cap refusal, got {other:?}"),
        }

        let caps = Caps {
            max_members: 100,
            max_bytes: 80,
        };
        extract(archive, caps).1.expect("exactly at the cap");
    }

    #[test]
    fn a_restrictive_directory_mode_does_not_block_its_own_children() {
        // The deferred-mode defect. A 0o500 directory applied at creation time
        // makes the write of `locked/inside.txt` fail with EACCES partway through
        // an extraction that tar itself handles fine.
        let archive = Archive::new()
            .dir("locked", 0o500)
            .file("locked/inside.txt", b"content")
            .dir("locked/deeper", 0o500)
            .file("locked/deeper/nested.txt", b"more")
            .bytes();

        let (dir, result) = extract(archive, OPEN);
        result.expect("children land before modes are applied");
        let root = dir.path().join("root");

        assert_eq!(
            std::fs::read(root.join("locked/inside.txt")).expect("child written"),
            b"content",
        );
        assert_eq!(
            std::fs::read(root.join("locked/deeper/nested.txt")).expect("nested child written"),
            b"more",
        );
        // And the mode really was applied afterwards, so the deferral is not just
        // silently dropping it.
        for name in ["locked", "locked/deeper"] {
            let mode = std::fs::metadata(root.join(name))
                .expect("dir exists")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o500, "{name} carries the archive mode");
        }
    }

    #[test]
    fn archive_modes_are_masked_like_the_data_filter() {
        // No setuid out of an upload, and no group- or other-writable bits.
        let mut header = Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o4777);
        header.set_cksum();
        let mut builder = Builder::new(Vec::new());
        builder
            .append_data(&mut header, "hazard", io::empty())
            .expect("append");
        let archive = builder.into_inner().expect("finish");

        let (dir, result) = extract(archive, OPEN);
        result.expect("accepted, with the mode narrowed");
        let mode = std::fs::metadata(dir.path().join("root/hazard"))
            .expect("file exists")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o755, "setuid and go-w stripped");
    }

    #[test]
    fn normalize_rejects_escapes_and_absorbs_benign_traversal() {
        assert_eq!(normalize(Path::new("a/b")).expect("plain").len(), 2);
        assert_eq!(normalize(Path::new("./a/./b")).expect("curdir").len(), 2);
        assert_eq!(normalize(Path::new("a/../b")).expect("absorbed").len(), 1);
        assert!(normalize(Path::new("/abs")).is_none(), "rooted");
        assert!(normalize(Path::new("../up")).is_none(), "escapes");
        assert!(normalize(Path::new("a/../../up")).is_none(), "escapes late");
    }

    #[test]
    fn link_target_bounds_depend_on_the_base_depth() {
        // From `d/` (depth 1), one `..` reaches the root and two escape.
        assert!(link_target_is_in_tree(1, Path::new("../sibling")));
        assert!(!link_target_is_in_tree(1, Path::new("../../outside")));
        // From the root (depth 0, the hard-link base), any `..` escapes.
        assert!(!link_target_is_in_tree(0, Path::new("../outside")));
        assert!(link_target_is_in_tree(0, Path::new("member")));
        // Absolute is refused regardless of base.
        assert!(!link_target_is_in_tree(9, Path::new("/etc/passwd")));
    }

    #[test]
    fn modes_parse_as_octal_and_a_bad_mode_is_rejected() {
        // Decimal 644 would be 0o1204; reading a mode as decimal was a real defect.
        assert_eq!(parse_mode("644"), Some(0o644));
        assert_eq!(parse_mode("0644"), Some(0o644));
        assert_eq!(parse_mode("0o644"), Some(0o644));
        assert_eq!(parse_mode("0"), Some(0));
        assert_eq!(parse_mode("000"), Some(0));
        assert_eq!(parse_mode("4755"), Some(0o4755));

        assert_eq!(parse_mode("899"), None, "8 and 9 are not octal digits");
        assert_eq!(parse_mode(""), None);
        assert_eq!(parse_mode("rwxr-xr-x"), None);
        assert_eq!(
            parse_mode("100644"),
            None,
            "a whole st_mode including file-type bits is a caller error",
        );
    }

    #[test]
    fn the_pre_flight_walk_measures_honestly_and_does_not_follow_symlinks() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("tree");
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("sub/big.bin"), vec![0u8; 4096]).expect("write");
        std::fs::write(root.join("small.txt"), b"hi").expect("write");
        std::os::unix::fs::symlink("sub/big.bin", root.join("link")).expect("symlink");

        let estimate = estimate_tree(&root, OPEN).expect("walk");
        // sub/, sub/big.bin, small.txt, link.
        assert_eq!(estimate.members, 4);
        // The symlink contributes a header and no content. Following it would add
        // another 4096 and could produce a spurious 413 on a tree that packs small.
        assert_eq!(estimate.bytes, 4096 + 2);

        // And the walk enforces the byte cap it measures against.
        let caps = Caps {
            max_members: 100,
            max_bytes: 1024,
        };
        match estimate_tree(&root, caps) {
            Err(Refusal::TooLarge(detail)) => assert!(detail.contains("exceeds 1024"), "{detail}"),
            other => panic!("expected a cap refusal, got {other:?}"),
        }
    }

    #[test]
    fn packing_reports_a_flushed_size_and_round_trips_through_extraction() {
        // The predecessor measured inside an unflushed gzip block and reported 10
        // bytes for a 327-byte archive. Here the spool is measured after the
        // builder has been consumed, so the number is the real one.
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("tree");
        std::fs::create_dir_all(source.join("sub")).expect("mkdir");
        std::fs::write(source.join("sub/a.txt"), b"alpha").expect("write");
        std::os::unix::fs::symlink("a.txt", source.join("sub/link")).expect("symlink");

        let mut spool = pack_tree(&source, tempfile::tempfile().expect("spool")).expect("pack");
        let size = spool.metadata().expect("stat").len();
        assert!(
            size >= 1024,
            "a real archive is at least a few blocks: {size}"
        );

        let mut packed = Vec::new();
        spool.read_to_end(&mut packed).expect("read spool");
        assert_eq!(packed.len() as u64, size, "the rewind exposed every byte");

        // The producing and consuming halves agree: a packed symlink comes back as
        // a symlink rather than being refused or dereferenced.
        let (out, result) = extract(packed, OPEN);
        result.expect("our own archive extracts");
        let root = out.path().join("root");
        assert_eq!(
            std::fs::read(root.join("sub/a.txt")).expect("file round-tripped"),
            b"alpha",
        );
        assert!(
            std::fs::symlink_metadata(root.join("sub/link"))
                .expect("link round-tripped")
                .file_type()
                .is_symlink(),
        );
    }

    #[test]
    fn refusals_map_to_the_status_codes_the_protocol_promises() {
        // A refused member is 400 and never 404, because a client mapping 404 onto
        // FileNotFoundError would report a phantom absent artifact.
        let member = Refusal::member(Path::new("bad/member"), "because").into_response();
        assert_eq!(member.status(), StatusCode::BAD_REQUEST);

        let too_large = Refusal::TooLarge("over".into()).into_response();
        assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let io = Refusal::Io(io::Error::other("disk")).into_response();
        assert_eq!(io.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // 507, not 413. The archive is not too big for the protocol, it is too big
        // for this filesystem right now — and a caller retries the first and not the
        // second.
        let pressure = Refusal::Pressure(
            PathBuf::from("/data"),
            disk::Reading {
                available: 10,
                reserve: 100,
            },
        )
        .into_response();
        assert_eq!(pressure.status(), StatusCode::INSUFFICIENT_STORAGE);
    }

    /// A probe that reports a fixed value the test chooses, so the verdict never
    /// depends on the host's real free space.
    fn full_probe(_path: &Path) -> io::Result<u64> {
        Ok(1)
    }

    #[tokio::test]
    async fn a_write_to_a_full_filesystem_is_refused_before_the_file_is_created() {
        // The incident shape: proceeding here is what produces an ENOSPC that
        // surfaces as an indistinguishable 500 after a partial file already exists.
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("nested/payload.bin");

        let state = AppState::with_probe(
            Config {
                disk_reserve_bytes: 1 << 30,
                ..Config::default()
            },
            full_probe,
            crate::identity::Report::skipped(),
        );

        let request = axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/v1/fs/file?path={}", target.display()))
            .body(Body::from("payload"))
            .expect("request");

        let response = write_file(State(state), request).await;
        assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);

        // Nothing was left behind. An empty or partial file at the target path is
        // worse than none: it reads as a successful transfer to whatever comes next.
        assert!(!target.exists(), "a refused write creates no file");
        assert!(
            !dir.path().join("nested").exists(),
            "and it does not create the parent directory either",
        );
    }

    #[tokio::test]
    async fn the_refusal_body_names_the_actual_free_space() {
        // "It failed" sends the caller reading their own code. "3 bytes available,
        // below the 500 byte reserve" tells them the disk is full, which is the whole
        // reason to report a number instead of a status alone.
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("f.bin");

        fn three_bytes_free(_path: &Path) -> io::Result<u64> {
            Ok(3)
        }

        let state = AppState::with_probe(
            Config {
                disk_reserve_bytes: 500,
                ..Config::default()
            },
            three_bytes_free,
            crate::identity::Report::skipped(),
        );
        let request = axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/v1/fs/file?path={}", target.display()))
            .body(Body::from("x"))
            .expect("request");

        let response = write_file(State(state), request).await;
        assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);

        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains('3'), "names the free space: {text}");
        assert!(text.contains("500"), "names the reserve: {text}");
        assert!(text.contains("f.bin"), "names the path: {text}");
    }

    #[tokio::test]
    async fn a_write_with_room_is_not_refused() {
        // The guard is not vacuous: with space available the same request succeeds
        // and every byte lands.
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("ok.bin");

        let state = AppState::with_probe(
            Config::default(),
            roomy_probe,
            crate::identity::Report::skipped(),
        );
        let request = axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/v1/fs/file?path={}", target.display()))
            .body(Body::from("payload"))
            .expect("request");

        let response = write_file(State(state), request).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(std::fs::read(&target).expect("written"), b"payload");
    }

    #[test]
    fn extraction_stops_when_the_root_filesystem_crosses_the_reserve() {
        // Members are copied one at a time, so `copy_guarded` cannot span them; this
        // covers the `Pacer` path. `max_tar_bytes` defaults to 8 GiB, far more than
        // the reserve, so the size cap alone does not imply the disk survives.
        //
        // The probe interval is 8 MiB, so the archive has to exceed it for a check to
        // fire at all — which is itself the property being pinned.
        let member = vec![b'x'; 3 * 1024 * 1024];
        let archive = Archive::new()
            .file("a", &member)
            .file("b", &member)
            .file("c", &member)
            .file("d", &member)
            .bytes();

        let caps = Caps {
            max_members: 100,
            max_bytes: 64 * 1024 * 1024,
        };
        let guard = disk::Guard {
            probe: full_probe,
            reserve: 1 << 30,
        };

        match extract_with(archive.clone(), caps, guard).1 {
            Err(Refusal::Pressure(_, reading)) => assert_eq!(reading.available, 1),
            other => panic!("expected a pressure refusal, got {other:?}"),
        }

        // And with room the same archive extracts fully, so the guard is not simply
        // rejecting every archive over 8 MiB.
        let (dir, result) = extract_with(archive, caps, roomy_guard());
        assert_eq!(result.expect("room means no refusal"), 4);
        assert_eq!(
            std::fs::metadata(dir.path().join("root/d"))
                .expect("last member landed")
                .len(),
            member.len() as u64,
        );
    }
}

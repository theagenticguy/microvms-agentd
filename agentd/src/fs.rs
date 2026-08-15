// SPDX-License-Identifier: Apache-2.0
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
//!
//! # Two layers, and why the lexical one is not enough on its own
//!
//! The rules above are the first layer. They read the member's name as text and
//! refuse a hostile member before any syscall runs, which is what makes the 400
//! bodies specific enough to debug.
//!
//! They are not sufficient by themselves. The lexical layer judges a member at the
//! depth its *name* implies, while the write goes wherever the *filesystem* says.
//! Those two can disagree once the archive has created a symlink of its own. Issue
//! #15 is the case: member `V/a/..` normalizes to `V` at depth 1 and its target `.`
//! is in tree, so `<root>/V` becomes a symlink pointing at the root. Member
//! `V/a/a/..` then normalizes to `V/a` at depth 2, and its target `../W/escape` is
//! judged from depth 1, where it stays in tree. The write of `<root>/V/a` follows
//! `<root>/V` and lands at `<root>/a` instead, one level shallower, and from there
//! the same target reaches outside the root.
//!
//! So there is a second layer underneath, and it is the kernel's. The extraction
//! opens the root once and creates every member relative to that descriptor with
//! `openat2` and `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`.
//! A path component that is a symlink stops resolution with `ELOOP`, and a
//! resolution that would leave the root stops with `EXDEV`. Both become a 400 that
//! names the member. The daemon no longer has to reason about whether a redirect is
//! safe, because a redirected write cannot happen.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{AT_FDCWD, AtFlags, OFlag, OpenHow, ResolveFlag, openat2};
use nix::sys::stat::{FchmodatFlags, Mode, fchmodat, mkdirat};
use nix::unistd::{UnlinkatFlags, linkat, symlinkat, unlinkat};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::TryStreamExt;
use tar::EntryType;
use tokio_util::io::{ReaderStream, StreamReader};

use crate::disk::{self, CopyError};
use crate::state::AppState;

/// The two typed shapes in this module, re-exported from their original paths. The
/// bodies here are opaque byte streams, so the query is the whole wire contract.
pub use protocol::fs::{FileReadQuery, FsQuery};

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
    /// A path under the root, with the normalized components that lead to it.
    ///
    /// `dest` is the joined path, and it is used for logging and for keying the
    /// deferred-mode table. The write itself never uses it. The write walks
    /// `parts` one component at a time through [`Confined`], so the number of
    /// components the kernel resolves is the number the daemon judged. The count
    /// of `parts` is also the member's depth, which a symlink target needs as its
    /// resolution base.
    Under { dest: PathBuf, parts: Vec<OsString> },
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
    let mut dest = root.to_path_buf();
    dest.extend(&parts);
    Landing::Under { dest, parts }
}

/// The extraction root, held open, with every write going through it.
///
/// One descriptor for the whole extraction. Each member is created by walking its
/// components one at a time from this descriptor, and each step is an `openat2`
/// with [`Confined::RESOLVE`]. A component that turns out to be a symlink fails
/// with `ELOOP`; a resolution that would leave the root fails with `EXDEV`.
///
/// The descriptor is opened `O_PATH`, which means it can be used as the `dirfd` of
/// an `*at` call and for nothing else. That is all this type needs, and `O_PATH` is
/// the narrowest thing that provides it — a directory opened `O_PATH` cannot be
/// read from or written to even if the descriptor leaks somewhere it should not.
struct Confined {
    root: OwnedFd,
}

impl Confined {
    /// The resolve flags every component walk uses.
    ///
    /// * `RESOLVE_BENEATH` is the one that closes issue #15. It refuses any
    ///   resolution that reaches a path which is not a descendant of the
    ///   descriptor, which covers a `..` that climbs out and an absolute path.
    /// * `RESOLVE_NO_SYMLINKS` refuses a symlink *anywhere* in the path, including
    ///   the final component. `RESOLVE_BENEATH` alone would follow a symlink and
    ///   only complain if the result left the root, so `<root>/V -> .` would still
    ///   redirect `<root>/V/a` to `<root>/a` — in tree, and at the wrong depth,
    ///   which is exactly the bug. Refusing traversal outright is what makes the
    ///   path the kernel resolves the same path the lexical layer judged.
    /// * `RESOLVE_NO_MAGICLINKS` is implied by `RESOLVE_NO_SYMLINKS` and named
    ///   anyway. Magic links are the `/proc/[pid]/fd/*` style entries that are not
    ///   symlinks and jump somewhere else when opened. Naming the flag means the
    ///   guarantee does not quietly depend on the implication holding.
    ///
    /// `RESOLVE_IN_ROOT` was considered and rejected. It *rewrites* an escaping
    /// path to land at the root rather than failing, so a member that tried to
    /// escape would be silently written somewhere the archive never named. This
    /// module refuses instead, because a rewrite teaches the caller nothing.
    ///
    /// `RESOLVE_NO_XDEV` was also considered and rejected. It refuses to cross a
    /// mount point, and an extraction root with a legitimate bind mount or a
    /// separate volume underneath it is a normal thing for a harness to have.
    /// Crossing a mount does not escape the root, so refusing it would break real
    /// uploads for no confinement gain.
    const RESOLVE: ResolveFlag = ResolveFlag::RESOLVE_BENEATH
        .union(ResolveFlag::RESOLVE_NO_SYMLINKS)
        .union(ResolveFlag::RESOLVE_NO_MAGICLINKS);

    /// Opens `root`, creating it if it is absent, and holds it.
    ///
    /// The root itself is opened by absolute path with only
    /// `RESOLVE_NO_MAGICLINKS`, not with [`Self::RESOLVE`]. `RESOLVE_BENEATH`
    /// rejects an absolute pathname outright, and the caller's `?path=` is
    /// absolute by contract — `write_tar` refuses a relative one. The root is also
    /// the caller's own choice rather than something out of the archive, and the
    /// module comment explains why a caller-named path is not confined. Symlinks
    /// on the way to the root are therefore fine and are followed, the same way
    /// the previous `create_dir_all` followed them. Everything *inside* the root
    /// comes from the archive and gets the full flag set.
    ///
    /// This is also where an old kernel is discovered. `openat2` landed in Linux 5.6
    /// and answers `ENOSYS` before that, which becomes a 500 and refuses the whole
    /// extraction. There is deliberately no fall back to the plain `create_dir_all`
    /// and `File::create` this replaced: falling back would mean the confinement a
    /// caller cannot see silently became the weaker one, and issue #15 is what the
    /// weaker one allows.
    fn open(root: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let how = OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_NO_MAGICLINKS);
        let root = openat2(AT_FDCWD, root, how).map_err(io::Error::from)?;
        Ok(Self { root })
    }

    /// Opens the single component `name` under `dir` as a directory.
    ///
    /// Single component on purpose. `openat2` would happily resolve `a/b/c` in one
    /// call under the same flags, but doing it a component at a time is what lets
    /// the error name which component failed, and a 400 that names the component
    /// is the difference between an operator fixing their tree and re-reading it.
    fn open_dir(&self, dir: BorrowedFd<'_>, name: &OsStr) -> Result<OwnedFd, Errno> {
        let how = OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
            .resolve(Self::RESOLVE);
        openat2(dir, name, how)
    }

    /// Walks `parts` from the root, creating each directory that is missing.
    ///
    /// Returns the descriptor for the last component. `EEXIST` from `mkdirat` is
    /// not an error: an archive may name the same directory twice, and a partial
    /// extraction that is retried has to converge. The open that follows decides
    /// whether the existing name is usable, and a symlink there fails with
    /// `ELOOP` rather than being traversed.
    fn make_dirs(&self, parts: &[OsString]) -> Result<OwnedFd, WalkError> {
        let mut current = self.root.try_clone().map_err(|err| WalkError {
            component: None,
            errno: Errno::try_from(err).unwrap_or(Errno::EIO),
        })?;
        for (index, part) in parts.iter().enumerate() {
            // 0o777 rather than the archive's mode, because the archive's mode is
            // replayed by `apply_deferred_modes` after every member has landed.
            // Applying it here is the defect that pass exists to avoid: a 0o500
            // directory blocks the writes of its own children.
            match mkdirat(&current, part.as_os_str(), Mode::from_bits_truncate(0o777)) {
                Ok(()) | Err(Errno::EEXIST) => {}
                Err(errno) => return Err(WalkError::at(parts, index, errno)),
            }
            current = self
                .open_dir(current.as_fd(), part)
                .map_err(|errno| WalkError::at(parts, index, errno))?;
        }
        Ok(current)
    }

    /// Splits `parts` into the parent directory descriptor and the final name.
    ///
    /// Every leaf creation needs exactly this: a `dirfd` the kernel has already
    /// confirmed is inside the root and reached without traversing a symlink, plus
    /// one name to create in it. `parts` is never empty here, because
    /// [`resolve_member`] returns [`Landing::Root`] for the empty case and the
    /// member loop handles that separately.
    fn parent_of(&self, parts: &[OsString]) -> Result<(OwnedFd, OsString), WalkError> {
        let (name, ancestors) = parts
            .split_last()
            .expect("a member has at least one component");
        let parent = self.make_dirs(ancestors)?;
        Ok((parent, name.clone()))
    }

    /// Creates the directory a `Directory` member names.
    fn create_dir(&self, parts: &[OsString]) -> Result<(), WalkError> {
        self.make_dirs(parts).map(drop)
    }

    /// Creates and opens the file a data member's bytes go into.
    ///
    /// `O_TRUNC` rather than `O_EXCL`, and the existing name is unlinked first, for
    /// the reason the previous `remove_file` plus `File::create` had: an archive may
    /// legitimately overwrite a name, and a retried partial extraction has to
    /// converge. The unlink is `unlinkat` without `AT_REMOVEDIR`, so a directory in
    /// the way survives and the open below reports `EISDIR`, which is the same
    /// answer `File::create` gave.
    fn create_file(&self, parts: &[OsString]) -> Result<std::fs::File, WalkError> {
        let (parent, name) = self.parent_of(parts)?;
        let last = parts.len() - 1;
        let how = OpenHow::new()
            .flags(OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC | OFlag::O_CLOEXEC)
            // 0o666 before umask, matching `File::create`. The archive's own mode
            // is replayed later by `apply_deferred_modes`.
            .mode(Mode::from_bits_truncate(0o666))
            .resolve(Self::RESOLVE);
        let fd = openat2(parent.as_fd(), name.as_os_str(), how)
            .map_err(|errno| WalkError::at(parts, last, errno))?;
        Ok(std::fs::File::from(fd))
    }

    /// Creates a symlink at `parts` with `target` written verbatim.
    ///
    /// The target is never resolved here and never checked against the filesystem.
    /// tar stores the link rather than what it points at, and a tree packed in
    /// dependency-free order can carry a symlink whose target arrives later or
    /// never. The lexical layer has already decided the target stays in tree.
    fn create_symlink(&self, parts: &[OsString], target: &Path) -> Result<(), WalkError> {
        let (parent, name) = self.parent_of(parts)?;
        let last = parts.len() - 1;
        self.replace(parent.as_fd(), &name);
        symlinkat(target, parent.as_fd(), name.as_os_str())
            .map_err(|errno| WalkError::at(parts, last, errno))
    }

    /// Creates a hard link at `parts` pointing at `source`, an archive-root-relative
    /// member name.
    ///
    /// Both ends go through the confined walk. `source` is resolved from the root
    /// because a hard link's target in a tar archive is a member name rather than a
    /// path relative to the link, which is the base difference the module comment
    /// describes. `AtFlags::empty()` rather than `AT_SYMLINK_FOLLOW`, so linking to
    /// a symlink member links the symlink rather than its target — the same thing
    /// `std::fs::hard_link` did.
    fn create_hard_link(
        &self,
        parts: &[OsString],
        source: &[OsString],
    ) -> Result<(), HardLinkError> {
        let (parent, name) = self.parent_of(parts).map_err(HardLinkError::Dest)?;
        let last = parts.len() - 1;
        self.replace(parent.as_fd(), &name);

        // An empty source is the archive naming the root as a hard-link target.
        // `linkat` with an empty path is EINVAL rather than "link the directory",
        // and that is the honest answer: a hard link to a directory is not
        // creatable on Linux at all.
        let Some((source_name, source_dirs)) = source.split_last() else {
            return Err(HardLinkError::Dest(WalkError {
                component: None,
                errno: Errno::EINVAL,
            }));
        };
        let source_parent = self
            .walk_existing(source_dirs)
            .map_err(HardLinkError::Source)?;

        linkat(
            source_parent.as_fd(),
            source_name.as_os_str(),
            parent.as_fd(),
            name.as_os_str(),
            AtFlags::empty(),
        )
        .map_err(|errno| HardLinkError::Dest(WalkError::at(parts, last, errno)))
    }

    /// Walks `parts` from the root without creating anything.
    ///
    /// Used for the source side of a hard link, where the member must already exist
    /// — `link(2)` needs an existing file, and creating the directories on the way
    /// there would turn "the archive names a member it never included" into an
    /// empty directory tree plus the same failure one call later.
    fn walk_existing(&self, parts: &[OsString]) -> Result<OwnedFd, WalkError> {
        let mut current = self.root.try_clone().map_err(|err| WalkError {
            component: None,
            errno: Errno::try_from(err).unwrap_or(Errno::EIO),
        })?;
        for (index, part) in parts.iter().enumerate() {
            current = self
                .open_dir(current.as_fd(), part)
                .map_err(|errno| WalkError::at(parts, index, errno))?;
        }
        Ok(current)
    }

    /// Unlinks `name` under `dir`, ignoring every failure.
    ///
    /// A link member replaces an existing name rather than failing on it, and
    /// `symlinkat`/`linkat` have no truncating equivalent of `O_TRUNC`, so the old
    /// name has to go first. Every error is ignored on purpose: the common one is
    /// `ENOENT`, which means there was nothing to replace, and any other means the
    /// create below fails and reports the real reason.
    fn replace(&self, dir: BorrowedFd<'_>, name: &OsStr) {
        let _ = unlinkat(dir, name, UnlinkatFlags::NoRemoveDir);
    }

    /// Applies `mode` to `parts`, without following a symlink at the last component.
    ///
    /// `FchmodatFlags::NoFollowSymlink` so a symlink member's own mode is what gets
    /// chmodded rather than whatever it points at. Linux does not support changing a
    /// symlink's mode and answers `EOPNOTSUPP`, which is fine — the alternative is
    /// following the link and changing the permissions of a file the archive did not
    /// name. [`apply_deferred_modes`] logs the failure and moves on.
    fn set_mode(&self, parts: &[OsString], mode: u32) -> Result<(), WalkError> {
        let (parent, name) = self.parent_of(parts)?;
        let last = parts.len() - 1;
        fchmodat(
            parent.as_fd(),
            name.as_os_str(),
            Mode::from_bits_truncate(mode),
            FchmodatFlags::NoFollowSymlink,
        )
        .map_err(|errno| WalkError::at(parts, last, errno))
    }
}

/// A confined walk that stopped, and where.
#[derive(Debug)]
struct WalkError {
    /// The component the kernel refused, when one is identifiable. `None` covers
    /// the failures that are not about a component, such as a descriptor that
    /// could not be duplicated.
    component: Option<OsString>,
    errno: Errno,
}

impl WalkError {
    fn at(parts: &[OsString], index: usize, errno: Errno) -> Self {
        Self {
            component: parts.get(index).cloned(),
            errno,
        }
    }

    /// Turns the stopped walk into the refusal the caller sees, prefixed with
    /// `context` when there is something to say about which end failed.
    ///
    /// Three errnos become a 400 that names the member, because each one is a thing
    /// the caller's archive did and the caller is the only one who can fix it:
    ///
    /// * `ELOOP` is a symlink in the path. Under [`Confined::RESOLVE`] the kernel
    ///   reports it for *any* symlink component, so this is the issue #15 case: the
    ///   archive created a symlink and then tried to write through it.
    /// * `EXDEV` is a resolution that would have left the root.
    /// * `EACCES` is directory permissions that forbid the write.
    ///
    /// Every other errno stays a 500. `ENOSPC`, `EIO` and `ENOENT` are not the
    /// caller's archive being wrong, and answering 400 for them would tell the
    /// caller to fix an archive that is fine. This split also keeps the outcomes the
    /// property tier already documents: a hard link to a member the archive never
    /// included is `ENOENT` and stays the 500 it has always been.
    fn into_refusal(self, member: &Path, context: &str) -> Refusal {
        let cause = match self.errno {
            Errno::ELOOP => "a symbolic link on the way there, which extraction refuses to follow",
            Errno::EXDEV => "a path that resolves outside the extraction root",
            Errno::EACCES => "directory permissions that refuse the write",
            _ => return Refusal::Io(io::Error::from(self.errno)),
        };
        let reason = match &self.component {
            Some(component) => format!(
                "{context}cannot create under {}: {cause}",
                Path::new(component).display(),
            ),
            None => format!("{context}cannot create: {cause}"),
        };
        Refusal::member(member, reason)
    }
}

/// Which end of a hard link failed. The two ends need different sentences, because
/// a bad destination is the member's own name and a bad source is the link target.
#[derive(Debug)]
enum HardLinkError {
    Dest(WalkError),
    Source(WalkError),
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
    // The kernel's half of the confinement, held for the whole extraction. Every
    // member below is created through this rather than by absolute path, so a
    // component that turns out to be a symlink stops the write instead of
    // redirecting it. See the module comment.
    let confined = Confined::open(root)?;

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
    // Keyed by the joined path so the ordering below can count components, and
    // carrying the components too, because the chmod goes through the confined walk
    // and needs them. The path is the key rather than the components because two
    // members can normalize to the same place and the later mode should win, which
    // is what a map keyed on the destination gives.
    let mut deferred: HashMap<PathBuf, (Vec<OsString>, u32)> = HashMap::new();
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

        let (dest, parts) = match resolve_member(root, &path) {
            Landing::Under { dest, parts } => (dest, parts),
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
                confined
                    .create_dir(&parts)
                    .map_err(|err| err.into_refusal(&path, ""))?;
                if let Ok(mode) = entry.header().mode() {
                    deferred.insert(dest, (parts, mode));
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
                // one less than the member's own depth. A hard link's target is an
                // archive-relative member name, so its base is the root, depth 0.
                let base_depth = if kind == EntryType::Symlink {
                    parts.len() - 1
                } else {
                    0
                };
                if !link_target_is_in_tree(base_depth, &target) {
                    return Err(Refusal::member(
                        &path,
                        format!("link target {} resolves outside the root", target.display()),
                    ));
                }

                if kind == EntryType::Symlink {
                    // The target is written verbatim, dangling or not. tar stores
                    // the link, not what it points at, and a tree packed in
                    // dependency-free order can carry a symlink whose target
                    // arrives in a later member — or never, which is still what
                    // the source tree looked like.
                    confined
                        .create_symlink(&parts, &target)
                        .map_err(|err| err.into_refusal(&path, ""))?;
                } else {
                    let source = normalize(&target).unwrap_or_default();
                    confined
                        .create_hard_link(&parts, &source)
                        .map_err(|err| match err {
                            HardLinkError::Dest(err) => err.into_refusal(&path, ""),
                            HardLinkError::Source(err) => err.into_refusal(
                                &path,
                                &format!("hard link target {}: ", target.display()),
                            ),
                        })?;
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

                let mut file = confined
                    .create_file(&parts)
                    .map_err(|err| err.into_refusal(&path, ""))?;
                // Streamed through `Entry`'s `Read` impl. `Entry::unpack` would
                // apply no confinement at all, and `unpack_in` applies a
                // link-target policy that differs from ours — it rejects targets
                // this contract must preserve.
                let written = io::copy(&mut entry, &mut file)?;

                if let Ok(mode) = entry.header().mode() {
                    deferred.insert(dest, (parts, mode));
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

    apply_deferred_modes(&confined, deferred);
    Ok(members)
}

/// Replays recorded modes once every member has landed.
///
/// Deepest paths first, so a directory made read-only does not block the chmod of
/// something beneath it. Modes are masked to `0o755` and a failure is logged
/// rather than fatal: the bytes are already correct, and the caller would rather
/// have a tree with a stale mode than a 500 and no tree.
///
/// The chmod goes through the same confined walk the writes did. It is not the
/// security-relevant step — the member already landed inside the root — but reusing
/// one path means there is no second way to reach a member, and a symlink appearing
/// mid-extraction cannot redirect a chmod either.
fn apply_deferred_modes(confined: &Confined, deferred: HashMap<PathBuf, (Vec<OsString>, u32)>) {
    let mut entries: Vec<(PathBuf, Vec<OsString>, u32)> = deferred
        .into_iter()
        .map(|(path, (parts, mode))| (path, parts, mode))
        .collect();
    entries.sort_by_key(|(_, parts, _)| std::cmp::Reverse(parts.len()));

    for (path, parts, mode) in entries {
        // Masked like CPython's `data` filter: no setuid/setgid/sticky out of an
        // upload, and no group- or other-writable bits.
        let masked = mode & 0o755;
        if let Err(err) = confined.set_mode(&parts, masked) {
            tracing::warn!(
                path = %path.display(),
                errno = ?err.errno,
                "could not apply archive mode",
            );
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
    // Only the `0o`/`0O` prefix needs stripping: `from_str_radix` already accepts
    // leading zeros in base 8, so `0644` and `000` parse without special-casing.
    let digits = raw.trim().trim_start_matches("0o").trim_start_matches("0O");
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

/// Extracts the query every fs route takes, or the 400 all of them answer when
/// `path` is missing.
///
/// A helper rather than a `Query<FsQuery>` extractor in the handler signatures:
/// the extractor's default rejection body ("Failed to deserialize query string:
/// missing field `path`") is not the body this surface has always answered, so
/// keeping the wire contract through the extractor route would mean a custom
/// rejection type — more machinery than the three lines it would replace. This
/// also leaves the handler signatures alone, which the proptest harness calls
/// directly. The `Err` is the un-built refusal rather than a `Response` so the
/// variant stays small (clippy's `result_large_err`).
fn fs_query(request: &Request) -> Result<FsQuery, (StatusCode, &'static str)> {
    match axum::extract::Query::<FsQuery>::try_from_uri(request.uri()) {
        Ok(query) => Ok(query.0),
        Err(_) => Err((StatusCode::BAD_REQUEST, "path query parameter is required")),
    }
}

/// Extracts `GET /v1/fs/file`'s query, which is [`fs_query`]'s plus a line range.
///
/// Two attempts rather than one, and the second is the point. A request whose
/// `start_line` is not a number and one that omits `path` entirely both fail the
/// first parse, and they are different mistakes: the missing-path body is the exact
/// string this surface has answered since the first commit and a test pins it, so
/// the fallback parse is what decides which of the two happened rather than
/// flattening both into whichever message reads better.
fn file_read_query(request: &Request) -> Result<FileReadQuery, (StatusCode, &'static str)> {
    match axum::extract::Query::<FileReadQuery>::try_from_uri(request.uri()) {
        Ok(query) => Ok(query.0),
        // `FsQuery` ignores the range keys, so it succeeds exactly when `path` is
        // present and well-formed — which makes it the discriminator.
        Err(_) => match axum::extract::Query::<FsQuery>::try_from_uri(request.uri()) {
            Ok(_) => Err((
                StatusCode::BAD_REQUEST,
                "start_line and end_line must be integers",
            )),
            Err(_) => Err((StatusCode::BAD_REQUEST, "path query parameter is required")),
        },
    }
}

/// Turns the requested range into a slicer, or names why it cannot be one.
///
/// `Ok(None)` means no range was asked for, and that case must stay distinguishable
/// from a range that happens to cover the whole file: with no range the response is
/// the un-sliced stream it has always been, byte for byte.
///
/// Both refusals are 400. Neither is 416 `Range Not Satisfiable`, which is the
/// tempting answer and the wrong one: 416 is about a range the *file* cannot
/// satisfy, and both of these are ranges no file could — a 1-based line 0 does not
/// exist and an end before a start is not a window. A caller who saw 416 would go
/// looking at the file.
fn line_range(query: &FileReadQuery) -> Result<Option<LineSlicer>, String> {
    if query.start_line.is_none() && query.end_line.is_none() {
        return Ok(None);
    }

    // Defaulted rather than required, matching the harness contract: `startLine`
    // absent is 1, `endLine` absent is through EOF.
    let start = query.start_line.unwrap_or(1);
    if start == 0 {
        return Err(
            "start_line is 1-based, so 0 is not a line. Line 1 is the first line; a caller \
             working from 0-based offsets wants start_line=1."
                .to_string(),
        );
    }
    if let Some(end) = query.end_line
        && end < start
    {
        return Err(format!(
            "end_line {end} is before start_line {start}. Both bounds are 1-based and \
             inclusive, so end_line must be at least start_line. An end_line past the last \
             line is fine and reads through EOF — this refusal is for an inverted range, \
             which no file can satisfy."
        ));
    }

    Ok(Some(LineSlicer::new(start, query.end_line)))
}

/// Keeps a byte-oriented read inside a line-oriented window, one chunk at a time.
///
/// Written as a chunk filter rather than as "read the file, split on newlines, join
/// the slice" because the second one buffers a whole file to hand back four lines of
/// it, on a VM whose baseline memory can be 512 MiB. This holds one chunk and a line
/// counter, and once the window closes it reports [`LineSlicer::finished`] so the
/// caller stops reading — lines 1..5 of a 10 GB file cost the first chunk.
///
/// **A line owns its terminating newline.** Lines 1..3 of `a\nb\nc\nd\n` are
/// `a\nb\nc\n`, so consecutive ranges concatenate back into the file rather than
/// losing a separator at every seam. The last line of a file with no trailing
/// newline simply has none to own.
struct LineSlicer {
    /// 1-based, inclusive.
    start: u64,
    /// 1-based, inclusive. `None` reads through EOF, which is also what an
    /// `end_line` past the last line does — the two are the same traversal.
    end: Option<u64>,
    /// The line the next unconsumed byte belongs to. Carried across chunks, because
    /// a chunk boundary is not a line boundary.
    line: u64,
    finished: bool,
}

impl LineSlicer {
    fn new(start: u64, end: Option<u64>) -> Self {
        Self {
            start,
            end,
            line: 1,
            finished: false,
        }
    }

    /// The part of `chunk` inside the window.
    ///
    /// The in-range bytes of any one chunk are contiguous — the window is a run of
    /// whole lines — so this is a single slice rather than a rebuilt buffer.
    fn take(&mut self, chunk: &[u8]) -> Bytes {
        if self.finished || chunk.is_empty() {
            return Bytes::new();
        }
        let mut from: Option<usize> = None;
        let mut to = 0usize;
        let mut pos = 0usize;

        while pos < chunk.len() {
            let inside = self.line >= self.start && self.end.is_none_or(|end| self.line <= end);
            // Where this line ends inside this chunk: just past its newline, or the
            // end of the chunk when the line continues into the next one.
            let newline = chunk[pos..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|offset| pos + offset);
            let segment_end = newline.map_or(chunk.len(), |at| at + 1);

            if inside {
                if from.is_none() {
                    from = Some(pos);
                }
                to = segment_end;
            }
            pos = segment_end;

            if newline.is_some() {
                self.line += 1;
                if let Some(end) = self.end
                    && self.line > end
                {
                    // The window closed inside this chunk, so the rest of the file
                    // is never read.
                    self.finished = true;
                    break;
                }
            }
        }

        match from {
            Some(from) => Bytes::copy_from_slice(&chunk[from..to]),
            None => Bytes::new(),
        }
    }

    /// Whether the window has closed, so the caller can stop reading.
    fn finished(&self) -> bool {
        self.finished
    }
}

/// Reads one file, optionally a line range of it. 404 only when the path is
/// genuinely absent.
///
/// With no `start_line` or `end_line` the response is exactly what it has always
/// been: the file's bytes, streamed. With a range it is still streamed — see
/// [`LineSlicer`] — and the range is 1-based and inclusive on both ends, with an
/// `end_line` past the last line reading through EOF rather than answering an error.
/// Those are the AI SDK harness's `readTextFile` semantics, and they are copied
/// rather than chosen: this route is what that method is implemented on top of.
pub async fn read_file(request: Request) -> Response {
    let query = match file_read_query(&request) {
        Ok(query) => query,
        Err(refusal) => return refusal.into_response(),
    };
    // Validated before the file is opened, so a bad range costs no syscall and
    // cannot be reported as anything about the file.
    let slicer = match line_range(&query) {
        Ok(slicer) => slicer,
        Err(detail) => return (StatusCode::BAD_REQUEST, detail).into_response(),
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
    // The un-ranged arm hands back the reader stream untouched: a range feature that
    // rewrapped every read would put its own bug surface on the path taken by every
    // caller who never asked for one.
    let body = match slicer {
        None => Body::from_stream(ReaderStream::new(file)),
        Some(slicer) => Body::from_stream(sliced_stream(ReaderStream::new(file), slicer)),
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/octet-stream")],
        body,
    )
        .into_response()
}

/// Filters a byte stream down to the slicer's line window, still a stream.
///
/// `try_unfold` rather than a `map` over the source, because closing the window has
/// to *stop reading* — a `map` would filter every remaining chunk of a large file to
/// nothing while still paying to read it. Ending the stream here drops the reader,
/// which is what makes lines 1..5 of a huge file cost the first chunk.
///
/// Empty chunks are dropped rather than yielded: a chunk entirely outside the window
/// is not a zero-byte read, and some HTTP bodies treat one as end-of-stream.
fn sliced_stream(
    source: ReaderStream<tokio::fs::File>,
    slicer: LineSlicer,
) -> impl futures_util::Stream<Item = io::Result<Bytes>> {
    futures_util::stream::try_unfold((source, slicer), |(mut source, mut slicer)| async move {
        use futures_util::StreamExt as _;
        loop {
            if slicer.finished() {
                return Ok(None);
            }
            let Some(chunk) = source.next().await else {
                return Ok(None);
            };
            let taken = slicer.take(&chunk?);
            if !taken.is_empty() {
                return Ok(Some((taken, (source, slicer))));
            }
        }
    })
}

/// Writes one file. Not confined to a root; see the module comment.
pub async fn write_file(State(state): State<AppState>, request: Request) -> Response {
    let query = match fs_query(&request) {
        Ok(query) => query,
        Err(refusal) => return refusal.into_response(),
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
    let query = match fs_query(&request) {
        Ok(query) => query,
        Err(refusal) => return refusal.into_response(),
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
    let query = match fs_query(&request) {
        Ok(query) => query,
        Err(refusal) => return refusal.into_response(),
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

        /// Writes one GNU extension member, which is how a name or a link target
        /// longer than the 100-byte header field travels in a tar archive.
        ///
        /// `kind` is `L` for a long name and `K` for a long link target. The value
        /// is the member's body, NUL-terminated, with the terminator counted in
        /// `size`, and the header's own name field carries GNU's marker string.
        /// `entry.path()` and `entry.link_name()` read these and override the
        /// inline fields, which is why `fs.rs` uses those accessors rather than the
        /// `entry.header()` ones.
        fn append_extension(&mut self, kind: u8, value: &str) {
            const LONG_LINK: &[u8] = b"././@LongLink";
            let mut header = Header::new_gnu();
            header
                .as_gnu_mut()
                .expect("new_gnu yields a gnu header")
                .name[..LONG_LINK.len()]
                .copy_from_slice(LONG_LINK);
            header.set_mode(0o644);
            header.set_entry_type(EntryType::new(kind));
            header.set_size(value.len() as u64 + 1);
            header.set_cksum();

            let mut body = value.as_bytes().to_vec();
            body.push(0);
            self.0
                .append(&header, body.as_slice())
                .expect("append extension member");
        }

        /// Appends a link whose name or target is longer than the 100-byte header
        /// field, using the GNU extension members above.
        ///
        /// [`Self::link`] cannot express this: `set_raw_path` writes into a 100-byte
        /// field and `set_link_name_literal` refuses a value that does not fit. The
        /// reproducer in issue #15 has a 104-byte path component and a 114-byte
        /// target, so both extensions are needed. The lengths are incidental to the
        /// escape, which the short-name test below shows, but this is the shape the
        /// property tier actually found, so this is the shape the guard reproduces.
        fn long_link(mut self, kind: EntryType, path: &str, target: &str) -> Self {
            if path.len() >= 100 {
                self.append_extension(b'L', path);
            }
            if target.len() >= 100 {
                self.append_extension(b'K', target);
            }

            // Both inline fields still carry a clipped prefix, the way GNU tar
            // writes them; the extension members above override what is read.
            let mut header = Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(0);
            header.set_mode(0o755);
            header
                .set_link_name_literal(&target[..target.len().min(99)])
                .expect("clipped link target fits the field");
            Self::set_raw_path(&mut header, &path[..path.len().min(99)]);
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
    fn a_symlink_the_archive_created_cannot_redirect_a_later_member_to_a_shallower_depth() {
        // Issue #15, as the property tier shrank it. Both members pass every lexical
        // check, and before the kernel layer existed the pair still escaped.
        //
        // Member 1 is named `V/a/..`, which normalizes to `V` at depth 1. Its target
        // is `.`, which stays in tree, so the daemon wrote `<root>/V` as a symlink
        // pointing at the root itself.
        //
        // Member 2 is named `V/a/a/..`, which normalizes to `V/a` at depth 2. A
        // symlink resolves from its own directory, so its target `../W/escape` is
        // judged from depth 1, where it reaches the root and stops. In tree.
        //
        // The escape is in the write rather than the judgement. `<root>/V` is a
        // symlink now, so writing `<root>/V/a` follows it and lands at `<root>/a`, a
        // level shallower than the name says. From there the same `../W/escape`
        // target reaches `<root>/../W/escape`, outside the root.
        //
        // Observed against the pre-fix code: extraction answered 204 and left
        // `<root>/a` as a symlink to `../W/escape`.
        //
        // The kernel layer refuses member 2 with ELOOP, because `V` is a symlink and
        // resolution under RESOLVE_NO_SYMLINKS will not traverse one.
        let v = "v".repeat(104);
        let w = "w".repeat(104);
        let hop = format!("{v}/a/..");
        let victim = format!("{v}/a/a/..");
        let archive = Archive::new()
            .long_link(EntryType::Symlink, &hop, ".")
            .long_link(EntryType::Symlink, &victim, &format!("../{w}/escape"))
            .bytes();

        // The extraction root is nested, so a member that escaped by one level lands
        // inside the TempDir where the assertions below can see it. An escape into
        // /tmp would leave nothing to find and would read as a pass.
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("nested/deeper/root");
        let result = extract_into(&root, archive.as_slice(), OPEN, &roomy_guard());

        // 400 naming the member, not a 500. The property tier asserts that every
        // refusal names a member the archive contained, and the member's name here is
        // the long one the GNU extension carried.
        let reason = refusal_reason(result);
        assert!(reason.contains(&victim), "names the member: {reason}");
        assert!(
            reason.contains("symbolic link"),
            "says the write would have followed a symlink: {reason}",
        );

        // Member 1 is legitimate on its own terms and is still created, because the
        // fix refuses the redirected *write* rather than retroactively refusing the
        // symlink that would have redirected it.
        assert!(
            std::fs::symlink_metadata(root.join(&v))
                .expect("the hop symlink was created")
                .file_type()
                .is_symlink(),
        );

        // Nothing landed outside the root. `<root>/a` is where the escape used to
        // put its symlink, and `<root>/../W/escape` is where that symlink pointed.
        assert!(
            !root.join("a").symlink_metadata().is_ok(),
            "the shallower path the symlink would have redirected to is empty",
        );
        for stray in [
            dir.path().join("nested/deeper").join(&w),
            dir.path().join("nested").join(&w),
            dir.path().join(&w),
        ] {
            assert!(
                stray.symlink_metadata().is_err(),
                "nothing outside the root: {}",
                stray.display(),
            );
        }
    }

    #[test]
    fn a_write_through_a_symlink_the_archive_created_is_refused_at_any_name_length() {
        // The same defect with names short enough for the inline header field, so the
        // guard does not silently depend on the GNU long-name path. `hop/a/..`
        // normalizes to `hop`, a symlink to `.`; `hop/a` is then a write through it.
        //
        // A file member rather than a symlink one, so the refusal cannot be coming
        // from any link-target rule. Before the fix this file landed at `<root>/a`,
        // one level shallower than `hop/a` names.
        let archive = Archive::new()
            .link(EntryType::Symlink, "hop/a/..", ".")
            .raw_file("hop/a", b"redirected")
            .bytes();

        let (dir, result) = extract(archive, OPEN);
        let reason = refusal_reason(result);
        assert!(reason.contains("hop/a"), "names the member: {reason}");
        assert!(reason.contains("symbolic link"), "{reason}");

        let root = dir.path().join("root");
        assert!(
            !root.join("a").exists(),
            "the redirected write did not land at the shallower path",
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

    #[tokio::test]
    async fn a_request_without_a_path_answers_the_same_400_on_every_fs_route() {
        // The four routes share one query helper; this pins the wire shape it must
        // preserve — 400 with this exact body, not the extractor's default
        // "Failed to deserialize query string" rejection.
        let state = AppState::with_probe(
            Config::default(),
            roomy_probe,
            crate::identity::Report::skipped(),
        );
        let request = |method: &str, uri: &str| {
            axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .expect("request")
        };

        let responses = [
            read_file(request("GET", "/v1/fs/file")).await,
            write_file(State(state.clone()), request("PUT", "/v1/fs/file")).await,
            read_tar(State(state.clone()), request("GET", "/v1/fs/tar")).await,
            write_tar(State(state), request("PUT", "/v1/fs/tar")).await,
        ];
        for response in responses {
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .expect("body");
            assert_eq!(&body[..], b"path query parameter is required");
        }
    }

    // ── line-ranged reads ───────────────────────────────────────────────────

    /// Drives `read_file` through the real handler and returns the status and body.
    ///
    /// Through the handler rather than against [`LineSlicer`] directly, because the
    /// three things most likely to break are not in the slicer: the query parse, the
    /// validation ordering, and whether the stream terminates. A unit test on the
    /// slicer would pass against a handler that never called it.
    async fn read_range(path: &Path, query: &str) -> (StatusCode, Vec<u8>) {
        let uri = format!(
            "/v1/fs/file?path={}{}{query}",
            path.display(),
            if query.is_empty() { "" } else { "&" }
        );
        let request = axum::http::Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .expect("request");
        let response = read_file(request).await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body")
            .to_vec();
        (status, body)
    }

    /// Writes a five-line file with a trailing newline and returns its path.
    fn five_lines(dir: &TempDir) -> PathBuf {
        let path = dir.path().join("five.txt");
        std::fs::write(&path, b"one\ntwo\nthree\nfour\nfive\n").expect("write");
        path
    }

    /// The ranges, all against one fixture, so a regression in the line counter shows
    /// up as a specific window rather than as "ranges are broken".
    ///
    /// A line owns its terminating newline, which is what makes 1..2 and 3..5
    /// concatenate back into the file. Dropping the newline would look correct on a
    /// single-line read and lose a separator on every seam.
    #[tokio::test]
    async fn a_line_range_returns_exactly_the_requested_inclusive_window() {
        let dir = TempDir::new().expect("tempdir");
        let path = five_lines(&dir);

        for (query, expected) in [
            ("start_line=1&end_line=1", "one\n"),
            ("start_line=1&end_line=2", "one\ntwo\n"),
            ("start_line=3&end_line=3", "three\n"),
            ("start_line=2&end_line=4", "two\nthree\nfour\n"),
            // Both bounds inclusive, so the whole file is expressible.
            ("start_line=1&end_line=5", "one\ntwo\nthree\nfour\nfive\n"),
            // start_line alone reads through EOF.
            ("start_line=4", "four\nfive\n"),
            // end_line alone starts at line 1, per the harness contract's default.
            ("end_line=2", "one\ntwo\n"),
        ] {
            let (status, body) = read_range(&path, query).await;
            assert_eq!(status, StatusCode::OK, "{query}");
            assert_eq!(
                String::from_utf8(body).expect("utf-8 fixture"),
                expected,
                "{query}"
            );
        }

        // The seam: two adjacent ranges rebuild the file exactly.
        let (_, head) = read_range(&path, "start_line=1&end_line=2").await;
        let (_, tail) = read_range(&path, "start_line=3&end_line=5").await;
        assert_eq!(
            [head, tail].concat(),
            std::fs::read(&path).expect("fixture"),
            "adjacent ranges must concatenate back into the file",
        );
    }

    /// An `end_line` past the last line reads through EOF and answers 200.
    ///
    /// This is the harness contract's own sentence, and it is the one a range
    /// implementation is most likely to get wrong: 416 or a 400 is the reflexive
    /// answer, and both would break a caller who asked for "lines 1..1000" of a short
    /// file without first counting it.
    #[tokio::test]
    async fn an_end_line_past_eof_reads_through_eof_without_an_error() {
        let dir = TempDir::new().expect("tempdir");
        let path = five_lines(&dir);

        for query in ["start_line=1&end_line=1000", "start_line=4&end_line=99"] {
            let (status, _) = read_range(&path, query).await;
            assert_eq!(status, StatusCode::OK, "{query}");
        }
        let (_, whole) = read_range(&path, "start_line=1&end_line=1000").await;
        assert_eq!(whole, std::fs::read(&path).expect("fixture"));
        let (_, tail) = read_range(&path, "start_line=4&end_line=99").await;
        assert_eq!(tail, b"four\nfive\n");

        // And a start_line past EOF is an empty 200 rather than a 404: the file is
        // there and the window is empty, which are different facts from the file
        // being absent.
        let (status, empty) = read_range(&path, "start_line=99").await;
        assert_eq!(status, StatusCode::OK);
        assert!(empty.is_empty(), "{empty:?}");
    }

    /// A file with no trailing newline still ends where it ends. Its last line owns
    /// no newline because there is none to own, so a range covering it must not
    /// invent one.
    #[tokio::test]
    async fn a_final_line_without_a_newline_is_returned_as_it_is() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("bare.txt");
        std::fs::write(&path, b"alpha\nbeta").expect("write");

        let (_, last) = read_range(&path, "start_line=2&end_line=2").await;
        assert_eq!(last, b"beta");
        let (_, both) = read_range(&path, "start_line=1&end_line=2").await;
        assert_eq!(both, b"alpha\nbeta");
        let (_, past) = read_range(&path, "start_line=1&end_line=50").await;
        assert_eq!(past, b"alpha\nbeta");
    }

    /// The two refusals, and both are 400 rather than 416. Neither range is one a
    /// file could satisfy, so a client sent to look at the file would be looking in
    /// the wrong place.
    #[tokio::test]
    async fn a_zero_start_or_an_inverted_range_is_refused_with_400() {
        let dir = TempDir::new().expect("tempdir");
        let path = five_lines(&dir);

        let (status, body) = read_range(&path, "start_line=0").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let detail = String::from_utf8(body).expect("utf-8");
        assert!(detail.contains("1-based"), "{detail}");

        let (status, body) = read_range(&path, "start_line=4&end_line=2").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let detail = String::from_utf8(body).expect("utf-8");
        assert!(detail.contains("before start_line"), "{detail}");
        // The refusal has to say that a *past-EOF* end is fine, or a reader takes
        // this message as "ranges must be inside the file" and starts counting lines
        // before every read.
        assert!(detail.contains("through EOF"), "{detail}");

        // A non-integer bound is also 400, and it does not masquerade as the
        // missing-path refusal.
        let (status, body) = read_range(&path, "start_line=first").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(&body[..], b"start_line and end_line must be integers");
    }

    /// No range means the old path, untouched. The un-ranged read is what every
    /// existing client uses, so it is asserted byte-for-byte against the file rather
    /// than against a re-derived expectation.
    #[tokio::test]
    async fn a_read_with_no_range_is_byte_identical_to_the_file() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("binary.bin");
        // Deliberately not text: the un-ranged path must not have acquired any
        // line-awareness, and a file with no newline at all and a NUL in it is where
        // that would show.
        let bytes: Vec<u8> = (0u8..=255).chain(0u8..=255).collect();
        std::fs::write(&path, &bytes).expect("write");

        let (status, body) = read_range(&path, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, bytes);
    }

    /// The slicer stops reading once the window closes, which is what keeps a
    /// small range off a large file cheap.
    ///
    /// Asserted on the slicer's own state rather than on timing: a wall-clock
    /// assertion would be flaky, and what matters is that `finished` is set, because
    /// that is the flag [`sliced_stream`] ends the stream on.
    #[test]
    fn the_slicer_stops_reading_once_the_window_closes() {
        let mut slicer = LineSlicer::new(1, Some(2));
        assert_eq!(&slicer.take(b"one\ntwo\nthree\n")[..], b"one\ntwo\n");
        assert!(
            slicer.finished(),
            "the window closed inside the chunk, so nothing more should be read"
        );
        // And a later chunk yields nothing even if one arrives.
        assert!(slicer.take(b"four\n").is_empty());
    }

    /// A window that straddles chunk boundaries still comes out whole, including a
    /// line split across two chunks. A slicer that reset its counter per chunk would
    /// pass every single-chunk test above and fail here.
    #[test]
    fn the_slicer_carries_its_line_counter_across_chunk_boundaries() {
        let mut slicer = LineSlicer::new(2, Some(3));
        let mut out = Vec::new();
        // "one\ntwo\nthree\nfour\n" cut at three arbitrary points, one of them mid-line.
        for chunk in [&b"one\ntw"[..], &b"o\nthr"[..], &b"ee\nfour\n"[..]] {
            out.extend_from_slice(&slicer.take(chunk));
        }
        assert_eq!(out, b"two\nthree\n");
        assert!(slicer.finished());
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

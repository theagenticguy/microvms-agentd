// SPDX-License-Identifier: Apache-2.0
//! Property tier: tar extraction confinement, and hostile header handling.
//!
//! The 18 example tests in `fs.rs` each pin one rule of the CPython `data`-filter
//! contract to one archive a human thought of. That is the wrong shape for a
//! traversal guard. Traversal is not a list of known-bad strings; it is an input
//! space, and the interesting members are the ones nobody thought to write down —
//! a `..` at depth three, an empty path component, a symlink created in member 1
//! that member 4 tries to write through, a link target that pops out of the tree
//! and back in. This tier generates that space instead of enumerating it.
//!
//! Four invariants carry the tar half, and they are deliberately stated as
//! outcomes rather than as behavior:
//!
//! 1. **Nothing lands outside the root.** Not "the right members are refused" —
//!    which would only restate the implementation — but "walk the disk afterwards
//!    and there is nothing there." An oracle that cannot be satisfied by copying
//!    the code under test.
//! 2. **Never a panic.** This daemon is the container `CMD` inside a Firecracker
//!    VM with no supervisor. A panicked extraction is not a failed request, it is
//!    a dead VM the platform will keep routing nothing to.
//! 3. **A refusal names the member.** An operator debugging a rejected
//!    `upload_dir` has the 400 body and nothing else; "bad archive" sends them
//!    re-reading their whole tree.
//! 4. **Caps decide at their boundary.** Not merely "a cap exists" — the expected
//!    verdict is computed from the generated member count and byte total, so an
//!    off-by-one in either direction fails.
//!
//! Two of the properties below exist to keep the other two honest. A daemon that
//! refuses its entire input space satisfies 1, 2 and 3 perfectly and breaks every
//! real upload, so one property *demands acceptance* of benign trees with in-tree
//! symlinks. And a confinement walk alone cannot see a `..` that gets silently
//! rewritten to land inside the root, so the redirect property asserts the
//! verdict as well as the landing site — that gap was found by breaking `fs.rs`
//! and watching this file still pass.
//!
//! The header group covers a different defect class entirely: `hmac.compare_digest`
//! raised `TypeError` on non-ASCII `str` input and killed the Python predecessor's
//! handler thread. Any caller controls that header, so the input space is "all
//! bytes a header value may carry" and the property is that none of them are
//! special.
//!
//! # Why this drives `fs::write_tar` rather than the extraction function
//!
//! `extract_into` is private, and it stays private. Going through the public
//! handler costs one `Body` and a current-thread runtime per case, and buys the
//! two things the private function cannot show: that the caps really are read from
//! `Config` rather than hardcoded, and that a refusal survives the trip through
//! `Refusal::into_response` into a status and a body an operator will actually
//! read. The refusal *text* is the contract here, so testing anything upstream of
//! the text would test the wrong artifact.

use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use agentd::auth::{bearer_bytes, constant_time_eq};
use agentd::{AppState, Config, fs};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use http_body_util::BodyExt;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tar::{Builder, EntryType, Header};
use tempfile::TempDir;

/// The `name` and `linkname` fields of a v7/ustar header. A value this long or
/// longer cannot be expressed inline and needs a GNU extension member.
const NAME_FIELD: usize = 100;

/// Marker name GNU tar puts on a long-name/long-link extension member.
const LONG_LINK: &[u8] = b"././@LongLink";

/// How many `..` components a generated path may carry.
///
/// Bounded on purpose, and it is the bound that makes property 1 *observable*.
/// The extraction root is nested [`ROOT_NESTING`] levels below the `TempDir`, so a
/// traversal that the guard failed to stop still lands somewhere inside the
/// `TempDir` where the walk can see it. Unbounded hops would let a real escape
/// land in `/tmp` or above, where the test would find nothing and report a pass.
const MAX_PARENT_HOPS: usize = 4;

/// Components between the `TempDir` and the extraction root. Must exceed
/// [`MAX_PARENT_HOPS`] with room to spare; see that constant.
const ROOT_NESTING: &str = "n/e/s/t/e/d/deeper/root";

/// Names reused across every member kind.
///
/// One shared pool rather than a pool per kind, so names collide across members
/// and an archive can overwrite itself or link to a name a sibling member owns.
/// It does *not* reliably produce the symlink-then-write-through-it ordering —
/// measured at ~3 of 256 cases — which is why that attack has its own property
/// with the shape built rather than sampled.
const NAMES: &[&str] = &[
    "a",
    "b",
    "d",
    "sub",
    "hop",
    "link",
    "loop_a",
    "loop_b",
    "target.txt",
    "sibling.txt",
];

// ---------------------------------------------------------------------------
// Archive construction
// ---------------------------------------------------------------------------

/// What a generated member is.
#[derive(Clone, Debug)]
enum Kind {
    File(Vec<u8>),
    Dir,
    Symlink(String),
    HardLink(String),
    /// A symlink whose target is its own final component — the shortest possible
    /// loop, and the one a naive "does the target exist" check turns into a hang.
    SelfSymlink,
    /// A raw type byte, carrying the device/fifo/contiguous types. Held as a byte
    /// rather than an `EntryType` so the generator can reach types the enum does
    /// not name.
    Special(u8),
}

impl Kind {
    fn entry_type(&self) -> EntryType {
        match self {
            Kind::File(_) => EntryType::Regular,
            Kind::Dir => EntryType::Directory,
            Kind::Symlink(_) | Kind::SelfSymlink => EntryType::Symlink,
            Kind::HardLink(_) => EntryType::Link,
            Kind::Special(byte) => EntryType::new(*byte),
        }
    }

    fn body(&self) -> &[u8] {
        match self {
            Kind::File(bytes) => bytes,
            _ => &[],
        }
    }

    /// The `linkname` value, given the member's own path.
    fn target(&self, path: &str) -> Option<String> {
        match self {
            Kind::Symlink(target) | Kind::HardLink(target) => Some(target.clone()),
            Kind::SelfSymlink => Some(Path::new(path).file_name().map_or_else(
                || ".".to_owned(),
                |name| name.to_string_lossy().into_owned(),
            )),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct Member {
    path: String,
    kind: Kind,
    mode: u32,
}

/// Writes a GNU long-name (`L`) or long-link (`K`) extension member.
///
/// Reproduced by hand because `Builder::append_data` would validate the path
/// first, and validation is exactly what a hostile packer does not do. `fs.rs`
/// reads member paths through `entry.path()` specifically so these extensions are
/// honored, which means a path over 100 bytes is only really tested if the test
/// emits one.
fn append_extension(builder: &mut Builder<Vec<u8>>, kind: u8, value: &[u8]) {
    let mut header = Header::new_gnu();
    header
        .as_gnu_mut()
        .expect("new_gnu yields a gnu header")
        .name[..LONG_LINK.len()]
        .copy_from_slice(LONG_LINK);
    header.set_mode(0o644);
    header.set_entry_type(EntryType::new(kind));
    // GNU stores the value NUL-terminated and counts the terminator in `size`.
    header.set_size(value.len() as u64 + 1);
    header.set_cksum();

    let mut data = value.to_vec();
    data.push(0);
    builder
        .append(&header, data.as_slice())
        .expect("append extension member");
}

/// Writes `path` into the header's `name` field byte for byte.
///
/// `Header::set_path` refuses `..` and a leading `/`, which is right for a packer
/// and useless for a test whose entire subject is archives containing both.
fn set_raw_name(header: &mut Header, path: &[u8]) {
    let slot = &mut header.as_old_mut().name;
    let width = path.len().min(slot.len());
    slot.fill(0);
    slot[..width].copy_from_slice(&path[..width]);
}

fn append_member(builder: &mut Builder<Vec<u8>>, member: &Member) {
    let name = member.path.as_bytes();
    if name.len() >= NAME_FIELD {
        append_extension(builder, b'L', name);
    }
    let target = member.kind.target(&member.path);
    if let Some(target) = target.as_deref().filter(|t| t.len() >= NAME_FIELD) {
        append_extension(builder, b'K', target.as_bytes());
    }

    let mut header = Header::new_gnu();
    header.set_entry_type(member.kind.entry_type());
    // Directories are forced owner-traversable. Not a policy claim — a 0o000
    // directory would defeat the *test's* own walk, and then property 1 would
    // report a pass because it could not read the tree rather than because the
    // tree was clean. Deferred restrictive modes have their own unit test.
    header.set_mode(if member.kind.entry_type() == EntryType::Directory {
        member.mode | 0o500
    } else {
        member.mode
    });
    let body = member.kind.body();
    header.set_size(body.len() as u64);
    set_raw_name(&mut header, name);
    if let Some(target) = &target {
        // Clipped to fit; the `K` member above carries the full value when the
        // target is long, and overrides this field on read.
        let inline = &target.as_bytes()[..target.len().min(NAME_FIELD - 1)];
        header
            .set_link_name_literal(inline)
            .expect("clipped link target fits the field");
    }
    header.set_cksum();

    builder.append(&header, body).expect("append member");
}

fn build_archive(members: &[Member]) -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());
    for member in members {
        append_member(&mut builder, member);
    }
    builder.into_inner().expect("finish archive")
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

fn path_component() -> impl Strategy<Value = String> {
    prop_oneof![
        // A narrow pool, drawn often, so names collide across members. Collisions
        // are the feature: they are what puts an overwrite, or a symlink named `d`
        // in front of a member named `d/x`, into one archive. Measured at only ~3
        // such orderings per 256 cases though, which is why the redirect attack
        // gets its own property below rather than relying on this.
        12 => prop::sample::select(NAMES.to_vec()).prop_map(str::to_owned),
        4 => Just("..".to_owned()),
        2 => Just(".".to_owned()),
        // An empty component yields `a//b`, which several traversal guards
        // written against a component count rather than a normalizer get wrong.
        1 => Just(String::new()),
        // Over the 100-byte header field on its own, so it forces the GNU
        // long-name (`L`) / long-link (`K`) extension path. `fs.rs` reads paths
        // through `entry.path()` precisely so those are honored; a generator that
        // never crossed 100 bytes would leave that untested.
        2 => Just("v".repeat(104)),
    ]
}

prop_compose! {
    /// A member path, hostile by construction: absolute sometimes, `..` at
    /// varying depths, `.` and empty components, and long enough to need a GNU
    /// long-name member when the 48-byte component is drawn more than twice.
    fn member_path()(
        absolute in prop::bool::weighted(0.15),
        // At most MAX_PARENT_HOPS components, so at most that many can be `..`.
        // The bound is structural rather than a filter because a filter would
        // discard cases, and it is load-bearing for property 1: see
        // MAX_PARENT_HOPS.
        parts in prop::collection::vec(path_component(), 1..=MAX_PARENT_HOPS),
    ) -> String {
        let joined = parts.join("/");
        if absolute { format!("/{joined}") } else { joined }
    }
}

fn link_target() -> impl Strategy<Value = String> {
    prop_oneof![
        // Absolute: refused outright, and the one target CPython's data filter
        // also refuses unconditionally.
        3 => Just("/etc/passwd".to_owned()),
        // Escaping relative, far past any plausible root depth.
        3 => Just("../../../../../../../../etc/passwd".to_owned()),
        // In-tree sibling: must be *preserved*, not refused. Harbor packs these.
        3 => Just("sibling.txt".to_owned()),
        // Lands exactly on the root. In-tree for a symlink one level down,
        // escaping from the root itself — the boundary case.
        2 => Just("..".to_owned()),
        2 => Just("../target.txt".to_owned()),
        // Pops out and back in: in-tree, but only if resolution is lexical.
        2 => Just("d/sub/../../target.txt".to_owned()),
        // Chain and cycle, given the shared name pool.
        2 => Just("loop_a".to_owned()),
        2 => Just("loop_b".to_owned()),
        // No target at all: the header says symlink and the field is empty.
        1 => Just(String::new()),
        1 => Just(".".to_owned()),
        // Over 100 bytes, so it travels in a GNU long-link (`K`) member and the
        // inline `linkname` field holds a *truncated* prefix. An extractor reading
        // the header field instead of `entry.link_name()` would judge the truncated
        // value and let the real target through — that shape is RUSTSEC-2026-0068.
        2 => Just(format!("../{}/escape", "w".repeat(104))),
        6 => member_path(),
    ]
}

fn member_kind() -> impl Strategy<Value = Kind> {
    prop_oneof![
        8 => prop::collection::vec(any::<u8>(), 0..=48).prop_map(Kind::File),
        4 => Just(Kind::Dir),
        6 => link_target().prop_map(Kind::Symlink),
        4 => link_target().prop_map(Kind::HardLink),
        1 => Just(Kind::SelfSymlink),
        // Char, block, fifo, and contiguous. The first three must be refused;
        // contiguous carries data and must not be.
        2 => prop::sample::select(vec![b'c', b'b', b'p', b'7']).prop_map(Kind::Special),
    ]
}

prop_compose! {
    fn member()(
        path in member_path(),
        kind in member_kind(),
        mode in prop::sample::select(vec![0o755u32, 0o700, 0o555, 0o500, 0o644, 0o4777, 0o000]),
    ) -> Member {
        Member { path, kind, mode }
    }
}

/// Bytes an HTTP header value may legally carry.
///
/// Restricted to the valid set on purpose: `HeaderValue::from_bytes` rejects
/// everything else, so a control byte can never reach `bearer_bytes` and
/// generating one would only exercise `http`'s validator. The high range is the
/// interesting one — `0x80..=0xff` is *valid* in a header value, and it is what
/// killed the predecessor.
fn header_byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        6 => 0x20u8..=0x7e,
        3 => 0x80u8..=0xff,
        1 => Just(b'\t'),
    ]
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// What the handler answered.
struct Answer {
    status: StatusCode,
    body: String,
}

/// Drives one archive through the real `PUT /v1/fs/tar` handler.
fn put_tar(root: &Path, archive: Vec<u8>, max_members: u64, max_bytes: u64) -> Answer {
    let config = Config {
        max_tar_members: max_members,
        max_tar_bytes: max_bytes,
        ..Config::default()
    };
    let request = Request::builder()
        .method("PUT")
        .uri(format!("http://agentd/v1/fs/tar?path={}", root.display()))
        .body(Body::from(archive))
        .expect("build request");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async move {
        let response = fs::write_tar(State(AppState::new(config)), request).await;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("collect response body")
            .to_bytes();
        Answer {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    })
}

/// Every path under `dir`, symlinks never followed.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // `symlink_metadata`, so a symlink to a directory is not descended.
            // Following would also let a generated cycle hang the walk.
            if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                stack.push(path.clone());
            }
            found.push(path);
        }
    }
    found
}

/// Whether following `link` lexically leaves `root`.
///
/// Re-derived here rather than called from `fs.rs` — deliberately. An oracle that
/// imports the function under test agrees with any bug it contains. This walks
/// components against a depth counter based at the link's own parent directory,
/// which is where the kernel resolves a symlink from.
///
/// A target that does not exist is *not* an escape. CPython's data filter permits
/// a dangling in-tree symlink and Harbor packs trees that contain them, so
/// conflating "dangling" with "escaping" would make this property demand a
/// regression.
fn link_escapes(root: &Path, link: &Path) -> bool {
    let Ok(target) = std::fs::read_link(link) else {
        return false;
    };
    if target.is_absolute() {
        return true;
    }
    let Ok(relative) = link.strip_prefix(root) else {
        return true;
    };
    let mut depth = relative.components().count().saturating_sub(1);
    for component in target.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => return true,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(next) => depth = next,
                None => return true,
            },
            Component::Normal(_) => depth += 1,
        }
    }
    false
}

/// Widens every directory mode so `TempDir::drop` can unlink the tree.
///
/// An archive mode of 0o500 lands on disk verbatim and correctly, and then the
/// cleanup that needs write permission fails silently and litters `/tmp` once per
/// case. Runs before the assertions so a failing case cleans up too.
fn make_removable(paths: &[PathBuf]) {
    for path in paths {
        if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
        }
    }
}

/// Property 1, both halves.
fn assert_confined(tmp: &Path, root: &Path, found: &[PathBuf]) -> Result<(), TestCaseError> {
    // Anything in the TempDir that is neither under the root nor an ancestor the
    // test itself created is a path extraction had no business writing.
    let strays: Vec<&PathBuf> = found
        .iter()
        .filter(|path| !path.starts_with(root) && !root.starts_with(path))
        .collect();
    prop_assert!(
        strays.is_empty(),
        "extraction created paths outside {}: {strays:?}",
        root.display(),
    );
    prop_assert!(
        tmp.exists(),
        "the TempDir itself survived extraction: {}",
        tmp.display(),
    );

    for path in found.iter().filter(|path| path.starts_with(root)) {
        if !std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            continue;
        }
        prop_assert!(
            !link_escapes(root, path),
            "in-tree symlink resolves outside the root: {} -> {:?}",
            path.display(),
            std::fs::read_link(path),
        );
    }
    Ok(())
}

/// Properties 2 and 3: the answer is one of four shapes, and a member refusal
/// names a member the archive actually contained.
///
/// The 500 carrying `tar operation failed` is a `Refusal::Io` and is a legitimate
/// outcome — a hard link to a member the archive never included fails at
/// `link(2)`. The 500 carrying `extraction failed` is the `spawn_blocking` join
/// error, which is to say a panic, which is the thing this property forbids.
fn assert_answer_shape(answer: &Answer, paths: &[String]) -> Result<(), TestCaseError> {
    prop_assert!(
        !answer.body.contains("extraction failed"),
        "extraction panicked: {} {}",
        answer.status,
        answer.body,
    );
    match answer.status {
        StatusCode::NO_CONTENT | StatusCode::PAYLOAD_TOO_LARGE => Ok(()),
        StatusCode::INTERNAL_SERVER_ERROR => {
            prop_assert_eq!(&answer.body, "tar operation failed", "unexpected 500 body");
            Ok(())
        }
        StatusCode::BAD_REQUEST => {
            let named = answer
                .body
                .strip_prefix("refused tar member ")
                .and_then(|rest| rest.split_once(": "))
                .map(|(member, _)| member.to_owned());
            let Some(named) = named else {
                return Err(TestCaseError::fail(format!(
                    "a 400 must name the refused member, got {:?}",
                    answer.body,
                )));
            };
            prop_assert!(
                paths.contains(&named),
                "refusal named {named:?}, which is not a member of the archive: {paths:?}",
            );
            Ok(())
        }
        other => Err(TestCaseError::fail(format!(
            "unexpected status {other}: {}",
            answer.body,
        ))),
    }
}

// ---------------------------------------------------------------------------
// Tar confinement
// ---------------------------------------------------------------------------

proptest! {
    // 256 cases, each a fresh TempDir, a spooled body, an extraction and a full
    // tree walk. Tuned to stay in the low seconds: a tier nobody runs because it
    // takes a minute protects nothing.
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// The invariant the module exists for, over the whole input space rather
    /// than over the archives someone thought to write down.
    ///
    /// Caps are set wide here so a cap refusal never masks a traversal one — this
    /// property is about confinement only, and the cap property below is about
    /// caps only. A single test doing both would pass whenever either fired.
    #[test]
    fn an_arbitrary_archive_stays_inside_its_root_and_never_panics(
        members in prop::collection::vec(member(), 0..8),
    ) {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join(ROOT_NESTING);
        let paths: Vec<String> = members.iter().map(|m| m.path.clone()).collect();

        let answer = put_tar(&root, build_archive(&members), 512, 1 << 20);

        let found = walk(tmp.path());
        make_removable(&found);
        assert_answer_shape(&answer, &paths)?;
        assert_confined(tmp.path(), &root, &found)?;
    }

    /// The order-dependent attack, built rather than stumbled into.
    ///
    /// A hop symlink lands first, then a later member writes *through* its name.
    /// That is the only shape where lexical resolution and `realpath` disagree, and
    /// the free generator above produces it in roughly 3 of 256 cases — too rare to
    /// leave to chance for the case the module was designed around. So the shape is
    /// fixed and the parts that can vary — where the hop points, how deep the
    /// victim path reaches, and how many `..` it uses to climb back out — are
    /// generated.
    ///
    /// The verdict is computable from the inputs, which is what makes this the
    /// sharpest property in the file. `hop` is one component deep, so exactly one
    /// `..` reaches the root and two or more must be refused — regardless of where
    /// the hop points or which member arrives first, because the judgement is on
    /// the path text. Asserting the verdict and not only the walk is deliberate: a
    /// `..` that stops bounding at depth zero silently *rewrites* `../x` to `x`,
    /// which lands inside the root and passes a confinement walk while quietly
    /// putting a member somewhere the archive never asked for.
    #[test]
    fn a_symlink_cannot_redirect_a_member_that_arrives_after_it(
        hop_target in prop::sample::select(vec!["d/sub", "d", ".", "..", "../..", "sub"]),
        climb in 1usize..=MAX_PARENT_HOPS,
        // Names that already exist as a directory, or as the hop itself, are
        // excluded. A file member landing on one of those is EISDIR — a legitimate
        // io error, but one that would mask the policy verdict this property is
        // measuring. Overwrite behavior is covered by the general property above.
        tail in prop::sample::select(
            NAMES
                .iter()
                .filter(|n| !matches!(**n, "d" | "sub" | "hop"))
                .copied()
                .collect::<Vec<_>>(),
        ),
        hop_first in prop::bool::ANY,
    ) {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join(ROOT_NESTING);

        let victim = format!("hop/{}/{tail}", vec![".."; climb].join("/"));
        let hop = Member {
            path: "hop".to_owned(),
            kind: Kind::Symlink(hop_target.to_owned()),
            mode: 0o777,
        };
        let through = Member {
            path: victim.clone(),
            kind: Kind::File(b"escaped".to_vec()),
            mode: 0o644,
        };

        let mut members = vec![
            Member { path: "d".to_owned(), kind: Kind::Dir, mode: 0o755 },
            Member { path: "d/sub".to_owned(), kind: Kind::Dir, mode: 0o755 },
        ];
        // Both orders. Reversing it is what proves the verdict comes from the path
        // text and not from what happened to be on disk at the time.
        if hop_first {
            members.push(hop);
            members.push(through);
        } else {
            members.push(through);
            members.push(hop);
        }
        let paths: Vec<String> = members.iter().map(|m| m.path.clone()).collect();

        let answer = put_tar(&root, build_archive(&members), 512, 1 << 20);

        let found = walk(tmp.path());
        make_removable(&found);
        assert_answer_shape(&answer, &paths)?;
        assert_confined(tmp.path(), &root, &found)?;

        // The expected verdict, computed from the member text alone.
        //
        // Two members can offend. `hop` escapes when its target pops past the
        // root: it sits one component deep, so a symlink there resolves from the
        // root and any leading `..` leaves. The victim escapes when it uses more
        // than one `..`, since `hop/..` is already the root. Extraction stops at
        // the first offender in *archive* order, which is why `hop_first` is
        // generated — the same two members must produce the same two verdicts
        // whichever arrives first, because the judgement is on the text.
        let hop_escapes = hop_target.starts_with("..");
        let victim_escapes = climb > 1;
        let expected_refusal = if hop_first {
            [(hop_escapes, "hop"), (victim_escapes, victim.as_str())]
                .into_iter()
                .find_map(|(bad, name)| bad.then_some(name))
        } else {
            [(victim_escapes, victim.as_str()), (hop_escapes, "hop")]
                .into_iter()
                .find_map(|(bad, name)| bad.then_some(name))
        };

        if let Some(offender) = expected_refusal {
            prop_assert_eq!(
                answer.status,
                StatusCode::BAD_REQUEST,
                "{} / hop -> {} must be refused: {}",
                victim,
                hop_target,
                answer.body,
            );
            prop_assert!(
                answer.body.contains(offender),
                "the refusal must name {:?}: {:?}",
                offender,
                answer.body,
            );
            return Ok(());
        }

        // Neither offends: accepted, and the victim lands at the *lexical* site —
        // `root/{tail}`, never the `root/d/sub/{tail}` that following the hop
        // would give.
        prop_assert_eq!(
            answer.status,
            StatusCode::NO_CONTENT,
            "{} through hop -> {} is in-tree and must be accepted: {}",
            victim,
            hop_target,
            answer.body,
        );
        let landed = std::fs::read(root.join(tail)).ok();
        prop_assert_eq!(
            landed.as_deref(),
            Some(&b"escaped"[..]),
            "landed where lexical resolution says (root/{}), not through the hop",
            tail,
        );
    }

    /// The compatibility half, and the reason this tier cannot be satisfied by a
    /// daemon that refuses everything.
    ///
    /// An earlier version refused every link member and would have broken
    /// `upload_dir` for any tree containing one symlink. Without a property that
    /// demands acceptance, "refuse the entire input space" passes every
    /// confinement assertion above perfectly.
    #[test]
    fn an_archive_of_plain_members_and_in_tree_symlinks_is_always_accepted(
        bodies in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..=32), 1..6),
    ) {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join(ROOT_NESTING);

        let mut members = vec![Member { path: "d".to_owned(), kind: Kind::Dir, mode: 0o755 }];
        for (index, body) in bodies.iter().enumerate() {
            members.push(Member {
                path: format!("d/f{index}"),
                kind: Kind::File(body.clone()),
                mode: 0o644,
            });
            // Sibling target, one level down: in-tree by the symlink rule, and
            // the exact shape Harbor's `follow_symlinks=False` produces.
            members.push(Member {
                path: format!("d/s{index}"),
                kind: Kind::Symlink(format!("f{index}")),
                mode: 0o777,
            });
        }
        let paths: Vec<String> = members.iter().map(|m| m.path.clone()).collect();

        let answer = put_tar(&root, build_archive(&members), 512, 1 << 20);

        let found = walk(tmp.path());
        make_removable(&found);
        prop_assert_eq!(
            answer.status,
            StatusCode::NO_CONTENT,
            "a benign tree with in-tree symlinks must extract: {}",
            answer.body,
        );
        for (index, body) in bodies.iter().enumerate() {
            let file = root.join(format!("d/f{index}"));
            let landed = std::fs::read(&file).ok();
            prop_assert_eq!(landed.as_deref(), Some(body.as_slice()));
            let link = root.join(format!("d/s{index}"));
            prop_assert!(
                std::fs::symlink_metadata(&link)
                    .is_ok_and(|meta| meta.file_type().is_symlink()),
                "symlink preserved as a symlink, not dereferenced or dropped",
            );
        }
        assert_answer_shape(&answer, &paths)?;
        assert_confined(tmp.path(), &root, &found)?;
    }

    /// Caps refuse rather than extract — and, just as importantly, do not refuse
    /// an archive that fits.
    ///
    /// Members here are benign by construction with unique names, so the only
    /// reason to see anything but 204 is a cap. That makes the expected verdict
    /// computable from the inputs alone, which is what turns "a cap exists" into
    /// "the cap is at the documented boundary".
    #[test]
    fn the_member_and_byte_caps_decide_exactly_at_their_boundary(
        sizes in prop::collection::vec(prop::option::of(0usize..=64), 0..12),
        max_members in 0u64..12,
        max_bytes in 0u64..200,
    ) {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join(ROOT_NESTING);

        // `None` is a directory, `Some(n)` an n-byte file. Names are index-derived
        // so no two members collide, which would produce an io error instead of a
        // cap verdict and make the expectation below wrong.
        let members: Vec<Member> = sizes
            .iter()
            .enumerate()
            .map(|(index, size)| match size {
                Some(size) => Member {
                    path: format!("f{index}"),
                    kind: Kind::File(vec![b'x'; *size]),
                    mode: 0o644,
                },
                None => Member {
                    path: format!("d{index}"),
                    kind: Kind::Dir,
                    mode: 0o755,
                },
            })
            .collect();
        let paths: Vec<String> = members.iter().map(|m| m.path.clone()).collect();

        let count = members.len() as u64;
        let total: u64 = sizes.iter().flatten().map(|size| *size as u64).sum();
        let over_cap = count > max_members || total > max_bytes;

        let answer = put_tar(&root, build_archive(&members), max_members, max_bytes);

        let found = walk(tmp.path());
        make_removable(&found);

        if over_cap {
            prop_assert_eq!(
                answer.status,
                StatusCode::PAYLOAD_TOO_LARGE,
                "{} members / {} bytes against caps {}/{}: {}",
                count,
                total,
                max_members,
                max_bytes,
                answer.body,
            );
            // The 413 has to say which cap. Extraction streams, so some bytes may
            // already be on disk; "refused" means the caller is told, not that
            // the tree is pristine.
            prop_assert!(
                answer.body.contains("members") || answer.body.contains("uncompressed bytes"),
                "a cap refusal must name the cap: {:?}",
                answer.body,
            );
        } else {
            prop_assert_eq!(
                answer.status,
                StatusCode::NO_CONTENT,
                "{} members / {} bytes fits caps {}/{}: {}",
                count,
                total,
                max_members,
                max_bytes,
                answer.body,
            );
        }
        assert_answer_shape(&answer, &paths)?;
        assert_confined(tmp.path(), &root, &found)?;
    }
}

// ---------------------------------------------------------------------------
// Header handling
// ---------------------------------------------------------------------------

proptest! {
    // Pure byte work, no filesystem: cases are close to free, so the count is
    // high enough to actually cover the 0x80..=0xff range that mattered.
    #![proptest_config(ProptestConfig { cases: 4096, ..ProptestConfig::default() })]

    /// No `Authorization` value can panic the parser or match a token it is not.
    ///
    /// The predecessor died on `Bearer tökén` because it compared `str` values.
    /// The property is stated as reconstruction rather than as a list of bad
    /// inputs: if the parser hands back bytes equal to the installed token, the
    /// header must literally have been `bearer` + one space + that token.
    /// Anything else is a spurious match, whatever produced it.
    #[test]
    fn an_arbitrary_authorization_value_never_panics_and_never_matches_by_accident(
        raw in prop::collection::vec(header_byte(), 0..64),
        token in prop::collection::vec(header_byte(), 1..24),
    ) {
        let value = HeaderValue::from_bytes(&raw)
            .expect("every generated byte is valid in a header value");
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value);

        // Reached at all: the call itself is half the property.
        let presented = bearer_bytes(&headers);

        let state = AppState::new(Config::default());
        state.bootstrap(&token, std::collections::HashMap::new());
        let matched = presented.is_some_and(|bytes| state.token_matches(bytes) == Some(true));

        if matched {
            let expected_len = b"bearer ".len() + token.len();
            prop_assert_eq!(raw.len(), expected_len, "spurious match on {:?}", raw);
            prop_assert!(raw[..6].eq_ignore_ascii_case(b"bearer"), "scheme: {:?}", raw);
            prop_assert_eq!(raw[6], b' ');
            prop_assert_eq!(&raw[7..], token.as_slice());
        }

        // And the comparison is total: whatever came back, it is either the token
        // or it is not, with no third outcome and no crash on the way there.
        if let Some(bytes) = presented {
            prop_assert_eq!(constant_time_eq(bytes, &token), bytes == token.as_slice());
        }
    }

    /// The parser is not vacuously safe.
    ///
    /// Every assertion above is satisfied by a `bearer_bytes` that always returns
    /// `None`, and that function would reject every legitimate caller. This is the
    /// round trip that forbids it — including for tokens made entirely of the
    /// high bytes that were the original crash.
    #[test]
    fn a_well_formed_bearer_header_round_trips_whatever_bytes_the_token_holds(
        token in prop::collection::vec(header_byte(), 1..24),
        upper in prop::bool::ANY,
    ) {
        let scheme: &[u8] = if upper { b"Bearer " } else { b"bEaReR " };
        let mut raw = scheme.to_vec();
        raw.extend_from_slice(&token);

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_bytes(&raw).expect("valid header bytes"),
        );

        prop_assert_eq!(bearer_bytes(&headers), Some(token.as_slice()));

        let state = AppState::new(Config::default());
        state.bootstrap(&token, std::collections::HashMap::new());
        prop_assert_eq!(state.token_matches(&token), Some(true));
    }

    /// A scheme that is not `bearer` yields nothing, however it is spelled.
    ///
    /// Guards the case-insensitive comparison against being loosened into a
    /// prefix match, which would make `bearerish <token>` authenticate.
    #[test]
    fn only_the_bearer_scheme_yields_a_token(
        scheme in "[A-Za-z]{1,12}",
        token in prop::collection::vec(header_byte(), 1..16),
    ) {
        let mut raw = scheme.clone().into_bytes();
        raw.push(b' ');
        raw.extend_from_slice(&token);

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_bytes(&raw).expect("valid header bytes"),
        );

        let expected = scheme.eq_ignore_ascii_case("bearer").then_some(token.as_slice());
        prop_assert_eq!(bearer_bytes(&headers), expected);
    }

    /// Control bytes cannot reach the parser at all.
    ///
    /// Recorded as a property rather than assumed: it is why the generators above
    /// exclude them, and if `http` ever loosened this the exclusion would become
    /// a coverage hole rather than a documented boundary.
    #[test]
    fn a_control_byte_is_refused_before_the_parser_sees_it(
        prefix in prop::collection::vec(0x20u8..=0x7e, 0..8),
        control in prop_oneof![Just(0u8), 0x01u8..=0x08, 0x0au8..=0x1f, Just(0x7fu8)],
    ) {
        let mut raw = b"Bearer ".to_vec();
        raw.extend_from_slice(&prefix);
        raw.push(control);
        prop_assert!(
            HeaderValue::from_bytes(&raw).is_err(),
            "byte {control:#04x} must not be constructible in a header value",
        );
    }
}

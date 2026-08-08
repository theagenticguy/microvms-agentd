//! **ARCH-3, ARCH-4, ARCH-5, and BIND-1**: the workspace's dependency edges, asserted exactly.
//!
//! Four requirements that are all the same claim from different sides — the CLI depends on core,
//! core depends on neither the CLI nor the bindings, the bindings depend on core and not on the
//! CLI, and nothing a binding needs lives in the CLI. Written as one file because they share a
//! source: `cargo metadata`'s resolved graph.
//!
//! # An exact edge set, not a set of absences
//!
//! `assert!(no edge from A to B)` passes when A has no dependencies at all — which is what a stub
//! crate looks like. So the assertions below are equalities over the edges *between the four
//! crates in question*, which is what makes them fail if `microvms-py` never grows its dependency
//! on core as well as if it grows one on the CLI.
//!
//! # ARCH-5's witness is an absence, and the absence is checkable
//!
//! "Nothing a binding needs lives in the CLI." The usual way to satisfy that is a promise in a doc
//! comment. Here it is a property: `microvms-cli` has no `lib` target, so there is no Rust API for
//! a binding to depend on even if someone wanted to. That is the strongest available form —
//! inexpressible rather than merely forbidden — and this file asserts it from the metadata.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The workspace's resolved metadata.
fn metadata() -> cargo_metadata::Metadata {
    cargo_metadata::MetadataCommand::new()
        .manifest_path(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .exec()
        .expect("cargo metadata runs")
}

/// The four crates whose edges these requirements are about.
const IN_QUESTION: [&str; 4] = [
    "microvms-cli",
    "microvms-core",
    "microvms-py",
    "microvms-js",
];

/// The direct dependencies of `name` that are among [`IN_QUESTION`].
fn edges_among_ours(metadata: &cargo_metadata::Metadata, name: &str) -> BTreeSet<String> {
    let package = metadata
        .packages
        .iter()
        .find(|package| package.name.as_str() == name)
        .unwrap_or_else(|| panic!("{name} is a workspace member"));
    package
        .dependencies
        .iter()
        .map(|dependency| dependency.name.clone())
        .filter(|dependency| IN_QUESTION.contains(&dependency.as_str()))
        .collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

/// **ARCH-3 and ARCH-4.** The CLI depends on core; core depends on neither the CLI nor a binding.
///
/// The second half is the one that matters architecturally: a library that depended on its own CLI
/// would make every consumer of the library — including both bindings — carry clap, ratatui, and a
/// tokio multi-thread runtime. The first half is asserted as an equality so a CLI that stopped
/// depending on core (by reimplementing it) fails here too.
#[test]
fn the_cli_depends_on_core_and_core_depends_on_neither_the_cli_nor_a_binding() {
    let metadata = metadata();
    assert_eq!(
        edges_among_ours(&metadata, "microvms-cli"),
        set(&["microvms-core"]),
        "the CLI must depend on microvms-core and on no other crate of ours"
    );
    assert_eq!(
        edges_among_ours(&metadata, "microvms-core"),
        BTreeSet::new(),
        "microvms-core must depend on none of the CLI or the bindings: a library that depended on \
         its own CLI would make every library consumer carry clap and a runtime"
    );
}

/// **BIND-1.** Each binding depends on core and **not** on the CLI.
///
/// Asserted as an equality per binding, so it fails in both directions: a binding that grew an
/// edge to the CLI fails, and a binding that has no edge to core at all — which is what a stub
/// looks like — fails too. That second half is why this is not a pair of `assert!(!contains)`
/// calls: those pass against a crate with an empty dependency list, which is exactly the state the
/// bindings are in until their own task lands.
///
/// The bindings are another task's (T-W3-8), so this test is *expected* to be the thing that tells
/// that task it is not finished — and the message says so, rather than reading as a failure of
/// this one.
#[test]
fn each_binding_depends_on_core_and_never_on_the_cli() {
    let metadata = metadata();
    for binding in ["microvms-py", "microvms-js"] {
        let edges = edges_among_ours(&metadata, binding);
        assert!(
            !edges.contains("microvms-cli"),
            "{binding} depends on microvms-cli. BIND-1 and ARCH-5 say nothing a binding needs \
             lives in the CLI, and the CLI has no lib target to depend on — so this edge cannot \
             even compile, which means the manifest is wrong rather than the code."
        );
        assert_eq!(
            edges,
            set(&["microvms-core"]),
            "{binding} must depend on microvms-core (and on no other crate of ours). If this is \
             failing with an empty set, the binding is still T-W1-1's dependency-free stub and \
             T-W3-8 has not landed — which is what this assertion is here to say."
        );
    }
}

/// **ARCH-5's witness.** `microvms-cli` has no library target, so there is nothing for a binding to
/// depend on.
///
/// The strongest available form of "nothing a binding needs lives here": not a rule, but an
/// absence the compiler enforces. A `lib` target added later — even an empty one — would make the
/// edge BIND-1 forbids *possible*, and this is what catches that at the moment it becomes possible
/// rather than at the moment someone uses it.
///
/// **Falsification** — add `src/lib.rs` to this crate and it goes red naming the target. Verified;
/// see the packet's guard proofs.
#[test]
fn the_cli_exports_no_library_target_at_all() {
    let metadata = metadata();
    let package = metadata
        .packages
        .iter()
        .find(|package| package.name.as_str() == "microvms-cli")
        .expect("a workspace member");

    let targets: Vec<(&str, Vec<String>)> = package
        .targets
        .iter()
        .map(|target| {
            (
                target.name.as_str(),
                target.kind.iter().map(|k| k.to_string()).collect(),
            )
        })
        .collect();

    let library: Vec<&(&str, Vec<String>)> = targets
        .iter()
        .filter(|(_, kinds)| {
            kinds.iter().any(|kind| {
                matches!(
                    kind.as_str(),
                    "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
                )
            })
        })
        .collect();
    assert!(
        library.is_empty(),
        "microvms-cli grew a library target ({library:?}). ARCH-5's witness is that it has none: a \
         binding cannot need a type from a crate that exports nothing, and the absence is what \
         makes that a property rather than a promise. Test-only code that needs to be reachable \
         belongs in `src/guards.rs` under cfg(test)."
    );

    // And exactly one binary, named `microvm`, so the crate is what it claims to be.
    let binaries: Vec<&str> = targets
        .iter()
        .filter(|(_, kinds)| kinds.iter().any(|kind| kind == "bin"))
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(binaries, ["microvm"], "{targets:?}");
}

/// The workspace's member list is the six crates the architecture describes.
///
/// Pinned because the requirements above are equalities over a *known* set: a seventh crate that
/// depended on the CLI would satisfy every assertion here while violating ARCH-5, since nothing
/// would have looked at it. This is the assertion that makes the set known.
#[test]
fn the_workspace_members_are_the_crates_the_architecture_names() {
    let metadata = metadata();
    let mut members: Vec<String> = metadata
        .workspace_members
        .iter()
        .filter_map(|id| {
            metadata
                .packages
                .iter()
                .find(|package| package.id == *id)
                .map(|package| package.name.to_string())
        })
        .collect();
    members.sort();
    assert_eq!(
        members,
        [
            "agentd",
            // `model/`'s *package* is `agentd-model`; the directory name is not the crate name,
            // which is exactly why this list is read out of the metadata rather than off `ls`.
            "agentd-model",
            "microvms-cli",
            "microvms-core",
            "microvms-js",
            "microvms-py",
            "protocol",
        ],
        "a workspace member appeared or vanished. The dependency-direction assertions above are \
         equalities over the four crates ARCH-3/4/5 name, so a new member that depended on the \
         CLI would pass all of them — this is what makes the set known."
    );
}

/// Nothing in the workspace depends on `microvms-cli`.
///
/// The general form of BIND-1, and it catches the case the per-binding test cannot: `agentd`, or
/// `model`, or a crate added later growing an edge to the CLI. There is deliberately no exception
/// list — a crate that needs something from the CLI needs it moved into core instead, which is the
/// kickoff's own rule stated the other way round.
#[test]
fn no_workspace_crate_depends_on_the_cli() {
    let metadata = metadata();
    for package in &metadata.packages {
        if !metadata.workspace_members.contains(&package.id) {
            continue;
        }
        if package.name.as_str() == "microvms-cli" {
            continue;
        }
        let depends = package
            .dependencies
            .iter()
            .any(|dependency| dependency.name == "microvms-cli");
        assert!(
            !depends,
            "{} depends on microvms-cli. Nothing a consumer needs lives in the CLI — if something \
             does, it belongs in microvms-core (ARCH-5).",
            package.name
        );
    }
}

// SPDX-License-Identifier: Apache-2.0
//! Fuzzes the tar extraction engine against the data-filter contract.
//!
//! The bytes are the archive, whole. That is the exact position of a hostile
//! packer in `SECURITY.md`'s "in scope" list: escaping the extraction root via
//! a member name, a symlink, or a hard link is a vulnerability, and this is the
//! surface where `RUSTSEC-2026-0068`-class parser divergence would land.
//!
//! Two oracles. A panic anywhere in the engine is a finding outright — the
//! daemon is the only channel into the VM, and `write_tar` runs this engine on
//! authenticated but untrusted bytes. And the extraction root's parent is a
//! canary: after a run, `root` must be the only thing in it. The canary only
//! sees a one-level escape; deeper escapes aim at absolute paths no harness
//! directory can watch, and confinement against those is the kernel-side walk
//! (`Confined`) this target exists to exercise.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let parent = tempfile::tempdir().expect("tempdir");
    let root = parent.path().join("root");
    std::fs::create_dir(&root).expect("create root");

    // Small caps, deliberately: the interesting inputs are member names and
    // header shapes, not volume, and a small byte cap keeps a run's disk
    // appetite bounded. Either verdict is fine; the oracles are below.
    let _ = agentd::fs::fuzz_extract(&root, data, 1_000, 1 << 20);

    let mut entries = std::fs::read_dir(parent.path()).expect("read parent");
    let only = entries.next().expect("root must survive").expect("entry");
    assert_eq!(
        only.file_name(),
        "root",
        "extraction wrote beside the root"
    );
    assert!(
        entries.next().is_none(),
        "extraction wrote beside the root"
    );
});

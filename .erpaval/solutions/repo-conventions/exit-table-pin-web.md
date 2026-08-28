---
tags: [microvms-agentd, microvms-cli, exit-codes, append-only-contract]
modules: [microvms-cli/src/exit.rs, microvms-cli/src/manifest.rs, microvms-cli/tests/manifest.rs, docs/reference/cli.md]
---

# Appending an exit row touches five pinned copies

Adding a row to `EXIT_TABLE` (microvms-cli/src/exit.rs) fails four other pins that must be updated in the same change:

1. `exit.rs` tests: the expected array literal (`[(u8, Option<&str>, &str); N]`), the code-count (`codes.len() == N-1`), the integer-count, and the core↔table equivalence test. When the new row has no core `ErrorKind` (CLI-only, e.g. `ERR_NAME_TAKEN`), rewrite the equivalence as a shared-prefix assertion plus an explicit CLI-only tail — a full-vector equality can never pass again.
2. `manifest.rs` in-crate test `the_manifest_carries_all_*_exit_rows` (row count).
3. `tests/manifest.rs` process test (`rows.len()`).
4. `docs/reference/cli.md` exit-code table and the "N rows, 0 through M" sentence.

Also: `clippy -D warnings` denies dead_code on unused pub methods in this crate (no lib target), so a helper written "for symmetry" with no production caller fails the lint gate — write only the method the caller needs.

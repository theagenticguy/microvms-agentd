# ERPAVal lessons index

Lessons learned from prior ERPAVal sessions. Claude reads this at
session start and greps `.erpaval/solutions/**` for relevant
lessons before starting work.

## By category

### api-patterns
- [axum 0.8's Listener trait is what makes turmoil simulation free](solutions/api-patterns/axum-listener-trait-enables-turmoil.md) — generic serve path, the `L::Addr: Debug` trap, `enable_tokio_io()`
- [aws-config with default-features=false cannot resolve credentials at all](solutions/api-patterns/aws-config-needs-its-own-http-client.md) — the chain does its own HTTP; load() panics; test the real constructor
- [napi-rs collapses custom error codes on Promise rejections](solutions/api-patterns/napi-async-collapses-error-codes.md) — err.cause.message carries the code; type aliases invisible to the derive

### architecture-patterns
- [A byte-offset cursor is what separates a working stream reconnect from a broken one](solutions/architecture-patterns/byte-offset-cursor-is-what-makes-reconnect-work.md) — server-side exec object, framed transport for the terminal event, stdin as a separate request, subscribe-before-replay

- [An absent value is not a neutral one, and the fallback decides](solutions/architecture-patterns/an-absent-value-is-not-a-neutral-one.md) — what a missing value means depends on the consumer's fallback, not on the neighbouring guard; how to find the next agreement pair

### best-practices
- [Capture subprocess output through pipes, not temp files, when grandchildren matter](solutions/best-practices/pipes-not-tempfiles-for-subprocess-output.md) — pgid before wait, concurrent drain, no `pre_exec` for demotion
- [A struct that carries a credential never derives Debug](solutions/best-practices/credential-structs-never-derive-debug.md) — hand-written redaction + per-type guard test; audit the class, not the instance
- [Golden figures come from running the oracle, never from the plan](solutions/best-practices/run-the-oracle-never-rederive-goldens.md) — the plan's ~1357s vs the oracle's 1371.29s; pin executed output verbatim

### test-failures
- [A confinement property that only checks the filesystem measures nothing](solutions/test-failures/proptest-and-dst-tiers-need-verdict-assertions.md) — assert the verdict, not just containment; vacuous model properties
- [A deterministic simulator has two clocks, and a spawned child obeys the wrong one](solutions/test-failures/simulated-time-and-real-children-are-two-clocks.md) — virtual timers against real work; never pace a child with `sleep` under turmoil
- [Four ways a guard passed against broken code in one session](solutions/test-failures/guards-that-passed-against-broken-code.md) — unpinned compile_fail, expiry-vs-margin fakes, uniform draws, oracle-contradicting assertions, fakes more forgiving than the real parser

## Recent additions

- 2026-08-17 (session-053b88): the absent-value asymmetry, from closing issue #35
  and sweeping for the fourth agreement pair.

- 2026-08-08 (session-fa0814): five lessons from the Rust port — the guard-proof
  quadrilogy, aws-config's hidden HTTP client, napi error-code collapse,
  credential Debug hygiene, and run-the-oracle goldens.

- 2026-08-05 (session-7ef43d): the byte-offset cursor pattern and the two-clocks
  trap, from adding SSE streaming plus stdin and the Python client library.
- 2026-08-05 (session-bdf1bf): the first three, from building the axum daemon and
  its proptest + turmoil verification tiers.

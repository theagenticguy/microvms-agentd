# ERPAVal lessons index

Lessons learned from prior ERPAVal sessions. Claude reads this at
session start and greps `.erpaval/solutions/**` for relevant
lessons before starting work.

## By category

### api-patterns
- [axum 0.8's Listener trait is what makes turmoil simulation free](solutions/api-patterns/axum-listener-trait-enables-turmoil.md) — generic serve path, the `L::Addr: Debug` trap, `enable_tokio_io()`

### architecture-patterns
- [A byte-offset cursor is what separates a working stream reconnect from a broken one](solutions/architecture-patterns/byte-offset-cursor-is-what-makes-reconnect-work.md) — server-side exec object, framed transport for the terminal event, stdin as a separate request, subscribe-before-replay

### best-practices
- [Capture subprocess output through pipes, not temp files, when grandchildren matter](solutions/best-practices/pipes-not-tempfiles-for-subprocess-output.md) — pgid before wait, concurrent drain, no `pre_exec` for demotion

### test-failures
- [A confinement property that only checks the filesystem measures nothing](solutions/test-failures/proptest-and-dst-tiers-need-verdict-assertions.md) — assert the verdict, not just containment; vacuous model properties
- [A deterministic simulator has two clocks, and a spawned child obeys the wrong one](solutions/test-failures/simulated-time-and-real-children-are-two-clocks.md) — virtual timers against real work; never pace a child with `sleep` under turmoil

## Recent additions

- 2026-08-05 (session-7ef43d): the byte-offset cursor pattern and the two-clocks
  trap, from adding SSE streaming plus stdin and the Python client library.
- 2026-08-05 (session-bdf1bf): the first three, from building the axum daemon and
  its proptest + turmoil verification tiers.

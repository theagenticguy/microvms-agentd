# ERPAVal lessons index

Lessons learned from prior ERPAVal sessions. Claude reads this at
session start and greps `.erpaval/solutions/**` for relevant
lessons before starting work.

## By category

### api-patterns
- [axum 0.8's Listener trait is what makes turmoil simulation free](solutions/api-patterns/axum-listener-trait-enables-turmoil.md) — generic serve path, the `L::Addr: Debug` trap, `enable_tokio_io()`

### best-practices
- [Capture subprocess output through pipes, not temp files, when grandchildren matter](solutions/best-practices/pipes-not-tempfiles-for-subprocess-output.md) — pgid before wait, concurrent drain, no `pre_exec` for demotion

### test-failures
- [A confinement property that only checks the filesystem measures nothing](solutions/test-failures/proptest-and-dst-tiers-need-verdict-assertions.md) — assert the verdict, not just containment; vacuous model properties

## Recent additions

- 2026-08-05 (session-bdf1bf): all three above, from building the axum daemon and
  its proptest + turmoil verification tiers.

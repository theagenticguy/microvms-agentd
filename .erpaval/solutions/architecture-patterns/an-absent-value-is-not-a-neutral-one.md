---
title: An absent value is not a neutral one, and the fallback decides
category: architecture-patterns
tags: [guards, agreement-pairs, defaults, dockerfile, agentd-port, sse-keepalive, s2]
session: session-053b88
date: 2026-08-17
---

# An absent value is not a neutral one, and the fallback decides

The agreement guards in `microvms-core/src/control/artifact.rs` all answer the
same question — two values set independently that must agree — and each has to
decide separately what an ABSENT value means. Copying the answer from a
neighbouring guard is where issue #35's open question came from.

- `require_matching_from`: no `FROM` → pass. Correct, because the build then has
  nothing to build and says so in its own error.
- `require_matching_agentd_port`: no `ENV AGENTD_PORT` → **refuse** when the
  client is off the default. `Config::from_env` only assigns when `env_parse`
  returns `Some` (`agentd/src/config.rs:118`), so the guest keeps `port: 9000`
  (`:84`) and lands in `CREATE_FAILED` with NOTHING in the Dockerfile to point
  at — harder to diagnose than a wrong value, not easier.
- `require_keepalive_under_idle_timeout`: no `AGENTD_SSE_KEEPALIVE_SECS` → pass,
  because the daemon's 15s default is already under every client timeout.

The deciding question is never "is the other guard permissive here?" but **what
does the consumer do when the value is missing?** A consumer that errors out
makes absence safe to pass. A consumer with a silent fallback makes absence a
disagreement wearing a disguise, because the fallback is a third value nobody
wrote down.

Corollary: an UNPARSEABLE value belongs wherever the absent one does, whenever
the consumer treats them alike. `env_parse` warns and keeps the default
(`agentd/src/config.rs:174-179`), so `AGENTD_PORT=nine` and no `AGENTD_PORT` have
one consequence in the guest and earned one verdict in the guard. Reasoning about
the parser ("an unparseable value is not this guard's business") gave the wrong
answer; reasoning about the guest gave the right one.

## Finding the next pair

The sweep that found the SSE keepalive pair worked by reading DOCSTRINGS for
derived constants, not by grepping for env vars.
`DEFAULT_STREAM_IDLE_TIMEOUT`'s own comment says "Four times the daemon's
fifteen-second SSE keepalive" (`microvms-core/src/session/exec.rs:56`) — a
constant justified by another component's DEFAULT, which a caller's Dockerfile
can move. A comment explaining a number in terms of a value owned elsewhere is
the signature of this bug class. Grep for prose like "twice the", "four times",
"matching the", "same as the daemon's".

See also [[guards-that-passed-against-broken-code]]: the vacuous-condition break
(`if true`) is the right proof for a guard whose new behaviour is a REVERSAL of
the shipped one, because the break reproduces exactly the old behaviour.

---
title: axum 0.8's Listener trait is what makes turmoil simulation free
category: api-patterns
tags: [rust, axum, turmoil, testing, dst, transport]
session: session-bdf1bf
date: 2026-08-05
---

## Lesson

Make the serve path generic over `axum::serve::Listener` and a deterministic
network simulator costs nothing in production code:

```rust
pub async fn serve<L>(listener: L, app: Router) -> std::io::Result<()>
where
    L: Listener + Send + 'static,
    L::Addr: std::fmt::Debug,   // REQUIRED, see below
{
    axum::serve(listener, app).with_graceful_shutdown(shutdown()).await
}
```

`tokio::net::TcpListener` already implements `Listener`, and a turmoil listener is
a five-line newtype in the test tier over `turmoil::net::TcpListener` with
`type Io = turmoil::net::TcpStream`. turmoil stays a dev-dependency, so the
shipping binary cannot link the simulator.

Pre-0.8 patterns on the internet show a hand-rolled
`hyper::server::conn::http1::Builder::serve_connection` loop with `TokioIo` and
`TowerToHyperService`. That is obsolete for axum 0.8 and much more code.

## The two traps

`L::Addr: Debug` is required. Omitting it fails with E0277 saying
`Serve<L, Router, Router> is not a future`, which points nowhere near the missing
bound (axum 0.8.9 `serve/mod.rs:218-226`).

`turmoil::Builder::enable_tokio_io()` is required if the served code registers a
signal handler. Graceful shutdown listening for SIGTERM panics inside a turmoil
host without it. It does not let network I/O escape the sim.

## Why it matters

Roughly a quarter of the defects in this project's Python predecessor were
transport-layer: keep-alive body desync, drain-or-close framing, a body buffered
before authorization. Under turmoil those become seeded, replayable tests, and
virtual time makes a 70-minute token-expiry scenario run instantly.

See [[proptest-and-dst-tiers-need-verdict-assertions]] for the mistake that makes
these tiers pass while measuring nothing.

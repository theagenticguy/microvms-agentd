---
title: aws-config with default-features=false cannot resolve credentials at all
category: api-patterns
tags: [aws, aws-config, sigv4, credentials, hand-rolled-client]
session: session-fa0814
date: 2026-08-08
---

# aws-config with default-features=false cannot resolve credentials at all

Hand-rolling a client for an unmodeled AWS service (reqwest + aws-sigv4) makes
`default-features = false` on aws-config look right — you already have an HTTP
client. It is wrong: the credential chain does ITS OWN HTTP (IMDS, SSO, STS)
through smithy's client, and `load()` PANICS with "a http_client is required"
before asking any credential question. Keep `default-https-client` on; two
stacks on one rustls is the price (microvms-core/Cargo.toml:58).

The deeper lesson: nothing caught it because all 300 tests constructed through
the injectable transport — the one constructor that talks to the world had no
test. Every `new()` that touches the real environment needs at least one test
that calls it and asserts a Result of either flavor; the panic is the bug
(microvms-core/src/control/transport.rs, constructing_the_real_transport_...).

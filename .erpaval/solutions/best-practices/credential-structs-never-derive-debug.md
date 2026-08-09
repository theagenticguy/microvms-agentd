---
title: A struct that carries a credential never derives Debug
category: best-practices
tags: [security, debug, logging, tokens, rust]
session: session-fa0814
date: 2026-08-08
---

# A struct that carries a credential never derives Debug

Three of six token-carrying types in one workspace leaked secrets through
derived Debug (control ProxyToken, RunHookPayload with the agent token,
HttpRequest with Authorization + proxy headers) while their three siblings
hand-wrote redaction — the invariant was known and still missed half its
sites, because a derive is the default and nothing flags it. Rules that
stick: hand-write Debug printing names/lengths only; add the guard test
(format with {:?}, assert the secret absent) PER TYPE, mirroring
microvms-core/src/session/proxy.rs:839; and when a review finds one leaky
derive, grep every struct holding a token/header/payload — the class, not
the instance. Redact ALL header values, not an allowlist: an allowlist is a
list someone must remember to extend.

---
title: napi-rs collapses custom error codes on Promise rejections
category: api-patterns
tags: [napi, bindings, errors, nodejs]
session: session-fa0814
date: 2026-08-08
---

# napi-rs collapses custom error codes on Promise rejections

`napi::Error` with a custom code keeps it on the SYNC path but the async path
is typed over napi's closed Status enum: measured `code="ERR_INVALID_ARG"`
sync vs `code="GenericFailure"` async on the same error. Since nearly every
binding method is async, "read err.code" works in the first test someone
writes and fails in production. The contract that survives both paths:
`err.cause.message` carries the ERR_* code, `err.cause.cause.message` the
fine-grained wire kind (microvms-js/src/*, T-W3-8 packet). Also: napi-derive's
return-type parse is syntactic — a type alias is invisible to it; and
`ClassInstance` is not Send, so guarded values cross async boundaries as
reference parameters, not object fields.

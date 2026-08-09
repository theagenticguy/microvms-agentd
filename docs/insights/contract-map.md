# microvms-agentd · Contract map

A "contract" here is not the default "type declared in module A, imported by module B". This is a
single Cargo workspace where `cargo build` already catches that case: rename a field in `protocol/`
and every crate that reads it stops compiling. The contracts worth mapping are the ones the compiler
cannot see.

So: **a contract is a shape whose producer and consumer sit in different processes, different
languages, or different artifacts — such that a change on one side does not fail the build.** That
covers seven of the nine sections below. Two same-workspace contracts are included because they are
the boundary a compiler does check and everything else leans on (the `protocol` crate) or because the
two definitions are deliberately *not* shared (the proxy header pair).

The repo is unusually explicit about this. Nearly every contract below carries a gate whose job is to
be the thing that disagrees, and several of those gates carry a comment explaining that a check with
nothing to disagree with reports clean and is therefore worse than no check. Where such a comment
exists it is quoted, because it is the assumption a future reader most needs.

Ordered by consumer count descending.

## The `protocol` crate — daemon↔client wire types

**Producer:** `protocol/src/lib.rs:29-32` (four modules), types in `protocol/src/exec.rs`,
`protocol/src/fs.rs`, `protocol/src/health.rs`, `protocol/src/hook.rs`

**Consumer(s):**

- `agentd/src/exec.rs:73-90` — the daemon re-exports the exec types so `exec::Phase` keeps working.
- `agentd/src/routes.rs:18-20` — `VERSION_HEADER`, `Health`, `DiskHealth`, `HOOK_PREFIX`, `RunHook`,
  `RunHookEnvelope`.
- `agentd/src/fs.rs:63` — `FsQuery`.
- `agentd/src/schema.rs:51` — `PROTOCOL_VERSION`, re-exported into the published document.
- `microvms-core/src/lib.rs:77` — `pub use protocol;`, so the whole crate is re-exported downstream.
- `microvms-core/src/session/mod.rs:282-363` — `Health`, `StartRequest`, `StartResponse` on the wire.
- `microvms-core/src/session/sse.rs:287-322` — dispatches on `EVENT_OUTPUT` / `EVENT_GAP` /
  `EVENT_EXIT` and deserializes the three payload types.
- `microvms-cli/src/commands/lifecycle.rs:683` — builds a `protocol::exec::StartRequest`.
- `microvms-py/Cargo.toml:31-32` and `microvms-js/Cargo.toml:24-25` — both bindings take the direct
  dependency, cited as ARCH-2.

**Shape:**

```rust
/// Protocol version, distinct from the daemon version.
pub const PROTOCOL_VERSION: &str = "1";

/// The response header carrying the daemon version, on every response including
/// errors.
pub const VERSION_HEADER: &str = "microvms-agentd-version";
```

```rust
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Running,
    Exited,
    Acked,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct PollResponse {
    pub exec_id: String,
    pub phase: Phase,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(flatten)]
    pub result: Option<Outcome>,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct ErrorBody {
    pub error: Cow<'static, str>,
    pub detail: String,
}
```

**Assumptions consumers make:**

- Every type derives *both* halves of serde even where one side needs one, because the missing half
  is what a client would hand-write — stated as the crate's reason for existing at
  `protocol/src/lib.rs:23-27` and again at `protocol/src/exec.rs:3-8`.
- The SSE dispatcher assumes the three event names are exactly the three constants and silently
  drops an unrecognized name: `microvms-core/src/session/sse.rs:287-322` matches
  `EVENT_OUTPUT`/`EVENT_GAP`/`EVENT_EXIT` and has no other arm that produces an event.
- A client assumes stdout/stderr share **one** offset space, so it holds one cursor rather than two
  that can disagree about ordering (`protocol/src/exec.rs:53-54`).
- A client assumes the terminal `exit` event always precedes a clean stream close, so a body that
  closes without one means the connection failed rather than the command finishing
  (`protocol/src/exec.rs:156-158`).
- A client branches on `ErrorBody.error` and the status code, **never** on `detail`
  (`protocol/src/exec.rs:201-205`) — which is why the ten slugs are `&'static str` consts chosen at
  each call site rather than formatted strings (`protocol/src/exec.rs:221-247`).
- `PollResponse.result` is absent-when-running on the way out and must read back as `None` on the way
  in; a client that treated the missing outcome as an error would fail on every poll before the first
  one that mattered (`protocol/src/exec.rs:302-331`).
- Version negotiation has no *request*: the daemon serves exactly one protocol version, so a client
  that has read the doc already knows which (`protocol/src/lib.rs:56-57`). Same
  `protocol_version` with a different `daemon_version` means proceed, and must not be treated as an
  error (`protocol/src/lib.rs:42-46`).
- `Health.disk == null` is not `disk` absent: unmeasurable free space is distinct from zero, and a
  monitor that conflated them would page on a missing `statvfs`
  (`protocol/src/health.rs:31-33`, test `protocol/src/health.rs:67-81`).
- `FsQuery.mode` is a **string**, because `0644` in JSON is either a syntax error or decimal 644
  depending on the parser (`protocol/src/fs.rs:21-23`).

**Drift risk:** Adding a variant to `Phase` or `StreamKind`, or a fourth SSE event name, compiles on
both sides and is then silently dropped by the SSE dispatcher's fall-through
(`microvms-core/src/session/sse.rs:287-322`). Mitigation: add the variant and the dispatch arm in the
same commit, and let the `docs/schema.json` byte-compare (below) surface the new shape in review.

## `docs/schema.json` — the generated wire contract, byte-compared

**Producer:** `agentd/src/schema.rs:232-323` (`document`), written by `agentd/src/bin/schema.rs:22-54`

**Consumer(s):**

- `agentd/src/bin/schema.rs:63-90` — `--check`, the CI gate.
- `agentd/tests/schema_artifact.rs:39-50` — the same comparison as a test.
- `agentd/tests/schema_artifact.rs:291-323` — the live `GET /v1/schema` route, compared against the
  committed file as parsed JSON.
- `.github/workflows/ci.yml:98-100` — `cargo run -p agentd --bin schema -- --check`.
- `mise.toml:118-123` — the `schema:check` task, in `check`'s dependency list at `mise.toml:205`.
- Any external client generator: the document is the published surface, and the route is
  unauthenticated precisely so a client can read it before it has a token
  (`agentd/tests/schema_artifact.rs:290-308`).

**Shape:**

```rust
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "microvms-agentd wire protocol",
        "daemon_version": crate::routes::VERSION,
        "protocol_version": PROTOCOL_VERSION,
        "version_header": crate::routes::VERSION_HEADER,
        "hook_prefix": crate::routes::HOOK_PREFIX,
        "generated_from": "the daemon's own serde types, via schemars",
        "auth": { ... },
        "unmatched_path": { ... },
        "limits": limits(config),
        "routes": routes,
        "$defs": defs,
        // Empty in a correct build. A name here means one type generated two
        // different shapes under the serialize and deserialize contracts, so a
        // `$ref` in the document would resolve to whichever won — the artifact
        // test asserts this stays empty rather than trusting that it does.
        "definition_collisions": collisions,
    })
```

**Assumptions consumers make:**

- The comparison is **byte for byte**, not parsed-JSON equality, because the formatting is part of
  what is committed: a semantic comparison would pass on a hand-reformatted file and the next
  regeneration would land a diff nobody asked for (`agentd/src/bin/schema.rs:58-62`).
- The document is generated against `Config::default()` rather than `from_env()`, so the artifact is a
  function of the source alone and the check does not fail on whichever machine has `AGENTD_PORT`
  exported (`agentd/src/bin/schema.rs:11-15`).
- `definition_collisions` is empty. Two generators run — `for_serialize()` and `for_deserialize()`
  (`agentd/src/schema.rs:241-246`) — because a field with a serde default is optional inbound and
  present outbound, and one generator cannot say both (`agentd/src/schema.rs:236-240`).
  `merge_definitions` *reports* rather than resolves a disagreement, since silently preferring one
  would publish a shape the daemon does not use on one of the two paths
  (`agentd/src/schema.rs:327-330`).
- Consumers must accept **either** rendering of a unit-variant enum. schemars 1.x emits `oneOf` +
  `const` when variants carry doc comments and a flat `enum` array when they do not, so `Phase`
  (documented) and `StreamKind` (not) appear in two forms in one document
  (`agentd/src/schema.rs:792-798`).
- The router is assembled from the same list the document is (`agentd/src/schema.rs:182-186`), so an
  undocumented route cannot be served; the tests therefore check the *reverse* direction — a
  documented route the daemon no longer serves (`agentd/tests/schema_artifact.rs:142-174`) and a
  status code the document promises that the daemon stopped producing (`:176-200`).
- The published `limits` are read from the **running** `Config`, so an operator who raised a cap by
  environment variable publishes their own numbers (`agentd/src/schema.rs:204-210`). Two of those
  caps are indistinguishable from a transport failure at the moment they fire
  (`agentd/tests/schema_artifact.rs:326-327`).

**Drift risk:** A doc comment added to a `protocol` field changes the artifact, because schemars
publishes doc comments as `description` — which is why `Health.version` carries a **line** comment
rather than a doc comment, stated in as many words at `protocol/src/health.rs:16-18`. Mitigation: run
`cargo run -p agentd --bin schema` in the same commit as any `protocol` edit; the gate then reports a
diff rather than a consumer discovering it.

## The botocore service model as wire contract

**Producer:** botocore's shipped `lambda-microvms` `2025-09-09` service model, resolved through the
package rather than by path (`scripts/check-model-drift:106-145`)

**Consumer(s):**

- `microvms-core/src/constants.rs:176-209` — `as_json()`, the 19 constants the client hardcodes.
- `scripts/check-model-drift:441-629` — `check()`, every constraint compared by name.
- `microvms-cli` `constants --emit-json` — the subprocess the gate reads
  (`scripts/check-model-drift:94-103`, `:296-331`).
- `.github/workflows/ci.yml:222` — `./scripts/check-model-drift`.
- `mise.toml:157` — the `model:check` task, in `check`'s dependency list at `mise.toml:205`.

**Shape:**

```python
CONSTANT_NAMES = (
    "MODEL_API_VERSION",
    "MAX_RUN_HOOK_PAYLOAD_BYTES",
    "MAX_IMAGE_NAME_LEN",
    "IMAGE_NAME_PATTERN",
    "MAX_DURATION_SEC",
    "MAX_MICROVM_HOOK_TIMEOUT_SEC",
    "MAX_IMAGE_HOOK_TIMEOUT_SEC",
    "MAX_HOOK_PORT",
    "CAPABILITIES",
    "ARCHITECTURES",
    "MAX_NETWORK_CONNECTORS",
    "MAX_RESOURCES",
    "MAX_CLIENT_TOKEN_LEN",
    "MODEL_IMAGE_READY_STATES",
    "TOLERATED_IMAGE_READY_STATES",
    "TERMINAL_STATES",
    "DEAD_STATES",
    "MICROVM_REGIONS",
    "SIZE_CLASSES",
)
```

**Assumptions consumers make:**

- The key set is a **closed set in both directions**. A missing name exits naming the constant
  (`scripts/check-model-drift:256-265`), because "a renamed constant does not fail compilation in
  either language — it makes this comparison stop happening, and a check that stops happening reports
  clean". An *extra* key exits too (`:353-361`), because a constant in the dump with no comparison
  written for it reads as covered and is not.
- The API version is checked before anything is compared, as a hard stop rather than one drift line
  among many: a constraint checked against a different model version is a constraint that was not
  checked (`scripts/check-model-drift:742-751`).
- An absent model is a `SystemExit`, never a skip (`scripts/check-model-drift:114-127`), and there is
  no `--skip-rust` flag any more because skipping the only client would leave the gate comparing
  nothing (`:724-727`).
- Two values the model states nothing about are compared against **literals in the script itself** —
  `PINNED_REGIONS` (`:210-216`) and `PINNED_SIZE_CLASSES` (`:222-228`). They were verified only by
  the Python-vs-Rust cross-comparison, and when that client was deleted the two `c.record` calls
  degenerated into self-comparisons that could not fail (`:50-59`). Those two comparisons pass their
  own `sides=` so a failure names "pinned here" rather than "model" and does not send the reader to
  the wrong file (`:610-627`, mechanism at `:417-423`).
- The six hook-timeout shapes are checked **separately**, not one per family, because they are six
  shapes in the model and AWS can move one without moving its siblings
  (`scripts/check-model-drift:522-539`).
- `SIZE_CLASSES` is deliberately excluded from the sorted-before-comparison set, because its order
  *is* meaningful — smallest baseline first — and sorting it would hide a reordering that matters
  (`scripts/check-model-drift:180-184`).
- The uncovered list is not decoration. It named `RunMicrovmRequestClientTokenString` as unbound, and
  checking it found that `run`'s token — which defaults its scope to a full image ARN — exceeded 128
  characters for a legal 64-character image name in the longest-named region, and would have failed
  the launch on a field the caller never set (`scripts/check-model-drift:576-580`).

**Drift risk:** The gate reports **33 constraints** compared and 37 constrained shapes it binds
nothing to (measured by running `./scripts/check-model-drift` on 2026-08-09). A new AWS constraint
lands in that uncovered list, which is printed but is not a failure. Mitigation: read the uncovered
list — with `--verbose` for the stated value — when bumping botocore, since that list is the answer
to "what did this not check" (`scripts/check-model-drift:679-691`).

## The CLI envelope — `apiVersion` 1 and `data.kind`

**Producer:** `microvms-cli/src/envelope.rs:66` (`API_VERSION`), `:314-321` (`ok`), `:324-342`
(`error`)

**Consumer(s):**

- `conformance/run_rs.py:159-214` — the `Envelope` dataclass, reading every field directly.
- `conformance/run_rs.py:279-304` — `Cli.call`, which raises `KindError` on a failure envelope.
- `conformance/run_rs.py:390-408` — `parse_stdout`, on all ~60 invocations.
- `conformance/run_rs.py:217-244` — `KindError`, which carries kind, code, and exit code.
- `microvms-cli/src/manifest.rs:91-110` — the manifest publishes the envelope's field list.
- `microvms-cli/src/render.rs:60-96` — the cost payload nests inside `data`.
- Any agent or shell reading `--json` output; `microvms-cli/src/envelope.rs:62-65` states the
  pinning promise to them.

**Shape:**

```rust
/// A failure envelope. Every field unconditional; see the module docs.
pub fn error(failure: &CliError) -> Value {
    let mut data = failure.data.clone();
    // The fine-grained daemon status, for the consumer the exit code is too coarse for.
    // Inserted rather than replacing whatever `data` already holds, so a teardown's leaked
    // identifiers and the kind coexist on one failure.
    if let Some(wire) = failure.wire_kind {
        data.insert("kind".to_string(), json!(wire.as_str()));
    }
    json!({
        "status": "error",
        "apiVersion": API_VERSION,
        "error": failure.message,
        "code": failure.code(),
        "exitCode": failure.exit.as_u8(),
        "finding": failure.finding(),
        "suggestions": failure.suggestions,
        "data": Value::Object(data),
    })
}
```

**Assumptions consumers make:**

- `apiVersion` is bumped when a field's *meaning* changes, never when a command is added — an agent
  pinned to `"1"` must keep parsing, and a new command changes the manifest instead
  (`microvms-cli/src/envelope.rs:62-65`).
- Every failure field is unconditional: `finding` present-and-empty, `suggestions` `[]`, `data` `{}`,
  never absent. "A key that appears conditionally is a key every consumer has to guard, and the
  consumer that forgets reads `undefined` as 'no finding' for a failure that had one"
  (`microvms-cli/src/envelope.rs:20-25`). `conformance/run_rs.py:164-167` takes that at its word and
  reads them directly so a vanished field is a `KeyError` rather than a `None` that flows into an
  assertion and passes.
- The exit code is deliberately **coarser** than the daemon's status discipline: five `WireKind`s
  collapse onto `ERR_PROTOCOL` — `Conflict`, `NotFound`, `ProtocolError`, `StdinClosed`, `TooLarge`
  (pinned at `microvms-cli/src/exit.rs:532-548`) — because a shell branching on `$?` cannot act
  differently on a 400 than on a 409 (`microvms-cli/src/exit.rs:39-44`). So the conformance oracle
  asserts on `data.kind` instead (`conformance/run_rs.py:220-231`, envelope side
  `microvms-cli/src/envelope.rs:27-31`).
- The **absence** of `data.kind` is information: it says the CLI refused locally before any call
  reached the daemon (`conformance/run_rs.py:182-191`, producer side
  `microvms-cli/src/envelope.rs:450-453`).
- Branch on `code`, never on `error` — `error` is human-readable and may be reworded between releases
  (`microvms-cli/src/manifest.rs:102`, `:113`).
- Progress is on stderr always, and `--quiet` cannot buy silence about a leak or a stale rate:
  `warn` ignores `quiet` and `progress` honours it (`microvms-cli/src/envelope.rs:13-18`,
  `:144-155`, both halves tested at `:489-496`). `conformance/run_rs.py:263-265` relies on this to
  pass `--quiet` on every invocation.
- The process exit code and `envelope.exitCode` are two independent renderings of one decision, and
  CLI-3 is the claim that they agree — so the driver cross-checks rather than trusting either
  (`conformance/run_rs.py:281-298`).
- `exec --stream` is the one exception and carries a **different discriminant**,
  `microvm.exec.stream` rather than `microvm.exec`, so a consumer branching on `type` learns which
  parse applies from the field it reads first (`microvms-cli/src/envelope.rs:33-45`). The envelope is
  written **compact** once a stream has started, because "the last line is the envelope" is only true
  if the envelope is one line (`microvms-cli/src/envelope.rs:46-49`, `:167-175`). The consumer
  asserts all three properties rather than tolerating them (`conformance/run_rs.py:306-388`), and
  uses a separate reader because `Cli.call`'s whole assertion is that stdout is *one* document
  (`conformance/run_rs.py:327-329`).

**Drift risk:** A new field added to `data` on a success envelope is invisible to `Envelope.parse`,
which reads `data` as an opaque dict (`conformance/run_rs.py:196`) — so a key the CLI stops emitting
fails only at the check that reads it, not at parse time. Mitigation: the manifest's `responseKeys`
per command is the machine-readable list, asserted non-empty for every command
(`microvms-cli/src/manifest.rs:426-443`).

## The manifest — generated, machine-readable, never hand-maintained

**Producer:** `microvms-cli/src/manifest.rs:34-130` (`build`)

**Consumer(s):**

- `microvms-cli/tests/manifest.rs:46-88` — cross-checks the manifest's commands against what the
  binary routes, with `microvms-cli/src/commands/mod.rs:102` (`RESPONSE_TYPES`, arity 16) as the one
  hand-written table.
- `microvms-cli/src/manifest.rs:200-253` — `render`, the human and dense views.
- `conformance/run_rs.py:310-314` — reads `exec`'s `alternateResponse` as the published statement of
  the NDJSON exception.
- Any agent choosing a command: the manifest is the whole surface, and its stated value is that it
  **cannot be wrong** (`microvms-cli/src/manifest.rs:4-11`).

**Shape:**

```rust
            json!({
                "name": name,
                "summary": sub.get_about().map(|about| about.to_string()).unwrap_or_default(),
                "parameters": sub.get_arguments().map(parameter).collect::<Vec<_>>(),
                "supportsJson": true,
                "responseType": kind,
                "responseKeys": keys,
                // Null for every command but `exec`, and present-and-null rather than absent for
                // the same reason the failure envelope's `finding` is: a key that appears
                // conditionally is a key every consumer has to guard.
                "alternateResponse": alternate,
            })
```

**Assumptions consumers make:**

- Everything is read out of `clap::Command` by introspection and out of `EXIT_TABLE`
  (`microvms-cli/src/manifest.rs:36-38`, `:85-90`), so a flag added to a handler appears without
  anyone remembering. The one thing that is a table is cross-checked against the clap tree, "so a
  command added without a row fails rather than shipping undescribed"
  (`microvms-cli/src/manifest.rs:13-16`).
- The command list is an **equality** with the registered subcommands, not a subset check: a manifest
  listing a command the parser does not accept is as bad as one omitting a command it does
  (`microvms-cli/src/manifest.rs:259-283`).
- `choices` is a closed set or `null`, and it is the CLI-5 witness — the field a reviewer or a test
  reads to see whether an S1 guard was downgraded to a convenience string flag
  (`microvms-cli/src/manifest.rs:18-23`). A free-text parameter reports `null`, never `[]`, because
  `[]` says "a closed set with nothing in it", which is not a thing, and a consumer treating it as a
  domain would refuse every value (`microvms-cli/src/manifest.rs:395-399`).
- A boolean flag publishes **no** domain even though clap reports `["true", "false"]` for `SetTrue`,
  because nineteen false positives would make the CLI-5 witness unreadable
  (`microvms-cli/src/manifest.rs:134-140`, `:461-486`). `is_flag` reads the *action* rather than the
  absence of possible values, since "has no domain" is not the same question
  (`microvms-cli/src/manifest.rs:188-197`).
- `alternateResponse` is present-and-null for every command but `exec`, asserted in both directions:
  a `--stream` added elsewhere without a response row would publish an undocumented second NDJSON
  shape, and an `exec` that lost the entry would leave a consumer parsing NDJSON as one document
  (`microvms-cli/src/manifest.rs:285-332`).
- The streaming exception is stated **twice** — in `exec`'s `alternateResponse` and in the
  `conventions` list — because the two lists are read by different consumers: an agent choosing a
  command reads the command entry, one writing a parser reads the conventions
  (`microvms-cli/src/manifest.rs:120-123`). The convention must name the discriminant, or a parser
  author is left guessing how to detect it (`microvms-cli/src/manifest.rs:498-513`).
- Every command's `summary` is the first line of its doc comment, and a deleted doc comment would
  publish an empty summary rather than fail — which is why the length is asserted
  (`microvms-cli/src/manifest.rs:445-459`).

**Drift risk:** The three count assertions (16 commands, 14 exit rows, 6 conventions) are literals in
tests (`microvms-cli/src/manifest.rs:278-283`, `:339`, `:497`), so adding a command or an exit code
fails the test rather than shipping silently. That is the intended behaviour; no mitigation needed
beyond updating the count in the same commit.

## The cost JSON shape — one shape, four independent emitters

**Producer:** `microvms-cli/src/render.rs:60-96` (`report_to_json`), `:99-129` (`line_to_json`), over
`microvms-core/src/cost.rs:1476-1573` (`CostReport`) and `:1438-1447` (`LineItem`)

**Consumer(s):**

- `microvms-py/src/cost.rs:392-423` — `line_to_dict`, the Python binding's copy of the same shape.
- `microvms-js/src/cost.rs:313-345` — `line_json`, the Node binding's copy, hand-assembled.
- `microvms-cli/src/commands/cost.rs:139-140` — nests the report under `data.report`.
- `microvms-cli/src/render.rs:162` — `report_dense`, the token-lean rendering.
- Any consumer summing a dollar column, which is the reader the whole shape is designed against.

**Shape:**

```rust
/// One line item as JSON. See the module docs on the missing `usd` key.
pub fn line_to_json(item: &LineItem) -> Value {
    let amount = match &item.amount {
        Amount::Estimated(usd) => json!({
            "kind": "estimated-usd",
            "usd": usd.amount().to_string(),
        }),
        Amount::Unpriced { reason } => json!({
            "kind": "unpriced",
            "reason": reason,
        }),
    };
```

**Assumptions consumers make:**

- An unpriced line emits **no `usd` key at all**, not a null, because a null gets summed as zero by
  anything permissive: "That is the one arithmetic this file refuses to enable"
  (`microvms-cli/src/render.rs:7-13`). Both bindings restate it independently
  (`microvms-py/src/cost.rs:387-391`, `microvms-js/src/cost.rs:308-312`).
- The Node binding returns the line as a JSON **string** rather than an object, because
  `#[napi(object)]` cannot express an absent key — an `Option` field serializes as `null`, which is
  the one value that gets summed as zero (`microvms-js/src/cost.rs:291-296`). And it is
  hand-assembled rather than derived, because a serde derive over an `Option` is exactly what would
  get the absent key wrong (`microvms-js/src/cost.rs:308-312`).
- Every **dollar** crosses as a string and every **seconds** figure as a number, and the split is
  about which consumer is being protected: a caller summing a dollar column must be stopped from
  doing float arithmetic on money, and a caller comparing a duration against a timeout must not have
  to discover that one client quotes seconds in quotes (`microvms-cli/src/render.rs:15-25`).
- A dollar string's **scale** is explicitly not part of the contract. `rust_decimal` normalizes a
  product's scale differently from Python's `decimal`, so `0.0384` and `0.03840000` both occur for
  the same line item; the figures are numerically equal and "comparing the strings byte for byte was
  never a supported operation" (`microvms-cli/src/render.rs:27-46`). The one place scale *is*
  asserted byte for byte is a **rate**, because a rate is a transcription and a derived figure is not
  a transcription of anything (`microvms-cli/src/render.rs:48-51`).
- The total's floor is published under `priced` and never under `total`, because a caller reading a
  field called `total` would have no reason to check `isLowerBound`
  (`microvms-cli/src/render.rs:87-94`, restated at `microvms-py/src/cost.rs:429-433` and
  `microvms-js/src/cost.rs:375-378`). `AtLeast`'s floor and `Exact`'s figure share the field on
  purpose: a consumer that ignores `isLowerBound` gets a number that is never an over-statement
  (`microvms-cli/src/render.rs:88-90`).
- Compute figures read `baseline_*` and never the peak. The 2 GB class reports 8 GB in the guest, so
  reading the peak would overstate the memory line exactly 4x
  (`microvms-core/src/cost.rs:1577-1583`).
- The `(None, None)` arm in both bindings is written rather than `unreachable!()`, because a panic
  across an FFI boundary is not an ordinary error and a third `Amount` variant should degrade to a
  visibly incomplete record rather than abort the interpreter or Node
  (`microvms-py/src/cost.rs:398-401`, `microvms-js/src/cost.rs:323-326`).

**Drift risk:** A third `Amount` variant would need the absent-key rule reimplemented in four places
and nothing forces that — `microvms-cli/src/render.rs:100-109` is an exhaustive match and would fail
to compile, but the two bindings match on `(estimate(), unpriced_reason())` tuples and would silently
take the `(None, None)` arm. Mitigation: if a variant is added, add it to the exhaustive CLI match
first and let the compiler point at the one site, then port to both bindings in the same commit.

## The twin rate tables — deliberate duplication as an independent oracle

**Producer:** `microvms-core/src/cost.rs:1009-1024` (`pinned_rates()`)

**Consumer(s):**

- `scripts/check-live-rates:118-129` — `PINNED`, the deliberate second copy.
- `scripts/check-live-rates:147-212` — `verify_twin`, which reads the Rust literals as text.
- `scripts/check-live-rates:537-556` — `check_drift`, the pinned table against the live Pricing API.
- `.github/workflows/live-conformance.yml:111` — `./scripts/check-live-rates`.
- `mise.toml:256-275` — the `live:rates` task, in `live`'s dependency list at `mise.toml:305`.
- `docs/PLATFORM.md` "What actually costs money" — the third copy, prose
  (`microvms-core/src/cost.rs:990-996`).

**Shape:**

```rust
pub fn pinned_rates() -> RateTable {
    RateTable {
        region: Region::UsEast1,
        source_url: "https://aws.amazon.com/lambda/pricing/".to_string(),
        retrieved: CalendarDate::from_ymd(2026, 8, 7),
        vcpu_second: dec!(0.0000276944),
        gb_second: dec!(0.0000036667),
        // $0.0001111111 per GB-hour x 730 hours. Was 0.08 — a plausible-looking
        // round number that understated every stored GB by 1.37%, which is the whole
        // argument for deriving it from the API figure rather than reading a page.
        storage_gb_month: dec!(0.0811111030),
        snapshot_read_gb: dec!(0.00155),
        snapshot_write_gb: dec!(0.0038),
        minimum_retention: MINIMUM_RETENTION,
    }
}
```

```python
PINNED_REGION = "us-east-1"
PINNED_RETRIEVED = "2026-08-07"
PINNED: dict[str, Decimal] = {
    "vcpu_second": Decimal("0.0000276944"),
    "gb_second": Decimal("0.0000036667"),
    # $0.0001111111 per GB-hour x 730 hours. Was 0.08 — a plausible-looking round
    # number that understated every stored GB by 1.37%, which is the whole argument
    # for deriving it from the API figure rather than reading a page.
    "storage_gb_month": Decimal("0.0811111030"),
    "snapshot_read_gb": Decimal("0.00155"),
    "snapshot_write_gb": Decimal("0.0038"),
}
```

**Assumptions consumers make:**

- The duplication is the point, not an oversight: "a drift check that imported the values it checks
  would compare a table against itself and pass by construction. Two independent readers is the same
  pattern this repo's harbor-harvest sibling uses for its tool/library twins — port a change to both,
  never unify them" (`scripts/check-live-rates:111-117`).
- `verify_twin` reads Rust **as text** rather than shelling out to `microvm cost --json`, and the
  reason is not laziness: the envelope carries *amounts*, not rates, so the CLI path would have to
  divide a line item by its quantity — which means a change in the cost arithmetic (baseline swapped
  for peak, a retention floor applied where it was not) "would surface here as a rate that drifted.
  That is a true failure reported against the wrong file" (`scripts/check-live-rates:150-159`).
- The cost of that trade is accepted explicitly: a reformat of `pinned_rates()` breaks the read, and
  that failure is an exit 1 naming the field it could not find, never a silent pass
  (`scripts/check-live-rates:160-164`).
- `TWIN_FIELDS` is an explicit map even though the names are identical today, so a Rust-side rename is
  a failure that names the field rather than a comparison that quietly stops happening
  (`scripts/check-live-rates:135-144`).
- The twin check runs **first on every path**, including `--twin-only`: a pinned figure that disagrees
  with its twin is already wrong whatever the API says, and finding out before the fetch keeps a
  two-table problem from reading as a one-table one (`scripts/check-live-rates:605-616`).
- The drift tolerance is 0.5% and is the rounding in the pinned figures rather than a licence to
  drift — the pinned snapshot rates are three-significant-figure roundings of ten-digit API figures,
  so a tighter tolerance would report drift on a table that is correct, and a check that always fires
  is a check nobody reads (`scripts/check-live-rates:491-495`).
- `relative` has no zero guard on purpose: a pinned rate of zero would mean the table claims a
  billable line item is free, and a `ZeroDivisionError` is a better outcome than a drift report that
  quietly skipped it (`scripts/check-live-rates:519-522`).
- Every line is reported, drifted or not, because a check that printed only its findings could not be
  told apart from one whose credentials silently failed over to zero line items
  (`scripts/check-live-rates:502-506`).
- There is no `--region` flag: one pinned table exists and it is us-east-1, and a flag that fetched
  some other region and compared it against itself would pass by construction
  (`scripts/check-live-rates:587-589`).
- A missing ARM rate **raises** rather than substituting the x86 sibling, which is 17.9% higher: the
  hand-pinned table used the ARM figure "correctly but by luck", and a fallback "would overstate
  every estimate by 17.9% and look entirely healthy doing it"
  (`scripts/check-live-rates:16-21`, enforced at `:408-442`).

**Drift risk:** Five rates, confirmed by running `./scripts/check-live-rates --twin-only`
(`twin ok: 5 pinned rate(s) agree with microvms-core/src/cost.rs`, 2026-08-09). A sixth rate added to
`RateTable` would not be compared until it is added to `PINNED` and `TWIN_FIELDS` — the gate reports
the count it checked, so the number is the tell. Mitigation: the gate prints
`twin ok: N pinned rate(s)`, so an unchanged N after a table grows is visible in the log.

## The endpoint proxy header pair — two lanes, two definitions

**Producer:** `microvms-core/src/session/proxy.rs:51` and `:57`; **independently**
`microvms-core/src/control/microvm.rs:50` and `:57`

**Consumer(s):**

- `microvms-core/src/session/proxy.rs:403-419` — `headers_from`, which builds both on every request.
- `microvms-core/src/control/microvm.rs:265-266` — the control lane's own pairing.
- `microvms-core/src/session/mod.rs:42-43` — re-exported from the session module.
- `microvms-js/src/session.rs:431-432` and `microvms-py/src/session.rs:533-534` — both bindings
  publish the names to their callers.
- `microvms-core/tests/turmoil_client.rs:301-302`, `:974-981`, `:1435-1437` — asserted on recorded
  requests.

**Shape:**

```rust
/// The header carrying the minted JWE. One of the two keys read out of the
/// `authToken` map.
pub const PROXY_AUTH_HEADER: &str = "X-aws-proxy-auth";

/// The header naming which of the token's allowed ports this request targets.
///
/// Sent on every request, never inferred. See the module docs for what its absence
/// looks like.
pub const PROXY_PORT_HEADER: &str = "X-aws-proxy-port";
```

**Assumptions consumers make:**

- Both headers go on every request. Omitting the port header is "a rejection that reads like a bad
  token, which is the worst available diagnostic: the header that is wrong is not the header the
  error mentions" (`microvms-core/src/session/proxy.rs:5-12`).
- `CreateMicrovmAuthToken` answers with `authToken` as a **map** of header name to value, not a bare
  string, because the API is shaped for schemes needing more than one header. Reading that map as a
  string is TRAP-7, and `ProxyToken` closes it by construction: it holds a map, exposes no `as_str`,
  has no `Display`, and the only way out names the header it reads
  (`microvms-core/src/session/proxy.rs:14-19`, type at `:76-79`).
- Header lookup is case-insensitive, because the map's keys come from a service response rather than
  from this crate: a client matching `X-aws-proxy-auth` exactly would break on a response spelling it
  lowercase, and the failure would look like a missing header rather than a missing key
  (`microvms-core/src/session/proxy.rs:102-114`).
- All the token's headers are forwarded, not just the two this client knows about, because the
  platform's stated reason for a map is that a scheme may need more than one header
  (`microvms-core/src/session/proxy.rs:116-125`).
- The port header is this client's to send — but if a control-plane response ever includes it, that
  value **wins**, because the service knows which ports it scoped the token to and this client only
  knows which one it was configured with (`microvms-core/src/session/proxy.rs:408-417`).
- Minting happens inside `ProxyAuth::headers`, which every request calls, because the service caps a
  proxy token at sixty minutes — shorter than a long agent run — and the resulting rejection is
  indistinguishable from a daemon that died (`microvms-core/src/session/proxy.rs:21-27`). Refresh is
  at *half* the ceiling, not just under it: refreshing at fifty-nine minutes puts the expiry inside
  the window between building the headers and the proxy validating them
  (`microvms-core/src/session/proxy.rs:29-33`).

**Drift risk:** The two definitions can diverge without a compile error. What makes the duplication
safe is one conversion test that goes through the *other* module's spelling, so if either lane
changed a name the port assertion fails (`microvms-core/src/session/proxy.rs:866-887`) — the stated
trade being "cheaper than merging the constants, which would mean one lane editing the other's file"
(`:859-865`). Mitigation: keep that conversion test; it is the whole of the coupling.

## `spec/core.symspec.json` ↔ `sandbox.rs` ↔ `model/src/client.rs` — a three-way mirror

**Producer:** `spec/core.symspec.json` `stateModel` — five variables, `vm_state` over six states,
`bootstrap_count` an int bounded 0..3

**Consumer(s):**

- `microvms-core/src/sandbox.rs:96-110` — `Lifecycle`, "the symspec's `vm_state`, verbatim" (`:91`),
  with the other four variables as private fields beside it (`:11-17`, accessors `:431-451`).
- `model/src/client.rs:61-74` — `VmState`, the stateright model's copy.
- `model/src/client.rs:142-147` — the symspec's five variables re-declared as model state.
- `model/src/lib.rs:74-81` — `ExecPhase`, which mirrors `protocol::exec::Phase`
  (called out at `protocol/src/exec.rs:16`).
- `mise.toml:137-155` — the `spec:core` task, Z3 over the symspec document.

**Shape:**

```json
{
 "initial": "vm_state = PENDING and token_installed = false and image_exists = false and was_terminated = false and bootstrap_count = 0",
 "variables": [
  {
   "domain": ["PENDING", "RUNNING", "SUSPENDING", "SUSPENDED", "TERMINATING", "TERMINATED"],
   "frame": "stable",
   "initial": "vm_state = PENDING",
   "name": "vm_state",
   "type": "enum"
  },
  { "frame": "stable", "initial": "token_installed = false", "name": "token_installed", "type": "bool" },
  { "frame": "stable", "initial": "image_exists = false", "name": "image_exists", "type": "bool" },
  { "frame": "stable", "initial": "was_terminated = false", "name": "was_terminated", "type": "bool" },
  { "domain": { "max": 3, "min": 0 }, "frame": "stable", "initial": "bootstrap_count = 0", "name": "bootstrap_count", "type": "int" }
 ]
}
```

**Assumptions consumers make:**

- `model/src/client.rs` mirrors `microvms_core::sandbox::Lifecycle` "by convention rather than by
  dependency — this crate has no cargo edge to `microvms-core`, exactly as it has none to `agentd`"
  (`model/src/client.rs:58-59`). Nothing compiles the mirror; it is a naming discipline.
- The Z3 proofs are only worth something if the transitions in `sandbox.rs` are the only way to move
  the state, which is why every one of the five fields is private and every mutation happens in one of
  five methods (`microvms-core/src/sandbox.rs:13-17`).
- Lifecycle is an enum, not a `String`, because a `String` would let `"RUNNING "` and `"Running"` both
  exist and every guard would have to decide which it meant
  (`microvms-core/src/sandbox.rs:93-95`).
- `sandbox.rs` is runtime-checked rather than typestate, and the reason is a binding constraint: a
  type whose Rust identity changes on every transition cannot be one `#[pyclass]`, so a typestate
  sandbox would be re-erased at the binding boundary and the check would exist twice with the
  binding's copy being the one most callers hit (`microvms-core/src/sandbox.rs:19-27`). What is kept
  from the idea is that the check happens **before** the wire call, and the test asserts the call
  count because that is the observable that distinguishes the two
  (`microvms-core/src/sandbox.rs:29-32`).
- The model counts **wire calls in the state** (`model/src/client.rs:82-95`), because a state-only
  property cannot say "this call should never have happened": a client that calls `ResumeMicrovm`,
  gets an error, and stays put satisfies every state-only property and burns a poll timeout
  (`model/src/client.rs:24-31`).
- Violations are recorded at the **transition**, where the pre-state is still in hand. Inferring from
  the post-state was tried and was wrong — a suspend from SUSPENDED and one from RUNNING both land in
  SUSPENDING, so nothing in the result tells them apart (`model/src/client.rs:175-181`,
  `:517-526`).
- Every `always` property is paired with a `sometimes` witness, because a safety property over a
  state space that never reached the interesting state passes while measuring nothing
  (`model/src/client.rs:38-42`, witnesses at `:594-635`).
- Properties are stated **unconditionally** rather than consulting the config they are meant to
  discriminate, since a property that reads that flag becomes vacuous in the very run where it should
  fail (`model/src/client.rs:508-513`, same idiom at `model/src/lib.rs:433-439`).
- The resume window is a boolean, not a clock: what the model has to settle is whether the client
  *checks* before calling, and a counter would multiply the state space by the window's width to
  prove the same one bit (`model/src/client.rs:44-52`).

**Drift risk:** A sixth `Lifecycle` state added to `sandbox.rs` compiles, and neither the symspec
document nor `model/src/client.rs` notices — the mirror has no mechanical enforcement in either
direction. Mitigation: `mise run spec:core` and the `model` crate's tests are the two readers; run
both when touching the lifecycle, and note that `spec:core` is deliberately outside `check`'s
dependency list because it needs a node path in someone's home directory (`mise.toml:150-154`).

## Other contracts

- **`agentd/src/config.rs` limits ↔ the published `limits` object** — `agentd/src/schema.rs:211-226`
  reads twelve fields off the running `Config`; a renamed field breaks the build, but a *new* cap
  added to `Config` and not to `limits` ships undiscoverable.
- **`microvms-core/src/constants.rs:176-209` `as_json()` ↔ `microvm constants --emit-json`** — the
  CLI prints the object verbatim, which is what makes the drift gate's source a function of the
  `pub const`s rather than a parse (`scripts/check-model-drift:296-305`).
- **`microvms-cli/src/exit.rs` `EXIT_TABLE` ↔ the process exit code ↔ the manifest** — three readers
  of one table; CLI-3 is the claim that the first two agree, cross-checked at
  `conformance/run_rs.py:281-298`.
- **`microvms-core/src/error.rs:219-256` `WireKind` ↔ `data.kind` strings** — the daemon status names
  travel as `wire.as_str()` (`microvms-cli/src/envelope.rs:330`) and the oracle asserts on the
  string, not the enum (`conformance/run_rs.py:186-191`).
- **`protocol::hook::HOOK_PREFIX` ↔ the platform's fixed path** — `/aws/lambda-microvms/runtime/v1`
  is the service's, not ours (`protocol/src/hook.rs:12-13`); the daemon publishes it as
  `hook_prefix` (`agentd/src/schema.rs:294`) so a consumer knows which paths never to call.
- **`RunHookEnvelope`'s camelCase key ↔ the platform's wrapper** — `runHookPayload`, not
  `run_hook_payload`; the wrong spelling terminates the VM with a 400 before any traffic is forwarded
  (`protocol/src/hook.rs:19-23`), pinned at `agentd/src/schema.rs:842-850`.
- **`scripts/check-live-rates:85-91` `MICROVM_REGIONS` ↔ `microvms-core`'s** — a literal copy used
  only to *write* an error, never to refuse, so the cost of it being stale is one misleading
  sentence; `scripts/check-model-drift` is what holds it equal (`scripts/check-live-rates:79-84`).
- **`HOURS_PER_MONTH` = 730, in two places** — `microvms-core/src/cost.rs` and
  `scripts/check-live-rates:98-101`; "two conventions for the same month is how the pinned table came
  to be 1.37% low" (`scripts/check-live-rates:30-33`).
- **`BASE_IMAGE_REFS` ↔ the Dockerfile `FROM` ↔ `baseImageArn`** — the pairing is a map rather than
  two loose literals because `microvms-core` refuses a Dockerfile whose `FROM` disagrees with the
  create call's `baseImageArn` (`conformance/run_rs.py:139-153`).
- **`conformance/run_rs.py` check names ↔ the deleted Python oracle's** — the 75 names are
  byte-identical to `conformance/run.py`'s on purpose, so the report diffs line for line against the
  last recorded oracle run in git history (`conformance/run_rs.py:18-22`).

## See also

- [microvms-agentd · Impact analysis](impact-analysis.md)
- [microvms-agentd · Debugging guide](debugging-guide.md)
- [microvms-agentd · Tech debt](tech-debt.md)
- [microvms-agentd · Data flow](../architecture/data-flow.md)
- [microvms-agentd · Module map](../architecture/module-map.md)

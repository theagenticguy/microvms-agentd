# microvms-agentd · Public API

Four crates carry a public surface. `microvms-core` is the client library: the control plane, the in-VM daemon client, the cost engine, and every trap closure in one crate (`microvms-core/src/lib.rs:2-3`), exposing nine public modules and re-exporting `protocol` so consumers name wire types through core rather than depending on `protocol` directly (`microvms-core/src/lib.rs:65-82`). `protocol` states the wire contract as types, shared by the daemon and every client. `microvms-py` and `microvms-js` are thin bindings that hold no validation of their own — every refusal a caller sees is raised by the core, with the core's message naming the `docs/PLATFORM.md` finding behind it (`microvms-py/src/lib.rs:12-18`).

`microvms-cli` is not part of this surface. It declares exactly one `[[bin]]` and no `src/lib.rs`, so it exports nothing a binding could depend on, and `tests/dependency_direction.rs` fails if a lib target ever appears (`microvms-cli/Cargo.toml:10-20`). Its commands are documented in `reference/cli.md`.

Thirty symbols are listed, ranked by inbound reference count within each surface. Seven public names fall below the cut and are not documented here: `CreateImageRequest` (`microvms-core/src/control/mod.rs:275`), `RunMicrovmRequest` (`microvms-core/src/control/mod.rs:389`), `run_report` (`microvms-core/src/cost.rs:1778`), `estimate_run` (`microvms-core/src/cost.rs:1900`), `RunRequest` (`microvms-core/src/sandbox.rs:146`), `Lifecycle` (`microvms-core/src/sandbox.rs:97`), and `TeardownReport` (`microvms-core/src/sandbox.rs:335`).

## microvms-core

### Error

```rs
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Error {
```

A failure classified once at the point it is raised, deliberately a struct with a private body rather than an enum, because an enum over every raise site would make each new failure a breaking change for a binding that matched exhaustively.

`microvms-core/src/error.rs:41-43`

### Region

```rs
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Region {
```

An AWS region, closed over the five that run MicroVMs plus a named escape hatch, so a typo'd region is a compile error rather than an `AccessDeniedException` carrying a null message.

`microvms-core/src/region.rs:44-45`

### ErrorKind

```rs
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ErrorKind {
```

The coarse failure classes, one per non-zero row of the CLI's exit table, with the integer exit code left to the CLI because a library owning process exit codes would be a library with an opinion about being a process.

`microvms-core/src/error.rs:126-127`

### SizeClass

```rs
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SizeClass {
    Mib512,
    Mib1024,
    Mib2048,
    Mib4096,
    Mib8192,
}
```

The five documented size classes, named for the baseline a caller writes into `minimumMemoryInMiB` and deliberately not for the peak, since naming both would suggest the two are picked independently.

`microvms-core/src/sizing.rs:112-119`

### Session

```rs
pub struct Session {
```

The control API of one running MicroVM.

`microvms-core/src/session/mod.rs:184`

### ControlPlane

```rs
pub struct ControlPlane {
```

The control-plane client, holding its transport and clock behind `Arc` so a caller keeping one across tasks does not need a second credential chain.

`microvms-core/src/control/mod.rs:160`

### Sandbox

```rs
pub struct Sandbox {
```

One MicroVM's whole life: the state machine, the suspended window, and explicit teardown.

`microvms-core/src/sandbox.rs:422`

### WireKind

```rs
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WireKind {
```

The daemon-side failure classes the conformance suite asserts on, several of which collapse onto one `ErrorKind` at the exit code rather than at the raise site.

`microvms-core/src/error.rs:218-219`

### RunHookTimeout

```rs
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunHookTimeout(u32);
```

A timeout for the `run`, `resume`, `suspend`, or `terminate` hook, accepting 1..=60 seconds and offering no conversion from `BuildHookTimeout`.

`microvms-core/src/hooks.rs:47-48`

### Transport

```rs
pub struct Transport {
```

A backend, the agent token, and the proxy auth every request needs, kept separate from `Session` because `ExecHandle` needs it and holding a whole session would make the two mutually recursive.

`microvms-core/src/session/mod.rs:63`

### BuildHookTimeout

```rs
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BuildHookTimeout(u32);
```

A timeout for the `ready` or `validate` image-build hook, accepting 1..=3600 seconds, and a distinct type so a build-sized value cannot reach a field that caps at 60.

`microvms-core/src/hooks.rs:53-54`

### EstimatedUsd

```rs
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EstimatedUsd(Decimal);
```

Dollars derived from published rates and not the bill, with no `From<EstimatedUsd> for f64`, no `Into`, no `Deref`, and no `as_f64`, so laundering an estimate into a float does not compile.

`microvms-core/src/cost.rs:548-549`

### ExecHandle

```rs
pub struct ExecHandle {
```

One exec addressed by its caller-minted id, which is also the idempotency key, so rebuilding a handle with the same id after a process restart still addresses the same server-side exec.

`microvms-core/src/session/exec.rs:213`

### RateTable

```rs
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateTable {
```

The five us-east-1 rates, held privately so that pricing compute from the ARM rate is a property of the type rather than of a code path a caller can bypass.

`microvms-core/src/cost.rs:848-849`

### CostReport

```rs
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CostReport {
```

Per-phase cost attribution for one sandbox, measured or projected, holding the rate table it was computed against so it stays reproducible after `pinned_rates` is updated.

`microvms-core/src/cost.rs:1477-1478`

### ExecResult

```rs
#[derive(Debug)]
pub struct ExecResult {
    pub exec_id: String,
    pub phase: protocol::exec::Phase,
    /// `None` while running. Present once the child has exited.
    pub outcome: Option<protocol::exec::Outcome>,
}
```

An exec's phase and, once it has one, its outcome — a thin wrapper over the daemon's `PollResponse` rather than a re-modelling of it, so the two cannot disagree.

`microvms-core/src/session/exec.rs:66-72`

### Image

```rs
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Image {
```

A built image, and the log group the service created alongside it.

`microvms-core/src/control/image.rs:58-59`

## protocol

### protocol::exec::Phase

```rs
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
```

An exec's phase on the wire, with schemars reading the same `#[serde(...)]` attributes serde does so the published schema describes what the daemon actually emits.

`protocol/src/exec.rs:22-24`

### protocol::health::Health

```rs
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct Health {
```

The `GET /v1/health` response: daemon version, bootstrap state, disk pressure, whether startup identity repair degraded, and the exec-activity pair `busy` / `execs`.

`protocol/src/health.rs:10-11`

### protocol::exec::StartRequest

```rs
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct StartRequest {
```

The `POST /v1/exec/start` body, whose `command` field is either an argv array or, with `shell: true`, a single script string.

`protocol/src/exec.rs:103-104`

### protocol::exec::Outcome

```rs
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct Outcome {
```

Captured output and exit status of a finished exec.

`protocol/src/exec.rs:57-58`

### protocol::exec::PollResponse

```rs
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct PollResponse {
    pub exec_id: String,
    pub phase: Phase,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(flatten)]
    pub result: Option<Outcome>,
}
```

The `GET /v1/exec/{id}` body, which flattens the outcome into the response and omits it entirely while the exec is still running.

`protocol/src/exec.rs:229-236`

## microvms-py

The Python module is declared rather than assembled: a `#[pymodule] mod microvms` lists its members in `#[pymodule_export]` use statements, 26 classes and 7 functions, so the macro can see the whole membership and `maturin generate-stubs` emits the real surface instead of a `__getattr__` escape hatch (`microvms-py/src/lib.rs:110-129`). The exception hierarchy stays imperative in `#[pymodule_init]`, because `create_exception!` builds its types at runtime and leaves no introspection record for `#[pymodule_export]` to carry (`microvms-py/src/lib.rs:103-137`). Every method is sync, blocking on one shared multi-thread tokio runtime with `py.detach` first (`microvms-py/src/lib.rs:42-46`). The generated stub and its PEP 561 marker are committed as `microvms-py/microvms.pyi` and `microvms-py/py.typed`, and `mise run stubs:check` fails when the committed stub no longer matches the pyo3 surface (`mise.toml:179-181`).

### microvms-py Region

```rs
#[pyclass(frozen, from_py_object, name = "Region", module = "microvms")]
#[derive(Clone)]
pub struct PyRegion {
```

An AWS region, closed over the five that run MicroVMs plus a named escape hatch, exported to Python as `Region`.

`microvms-py/src/region.rs:30-32`

### microvms-py Sandbox

```rs
#[pyclass(frozen, name = "Sandbox", module = "microvms")]
pub struct PySandbox {
```

One MicroVM's whole life, with `build_image`, `run`, `suspend`, `resume`, and `terminate` as the five transitions and every state guard left in the core.

`microvms-py/src/sandbox.rs:266-267`

### microvms-py Session

```rs
#[pyclass(frozen, name = "Session", module = "microvms")]
pub struct PySession {
```

One running MicroVM's control API, with the proxy auth handled for you.

`microvms-py/src/session.rs:182-183`

### microvms-py EstimatedUsd

```rs
#[pyclass(
    frozen,
    skip_from_py_object,
    name = "EstimatedUsd",
    module = "microvms"
)]
#[derive(Clone, Copy)]
pub struct PyEstimatedUsd {
```

A dollar figure with no `__float__`, `__int__`, `__index__`, or `__add__`, whose `amount` answers a string, so `float(usd)` raises `TypeError` — the Python equivalent of the core's missing impl.

`microvms-py/src/cost.rs:169-176`

## microvms-js

The Node surface has no barrel: every `#[napi]` item in the crate is exported, and `index.d.ts` plus the `index.js` loader and the compiled `.node` addon are generated by `napi build` and excluded from the repository as one platform's build output (`.gitignore:27-29`). Two shapes appear side by side and mean different things: `#[napi]` on a struct is a JS class with methods, while `#[napi(object)]` is a copied plain object with no methods, which is how the same wire results that pyo3 renders as frozen classes arrive in Node (`microvms-js/src/exec.rs:65-66`, `microvms-js/src/session.rs:48-49`). Construction diverges from Python for a reason that is structural rather than stylistic: `PySandbox` has a `#[new]` constructor that blocks on the shared runtime (`microvms-py/src/sandbox.rs:310-316`), and a `#[napi(constructor)]` cannot be async, so the Node class is built through a static factory instead (`microvms-js/src/sandbox.rs:354-358`).

### microvms-js Region

```rs
#[napi]
#[derive(Clone)]
pub struct Region {
```

An AWS region, closed over the five that run MicroVMs plus a named escape hatch, taken as an instance rather than a string everywhere on this surface.

`microvms-js/src/region.rs:33-35`

### microvms-js Session

```rs
#[napi]
pub struct Session {
```

One running MicroVM's control API, with the proxy auth handled for you.

`microvms-js/src/session.rs:227-228`

### microvms-js Sandbox

```rs
#[napi]
pub struct Sandbox {
```

One MicroVM's whole life, with `buildImage`, `run`, `suspend`, `resume`, and `terminate` as the five transitions and every state guard left in the core.

`microvms-js/src/sandbox.rs:345-346`

### microvms-js ExecProcess

```rs
#[napi]
pub struct ExecProcess {
```

A long-running exec in the AI SDK's `SandboxProcess` shape, built by `Session.spawn` and never by a constructor, and the one entry on this surface with no peer in `microvms-py`.

`microvms-js/src/process.rs:192-193`

## HTTP

The daemon serves 18 routes. All of them come from one list, `surface_docs`, which `app` walks to build the router and `GET /v1/schema` walks to publish the document (`agentd/src/routes.rs:371-626`). A route cannot be served unless it appears in that list, and a listed route with no handler panics at startup rather than serving an undocumented surface (`agentd/src/routes.rs:110-140`). Each row also declares its auth, which is what splits the router in two: `Auth::Bearer` rows go behind the token guard, `Auth::Open` and `Auth::PlatformHook` rows do not (`agentd/src/routes.rs:51-59`).

The six lifecycle hooks sit under a prefix fixed by the service, `/aws/lambda-microvms/runtime/v1` (`protocol/src/hook.rs:15`). They are unauthenticated because the platform has no token to present, and a consumer must never call them.

### POST /aws/lambda-microvms/runtime/v1/ready

The image-build readiness probe, answering 200 even before bootstrap, because the question it answers is whether the daemon started.

`agentd/src/routes.rs:401-408`

### POST /aws/lambda-microvms/runtime/v1/resume

Acknowledged; the token, filesystem, exec records, and even backgrounded processes survive a suspend/resume cycle, but the guest's view of time jumps, so any timeout or lease held by a running command expires at once.

`agentd/src/routes.rs:446-455`

### POST /aws/lambda-microvms/runtime/v1/run

The one-shot token bootstrap and the optional launch environment beside it, both one JSON parse deeper than the request body inside `runHookPayload`, sharing the platform's 4096-byte payload budget.

`agentd/src/routes.rs:416-438`

### POST /aws/lambda-microvms/runtime/v1/suspend

Acknowledged and logged.

`agentd/src/routes.rs:439-445`

### POST /aws/lambda-microvms/runtime/v1/terminate

Acknowledged; begins graceful shutdown with in-flight requests draining.

`agentd/src/routes.rs:456-462`

### POST /aws/lambda-microvms/runtime/v1/validate

The image-build validation probe, on the same reasoning as `ready`.

`agentd/src/routes.rs:409-415`

### POST /v1/exec/start

Starts a command under a caller-minted `exec_id`, idempotent on that id, so a retry returns success without spawning a second child.

`agentd/src/routes.rs:463-474`

### GET /v1/exec/{id}

Polls status and output, read-only, so polling never mutates the entry and output survives until an explicit ack.

`agentd/src/routes.rs:475-485`

### POST /v1/exec/{id}/ack

Releases output and enters TTL collection; only acked entries are ever collected, so output nobody read is never destroyed.

`agentd/src/routes.rs:519-529`

### POST /v1/exec/{id}/kill

Sends SIGTERM then SIGKILL to the whole process group rather than the direct child alone, because a shell that backgrounded a server leaves the interesting process outside the child pid.

`agentd/src/routes.rs:530-541`

### POST /v1/exec/{id}/stdin

Writes to a child's stdin or signals EOF, a separate request from the output stream so a dropped attach does not cost the ability to feed the process.

`agentd/src/routes.rs:504-518`

### GET /v1/exec/{id}/stream

Follows output as Server-Sent Events from a byte offset, resumable with `?offset=N`; a body that ends without an `exit` event means the connection failed, not the command.

`agentd/src/routes.rs:486-503`

### GET /v1/fs/file

Reads one file, or a 1-based inclusive line range of it, always streamed — an `end_line` past the last line reads through EOF without error, and omitting both bounds returns the whole file byte-identically.

`agentd/src/routes.rs:542-557`

### PUT /v1/fs/file

Writes one file, deliberately not confined to a root, because the same token authorizes exec and a root prefix would add no security while breaking harnesses that write to home directories and `/etc`.

`agentd/src/routes.rs:558-570`

### GET /v1/fs/tar

Downloads a tree as tar, packing symlinks as symlinks, which is the producing half of what extraction accepts.

`agentd/src/routes.rs:571-582`

### PUT /v1/fs/tar

Uploads and extracts a tar under `?path=`, the one confined write path because member paths come from the archive rather than the caller, mirroring the CPython tarfile `data` filter.

`agentd/src/routes.rs:583-598`

### GET /v1/health

Reports liveness, daemon version, bootstrap completion, and whether any exec is still running; `busy` exists so an orchestrator outside the VM can hold it alive, since the platform measures idleness by inbound traffic through a proxy that terminates outside the guest.

`agentd/src/routes.rs:599-614`

### GET /v1/schema

Returns this document: every route, shape, status code, and operative limit.

`agentd/src/routes.rs:615-624`

## See also

- [contract map](../insights/contract-map.md) — 22 shared source citations
- [impact analysis](../insights/impact-analysis.md) — 21 shared source citations
- [business logic](../insights/business-logic.md) — 13 shared source citations
- [system overview](../architecture/system-overview.md) — 8 shared source citations
- [debugging guide](../insights/debugging-guide.md) — 8 shared source citations

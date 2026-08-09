# microvms-agentd · Public API

The public surface is four crates. `microvms-core` is the client library — the control plane, the in-VM session, the cost engine, and the trap closures, in one crate (`microvms-core/src/lib.rs:2`). `protocol` is the wire contract as types, shared by the daemon and every Rust client of it (`protocol/src/lib.rs:2`). `microvms-py` and `microvms-js` are thin bindings that add no validation of their own.

`microvms-cli` is deliberately not here: it declares exactly one `[[bin]]` and no `src/lib.rs`, so it exports nothing (`microvms-cli/Cargo.toml:11`). Its command surface is documented in `reference/cli.md`.

## microvms-core

### Region

```rs
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Region {
```

An AWS region, closed over the five that run MicroVMs plus a named escape hatch, so a typo'd region is a compile error rather than an `AccessDeniedException` with a null message.

`microvms-core/src/region.rs:44-45`

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

One of the five documented size classes, closed so an off-table `minimumMemoryInMiB` figure is not a value that exists.

`microvms-core/src/sizing.rs:112-119`

### Error

```rs
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Error {
```

The one error type every fallible call in the crate returns, carrying a coarse `ErrorKind`, an optional wire classification, and a message that names the `docs/PLATFORM.md` finding behind a local refusal.

`microvms-core/src/error.rs:41-43`

`ErrorKind` is the coarse classification `Error::kind` answers — thirteen failure classes, one per non-zero row of the CLI's exit table, with the integer exit code deliberately left to the CLI (`microvms-core/src/error.rs:127`).

### WireKind

```rs
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WireKind {
```

The daemon-side failure classes a client branches on, several of which collapse onto one `ErrorKind` at the exit code rather than at the raise site.

`microvms-core/src/error.rs:218-219`

### RunHookTimeout

```rs
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunHookTimeout(u32);
```

A timeout for the `run`, `resume`, `suspend`, or `terminate` hook: 1..=60 seconds, with no conversion from `BuildHookTimeout`.

`microvms-core/src/hooks.rs:47-48`

### BuildHookTimeout

```rs
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BuildHookTimeout(u32);
```

A timeout for the `ready` or `validate` image-build hook: 1..=3600 seconds, kept a separate type so a build-sized value cannot reach a field that caps at 60.

`microvms-core/src/hooks.rs:53-54`

## microvms-core::control

### ControlPlane

```rs
pub struct ControlPlane {
```

The control-plane client, holding its transport and clock behind `Arc` so a caller across tasks does not need a second credential chain.

`microvms-core/src/control/mod.rs:132`

### CreateImageRequest

```rs
#[derive(Clone, Debug)]
pub struct CreateImageRequest {
```

Everything `CreateMicrovmImage` needs, with the traps closed in the type — notably no `client_token` field at all.

`microvms-core/src/control/mod.rs:225-226`

Also re-exported from `control::` and not given their own entry: `Image`, a built image plus the log group the service created alongside it (`microvms-core/src/control/image.rs:58-59`); `RunMicrovmRequest` (`microvms-core/src/control/mod.rs:320`); `Microvm`, `ProxyToken`, `RunHookPayload`, `BaseImage`, `WaitOpts`, `ConnectorIntent`, and `build_artifact` (`microvms-core/src/control/mod.rs:58-61`).

## microvms-core::session

### Session

```rs
pub struct Session {
```

The control API of one running MicroVM.

`microvms-core/src/session/mod.rs:183`

### Session::run

```rs
    pub async fn run(&self, req: protocol::exec::StartRequest) -> Result<ExecHandle, Error> {
```

Starts a command and returns its handle without waiting, using the request's `exec_id` as the idempotency key.

`microvms-core/src/session/mod.rs:333`

### ExecHandle

```rs
pub struct ExecHandle {
```

One exec addressed by its caller-minted id, so a handle survives a process restart: rebuild it with the same id and every method still addresses the same server-side exec.

`microvms-core/src/session/exec.rs:189`

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

An exec's phase and, once it has one, its outcome — a thin wrapper over the daemon's `PollResponse` rather than a re-modelling of it.

`microvms-core/src/session/exec.rs:66-72`

## microvms-core::sandbox

### Sandbox

```rs
pub struct Sandbox {
```

One MicroVM's whole life, holding the state machine, the suspended window, and explicit teardown.

`microvms-core/src/sandbox.rs:353`

### Sandbox::build_image

```rs
    pub async fn build_image(&mut self, request: CreateImageRequest) -> Result<&Image, Error> {
```

Builds an image and waits for it to become usable, running every local guard before the call because the create happens after the caller's artifact upload.

`microvms-core/src/sandbox.rs:482`

### Sandbox::run

```rs
    pub async fn run(&mut self, request: RunRequest) -> Result<&mut Session, Error> {
```

Launches a MicroVM, waits for RUNNING, and hands back its session, refusing a second bootstrap with zero control-plane calls.

`microvms-core/src/sandbox.rs:521`

### Sandbox::terminate

```rs
    pub async fn terminate(&mut self, opts: TeardownOpts) -> TeardownReport {
```

Tears down best-effort and never returns an error, reporting what leaked instead.

`microvms-core/src/sandbox.rs:801`

### Lifecycle

```rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifecycle {
```

The sandbox's `vm_state` as six states and no others, so `"RUNNING "` and `"Running"` cannot both exist.

`microvms-core/src/sandbox.rs:96-97`

### RunRequest

```rs
#[derive(Clone, Debug)]
pub struct RunRequest {
```

Everything a launch needs, with the defaults the Python client measured and an optional `agent_token` for a caller minting its own.

`microvms-core/src/sandbox.rs:145-146`

### TeardownReport

```rs
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TeardownReport {
```

What teardown managed and what leaked, returned rather than raised because it runs where a `finally` would.

`microvms-core/src/sandbox.rs:289-290`

`TeardownOpts` selects which resources beyond the VM `terminate` should delete and whether to wait for TERMINATED (`microvms-core/src/sandbox.rs:228`).

## microvms-core::cost

### RateTable

```rs
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateTable {
```

us-east-1 rates pinned to when they were read and where from, with the five rate fields private so compute is priced from the ARM figure by construction.

`microvms-core/src/cost.rs:846-847`

### CostReport

```rs
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CostReport {
```

Per-phase attribution for one sandbox, measured or projected, holding the rate table it was computed against so it stays reproducible after `pinned_rates` is updated.

`microvms-core/src/cost.rs:1475-1476`

### run_report

```rs
pub fn run_report(
    size: SizeClass,
    usage: &RunUsage,
    rates: &RateTable,
    today: CalendarDate,
    label: impl Into<String>,
) -> Result<CostReport, Error> {
```

Per-phase attribution for one sandbox's lifecycle, taking `today` as a parameter rather than reading a clock so a report is a pure function of its inputs.

`microvms-core/src/cost.rs:1776-1782`

### estimate_run

```rs
pub fn estimate_run(
    size: SizeClass,
    plan: &PlanUsage,
    rates: &RateTable,
    today: CalendarDate,
    label: impl Into<String>,
) -> Result<CostReport, Error> {
```

What a plan will cost before spending anything, marking every duration `Provenance::Projected` so the report can never claim to be measured.

`microvms-core/src/cost.rs:1898-1904`

## protocol

### PROTOCOL_VERSION

```rs
pub const PROTOCOL_VERSION: &str = "1";
```

The protocol version, tracking the `/v1/` path namespace rather than the crate version, so a daemon patch does not look like an incompatible upgrade.

`protocol/src/lib.rs:58`

### protocol::exec::StartRequest

```rs
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct StartRequest {
```

The `POST /v1/exec/start` body, where `command` is either an argv array or, with `shell: true`, a single script string.

`protocol/src/exec.rs:64-65`

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

The `GET /v1/exec/{id}` body, flattening the outcome into the response and omitting it entirely while the exec is still running.

`protocol/src/exec.rs:190-197`

### protocol::health::Health

```rs
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct Health {
```

The `GET /v1/health` response: daemon version, bootstrap state, disk pressure, and whether startup identity repair degraded.

`protocol/src/health.rs:10-11`

### protocol::hook::RunHookEnvelope

```rs
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct RunHookEnvelope {
    #[serde(rename = "runHookPayload")]
    pub run_hook_payload: Option<String>,
}
```

The envelope the platform posts to the run hook, whose single string field means the caller's own JSON is one `serde_json` parse deeper than the request body.

`protocol/src/hook.rs:24-28`

Also public and not given their own entry: `VERSION_HEADER`, the `microvms-agentd-version` response header on every response including errors (`protocol/src/lib.rs:66`); `protocol::exec::StreamKind`, `OutputEvent`, `GapEvent`, `ExitEvent`, `StdinRequest`, `StdinResponse`, `KillResponse`, `ErrorBody`, and the ten `ERROR_*` slugs a client branches on (`protocol/src/exec.rs:227-247`).

## Bindings

### microvms-py Sandbox

```rs
#[pyclass(frozen, name = "Sandbox", module = "microvms")]
pub struct PySandbox {
```

The Python-facing sandbox, exposing `build_image`, `run`, `suspend`, `resume`, and `terminate` as sync methods over one shared tokio runtime, with every state guard left in the core.

`microvms-py/src/sandbox.rs:266-267`

### microvms-js Sandbox

```ts
export declare class Sandbox {
```

The Node-facing sandbox, built through the static async factory `create(region: Region): Promise<Sandbox>` because credential resolution cannot happen in a napi constructor, and taking a `Region` instance rather than a string.

`microvms-js/index.d.ts:366`

## HTTP

The daemon serves 18 routes, assembled by walking one list (`agentd/src/routes.rs:332`) that both the router and `GET /v1/schema` read. A route cannot be served unless it appears in that list, and a listed route with no handler panics at startup (`agentd/src/routes.rs:31-35`).

The six lifecycle hooks live under `HOOK_PREFIX` = `/aws/lambda-microvms/runtime/v1` (`protocol/src/hook.rs:13`), are unauthenticated because the platform has no token to present, and must never be called by a consumer. Every `/v1/` route except `health` and `schema` requires the bearer agent token.

### POST /aws/lambda-microvms/runtime/v1/ready

Image-build readiness probe, answering 200 even before bootstrap because the question is whether the daemon started.

`agentd/src/routes.rs:113`

### POST /aws/lambda-microvms/runtime/v1/resume

Acknowledged; the token, filesystem, exec records, and backgrounded processes survive a suspend/resume cycle, but the guest's view of time jumps so any held timeout or lease expires at once.

`agentd/src/routes.rs:117`

### POST /aws/lambda-microvms/runtime/v1/run

One-shot token bootstrap, where `agent_token` sits one JSON parse deeper than the request body inside `runHookPayload`.

`agentd/src/routes.rs:115`

### POST /aws/lambda-microvms/runtime/v1/suspend

Acknowledged and logged.

`agentd/src/routes.rs:116`

### POST /aws/lambda-microvms/runtime/v1/terminate

Acknowledged, beginning graceful shutdown with in-flight requests draining.

`agentd/src/routes.rs:118`

### POST /aws/lambda-microvms/runtime/v1/validate

Image-build validation probe, on the same reasoning as `ready`.

`agentd/src/routes.rs:114`

### POST /v1/exec/start

Start a command under a caller-minted `exec_id`, idempotent on that id so a retry returns success without spawning a second child.

`agentd/src/routes.rs:119`

### GET /v1/exec/{id}

Poll status and output, read-only: polling never mutates the entry and output survives until an explicit ack.

`agentd/src/routes.rs:120`

### POST /v1/exec/{id}/ack

Release output and enter TTL collection, so only acked entries are ever collected and output nobody read is never destroyed.

`agentd/src/routes.rs:127`

### POST /v1/exec/{id}/kill

SIGTERM then SIGKILL to the whole process group rather than just the direct child.

`agentd/src/routes.rs:128`

### POST /v1/exec/{id}/stdin

Write to a child's stdin or signal EOF, kept a separate request from the output stream so a dropped attach does not cost the ability to feed the process.

`agentd/src/routes.rs:126`

### GET /v1/exec/{id}/stream

Follow output as Server-Sent Events from a byte offset, emitting `output`, `gap`, and a terminal `exit` event so a body that closes without one means the connection failed rather than the command finishing.

`agentd/src/routes.rs:125`

### GET /v1/fs/file

Read one file, streamed as `application/octet-stream`.

`agentd/src/routes.rs:132`

### PUT /v1/fs/file

Write one file, deliberately not confined to a root because the same token authorizes exec.

`agentd/src/routes.rs:133`

### GET /v1/fs/tar

Download a tree as uncompressed tar, packing symlinks as symlinks.

`agentd/src/routes.rs:134`

### PUT /v1/fs/tar

Upload and extract a tar under `?path=`, the one confined write path, mirroring the CPython tarfile `data` filter because member paths come from the archive rather than the caller.

`agentd/src/routes.rs:135`

### GET /v1/health

Liveness, daemon version, and whether bootstrap has completed. Unauthenticated.

`agentd/src/routes.rs:136`

### GET /v1/schema

Every route, shape, status code, and operative limit as one document. Unauthenticated, so a protocol-version mismatch stays diagnosable.

`agentd/src/routes.rs:137`

## See also

- [microvms-agentd · Impact analysis](../insights/impact-analysis.md)
- [microvms-agentd · Business logic](../insights/business-logic.md)
- [microvms-agentd · Contract map](../insights/contract-map.md)
- [microvms-agentd · System overview](../architecture/system-overview.md)
- [microvms-agentd · Debugging guide](../insights/debugging-guide.md)

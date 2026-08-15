# microvms-agentd · Public API

The public surface is four crates. `microvms-core` is the client library. It contains the control plane, the in-VM session, the cost engine, and the trap closures in one crate (`microvms-core/src/lib.rs:2`). `protocol` expresses the wire contract as types, and both the daemon and every Rust client share it (`protocol/src/lib.rs:2`). `microvms-py` and `microvms-js` are thin bindings that add no validation of their own.

`microvms-cli` is not part of the public API. It declares exactly one `[[bin]]` and no `src/lib.rs`, so it exports nothing (`microvms-cli/Cargo.toml:11`). Its command surface is documented in `reference/cli.md`.

## microvms-core

### Region

```rs
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Region {
```

`Region` names an AWS region. The enum is closed over the five regions that run MicroVMs, plus a named escape hatch. Because the set is closed, a typo'd region is a compile error rather than an `AccessDeniedException` with a null message.

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

`SizeClass` is one of the five documented size classes. The enum is closed, so an off-table `minimumMemoryInMiB` figure cannot be represented.

`microvms-core/src/sizing.rs:112-119`

### Error

```rs
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Error {
```

`Error` is the one error type every fallible call in the crate returns. It carries a coarse `ErrorKind`, an optional wire classification, and a message. When a call fails a local guard, the message names the `docs/PLATFORM.md` finding behind that guard.

`microvms-core/src/error.rs:41-43`

`ErrorKind` is the coarse classification that `Error::kind` returns. It has thirteen failure classes, one per non-zero row of the CLI's exit table. The mapping to an integer exit code is left to the CLI (`microvms-core/src/error.rs:127`).

### WireKind

```rs
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WireKind {
```

`WireKind` lists the daemon-side failure classes a client branches on. Several of them collapse onto one `ErrorKind` at the exit code rather than at the raise site.

`microvms-core/src/error.rs:218-219`

### RunHookTimeout

```rs
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunHookTimeout(u32);
```

`RunHookTimeout` is a timeout for the `run`, `resume`, `suspend`, or `terminate` hook. It accepts 1..=60 seconds and provides no conversion from `BuildHookTimeout`.

`microvms-core/src/hooks.rs:47-48`

### BuildHookTimeout

```rs
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BuildHookTimeout(u32);
```

`BuildHookTimeout` is a timeout for the `ready` or `validate` image-build hook, and it accepts 1..=3600 seconds. It is a separate type so a build-sized value cannot reach a field that caps at 60.

`microvms-core/src/hooks.rs:53-54`

## microvms-core::control

### ControlPlane

```rs
pub struct ControlPlane {
```

`ControlPlane` is the control-plane client. It holds its transport and clock behind `Arc`, so a caller sharing it across tasks does not need a second credential chain.

`microvms-core/src/control/mod.rs:132`

### CreateImageRequest

```rs
#[derive(Clone, Debug)]
pub struct CreateImageRequest {
```

`CreateImageRequest` holds everything `CreateMicrovmImage` needs. The type closes the known traps; for example, it has no `client_token` field at all.

`microvms-core/src/control/mod.rs:225-226`

Also re-exported from `control::` and not given their own entry: `Image`, a built image plus the log group the service created alongside it (`microvms-core/src/control/image.rs:58-59`); `RunMicrovmRequest` (`microvms-core/src/control/mod.rs:320`); `Microvm`, `ProxyToken`, `RunHookPayload`, `BaseImage`, `WaitOpts`, `ConnectorIntent`, and `build_artifact` (`microvms-core/src/control/mod.rs:58-61`).

## microvms-core::session

### Session

```rs
pub struct Session {
```

`Session` is the control API of one running MicroVM.

`microvms-core/src/session/mod.rs:183`

### Session::run

```rs
    pub async fn run(&self, req: protocol::exec::StartRequest) -> Result<ExecHandle, Error> {
```

`Session::run` starts a command and returns its handle without waiting. It uses the request's `exec_id` as the idempotency key.

`microvms-core/src/session/mod.rs:333`

### ExecHandle

```rs
pub struct ExecHandle {
```

`ExecHandle` addresses one exec by its caller-minted id. Because the id comes from the caller, a handle survives a process restart. Rebuild it with the same id and every method still addresses the same server-side exec.

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

`ExecResult` holds an exec's phase and, once the child has exited, its outcome. It is a thin wrapper over the daemon's `PollResponse` rather than a re-modelling of it.

`microvms-core/src/session/exec.rs:66-72`

## microvms-core::sandbox

### Sandbox

```rs
pub struct Sandbox {
```

`Sandbox` manages one MicroVM's whole life. It holds the state machine, the suspended window, and explicit teardown.

`microvms-core/src/sandbox.rs:353`

### Sandbox::build_image

```rs
    pub async fn build_image(&mut self, request: CreateImageRequest) -> Result<&Image, Error> {
```

`build_image` builds an image and waits for it to become usable. It runs every local guard before the call, because the create happens after the caller's artifact upload.

`microvms-core/src/sandbox.rs:482`

### Sandbox::run

```rs
    pub async fn run(&mut self, request: RunRequest) -> Result<&mut Session, Error> {
```

`Sandbox::run` launches a MicroVM, waits for RUNNING, and hands back its session. If the sandbox has already bootstrapped, the call fails locally, without making any control-plane calls.

`microvms-core/src/sandbox.rs:521`

### Sandbox::terminate

```rs
    pub async fn terminate(&mut self, opts: TeardownOpts) -> TeardownReport {
```

`terminate` tears down best-effort. It always returns a report rather than an error, and the report lists what leaked.

`microvms-core/src/sandbox.rs:801`

### Lifecycle

```rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifecycle {
```

`Lifecycle` models the sandbox's `vm_state` as six states and no others, so variant strings like `"RUNNING "` and `"Running"` cannot both exist.

`microvms-core/src/sandbox.rs:96-97`

### RunRequest

```rs
#[derive(Clone, Debug)]
pub struct RunRequest {
```

`RunRequest` holds everything a launch needs. Its defaults are the values the Python client measured, and it has an optional `agent_token` field for a caller that mints its own token.

`launch_env` is a `HashMap<String, String>` delivered in the same `runHookPayload` as the token, and the daemon applies it as the base environment of every exec in that VM, under each request's own `env`. Empty by default, and an empty map produces byte-for-byte the payload this client always sent, so a caller who never touches the field cannot be affected by it existing. Build it one pair at a time with `with_launch_env(key, value)`. It shares the token's 4096-byte payload budget, and `RunHookPayload::for_launch` refuses an over-ceiling payload before any control-plane call, naming the byte count and how much of it the env accounted for.

`microvms-core/src/sandbox.rs:145-146`

### TeardownReport

```rs
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TeardownReport {
```

`TeardownReport` records what teardown deleted and what leaked. It is returned rather than raised because teardown runs where a `finally` would.

`microvms-core/src/sandbox.rs:289-290`

`TeardownOpts` selects which resources beyond the VM `terminate` should delete and whether to wait for TERMINATED (`microvms-core/src/sandbox.rs:228`).

## microvms-core::cost

### RateTable

```rs
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateTable {
```

`RateTable` holds us-east-1 rates, pinned to the date and source they were read from. Its five rate fields are private, which forces compute to be priced from the ARM figure.

`microvms-core/src/cost.rs:846-847`

### CostReport

```rs
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CostReport {
```

`CostReport` gives per-phase cost attribution for one sandbox, either measured or projected. It holds the rate table it was computed against, so it stays reproducible after `pinned_rates` is updated.

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

`run_report` computes per-phase cost attribution for one sandbox's lifecycle. It takes `today` as a parameter rather than reading a clock, so a report is a pure function of its inputs.

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

`estimate_run` computes what a plan will cost before anything is spent. It marks every duration `Provenance::Projected`, so the report cannot be mistaken for a measured one.

`microvms-core/src/cost.rs:1898-1904`

## protocol

### PROTOCOL_VERSION

```rs
pub const PROTOCOL_VERSION: &str = "1";
```

`PROTOCOL_VERSION` is the protocol version. It tracks the `/v1/` path namespace rather than the crate version, so a daemon patch does not look like an incompatible upgrade.

`protocol/src/lib.rs:58`

### protocol::exec::StartRequest

```rs
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct StartRequest {
```

`StartRequest` is the `POST /v1/exec/start` body. Its `command` field is either an argv array or, with `shell: true`, a single script string.

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

`PollResponse` is the `GET /v1/exec/{id}` body. It flattens the outcome into the response and omits it entirely while the exec is still running.

`protocol/src/exec.rs:190-197`

### protocol::health::Health

```rs
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct Health {
```

`Health` is the `GET /v1/health` response. It reports the daemon version, bootstrap state, disk pressure, whether startup identity repair degraded, and the exec-activity pair `busy` / `execs`.

`busy` and `execs` exist for an orchestrator *outside* the VM: the platform measures idleness by inbound traffic through the endpoint proxy, which terminates outside the guest, so a request from inside the guest cannot reset the idle timer and the orchestrator's own poll is the only traffic that counts. `busy` is true only while some exec is actually running, so an exited-but-unacked exec reads false; `execs` counts every registered entry in any phase. Both are `#[serde(default)]`, unlike every other field, because a client routinely talks to a daemon baked into an older image and a required field would make a health call fail outright against one.

`protocol/src/health.rs:10-11`

### protocol::hook::RunHookEnvelope

```rs
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct RunHookEnvelope {
    #[serde(rename = "runHookPayload")]
    pub run_hook_payload: Option<String>,
}
```

`RunHookEnvelope` is the envelope the platform posts to the run hook. Its single field is a string, so the caller's own JSON sits one `serde_json` parse deeper than the request body.

`protocol/src/hook.rs:24-28`

### protocol::hook::RunHook

```rs
#[derive(Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct RunHook {
    pub agent_token: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
}
```

`RunHook` is the caller's own payload, one parse inside `RunHookEnvelope`. `env` is optional and becomes the base environment of every later exec, under each request's own `env`.

Read it with `RunHook::parse`, not with `serde_json::from_str`. The function walks the payload by hand so that every refusal is one of `RunHookError`'s named variants, and the reason is the trust contract rather than tidiness: serde's own messages quote the value they rejected, and the value here is the agent token. Each variant names a key or a shape and never a value. `parse` also ignores unknown keys, because a 400 at this hook makes the platform terminate the VM before forwarding any traffic — a newer client's unrecognised field must not be a dead launch.

`protocol/src/hook.rs`

### protocol::fs::FileReadQuery

```rs
#[derive(Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct FileReadQuery {
    pub path: String,
    #[serde(default)]
    pub start_line: Option<u64>,
    #[serde(default)]
    pub end_line: Option<u64>,
}
```

`FileReadQuery` is `GET /v1/fs/file`'s query: `FsQuery`'s `path` plus an optional line range. A separate type from `FsQuery` because a line range means nothing on the write or tar routes and a shared type would publish it on all four; `mode` is absent here for the same reason pointing the other way.

Both bounds are 1-based and inclusive, matching the AI SDK harness's `readTextFile`. An `end_line` past the last line reads through EOF without an error rather than answering 416. `start_line=0` and `end_line < start_line` are 400.

`protocol/src/fs.rs`

Also public and not given their own entry: `VERSION_HEADER`, the `microvms-agentd-version` response header on every response including errors (`protocol/src/lib.rs:66`); `protocol::exec::StreamKind`, `OutputEvent`, `GapEvent`, `ExitEvent`, `StdinRequest`, `StdinResponse`, `KillResponse`, `ErrorBody`, and the ten `ERROR_*` slugs a client branches on (`protocol/src/exec.rs:227-247`); and `protocol::hook::RunHookError`, the typed refusal `RunHook::parse` answers with.

## Bindings

### microvms-py Sandbox

```rs
#[pyclass(frozen, name = "Sandbox", module = "microvms")]
pub struct PySandbox {
```

`PySandbox` is the Python-facing sandbox. It exposes `build_image`, `run`, `suspend`, `resume`, and `terminate` as sync methods over one shared tokio runtime. Every state guard stays in the core crate.

`microvms-py/src/sandbox.rs:266-267`

### microvms-js Sandbox

```ts
export declare class Sandbox {
```

`Sandbox` is the Node-facing sandbox. It is built through the static async factory `create(region: Region): Promise<Sandbox>`, because credential resolution cannot happen in a napi constructor. The factory takes a `Region` instance rather than a string.

`microvms-js/index.d.ts:366`

## HTTP

The daemon serves 18 routes, assembled by walking one list (`agentd/src/routes.rs:332`) that both the router and `GET /v1/schema` read. A route cannot be served unless it appears in that list, and a listed route with no handler panics at startup (`agentd/src/routes.rs:31-35`).

The six lifecycle hooks live under `HOOK_PREFIX` = `/aws/lambda-microvms/runtime/v1` (`protocol/src/hook.rs:13`). They are unauthenticated because the platform has no token to present, and a consumer must never call them. Every `/v1/` route except `health` and `schema` requires the bearer agent token.

### POST /aws/lambda-microvms/runtime/v1/ready

This is the image-build readiness probe. It answers 200 even before bootstrap, because the question it answers is whether the daemon started.

`agentd/src/routes.rs:113`

### POST /aws/lambda-microvms/runtime/v1/resume

The daemon acknowledges this hook. The token, filesystem, exec records, and backgrounded processes survive a suspend/resume cycle. The guest's view of time jumps, however, so any held timeout or lease expires at once.

`agentd/src/routes.rs:117`

### POST /aws/lambda-microvms/runtime/v1/run

This hook is the one-shot token bootstrap. The `agent_token` sits inside `runHookPayload`, one JSON parse deeper than the request body.

`agentd/src/routes.rs:115`

### POST /aws/lambda-microvms/runtime/v1/suspend

The daemon acknowledges and logs this hook.

`agentd/src/routes.rs:116`

### POST /aws/lambda-microvms/runtime/v1/terminate

The daemon acknowledges this hook and begins graceful shutdown, draining in-flight requests.

`agentd/src/routes.rs:118`

### POST /aws/lambda-microvms/runtime/v1/validate

This is the image-build validation probe. It behaves like `ready` and for the same reason.

`agentd/src/routes.rs:114`

### POST /v1/exec/start

Starts a command under a caller-minted `exec_id`. The route is idempotent on that id, so a retry returns success without spawning a second child.

`agentd/src/routes.rs:119`

### GET /v1/exec/{id}

Polls status and output. The route is read-only, so polling never mutates the entry, and output survives until an explicit ack.

`agentd/src/routes.rs:120`

### POST /v1/exec/{id}/ack

Releases output and moves the entry into TTL collection. Only acked entries are ever collected, so unread output is never destroyed.

`agentd/src/routes.rs:127`

### POST /v1/exec/{id}/kill

Sends SIGTERM then SIGKILL to the whole process group rather than just the direct child.

`agentd/src/routes.rs:128`

### POST /v1/exec/{id}/stdin

Writes to a child's stdin or signals EOF. It is a separate request from the output stream, so a dropped attach does not cost the ability to feed the process.

`agentd/src/routes.rs:126`

### GET /v1/exec/{id}/stream

Follows output as Server-Sent Events from a byte offset. The stream emits `output`, `gap`, and a terminal `exit` event. Because a finished command always ends with `exit`, a body that closes without one means the connection failed.

`agentd/src/routes.rs:125`

### GET /v1/fs/file

Reads one file, streamed as `application/octet-stream`.

`agentd/src/routes.rs:132`

### PUT /v1/fs/file

Writes one file. The path is not confined to a root, because the same token already authorizes exec.

`agentd/src/routes.rs:133`

### GET /v1/fs/tar

Downloads a tree as uncompressed tar, packing symlinks as symlinks.

`agentd/src/routes.rs:134`

### PUT /v1/fs/tar

Uploads and extracts a tar under `?path=`. This is the one confined write path, because member paths come from the archive rather than the caller. The confinement mirrors the CPython tarfile `data` filter.

`agentd/src/routes.rs:135`

### GET /v1/health

Reports liveness, the daemon version, and whether bootstrap has completed. The route is unauthenticated.

`agentd/src/routes.rs:136`

### GET /v1/schema

Returns every route, shape, status code, and operative limit as one document. The route is unauthenticated, so a protocol-version mismatch stays diagnosable.

`agentd/src/routes.rs:137`

## See also

- [microvms-agentd · Impact analysis](../insights/impact-analysis.md)
- [microvms-agentd · Business logic](../insights/business-logic.md)
- [microvms-agentd · Contract map](../insights/contract-map.md)
- [microvms-agentd · System overview](../architecture/system-overview.md)
- [microvms-agentd · Debugging guide](../insights/debugging-guide.md)

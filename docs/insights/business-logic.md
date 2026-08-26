# microvms-agentd · Business logic

## What counts as business logic here, and why the shape is unusual

There are no users, no orders, and no billing accounts in this codebase. The domain rules are
**trap closures**: rules that exist because someone measured AWS Lambda MicroVMs behaving in a
way that points away from its own cause, and paid for that measurement once so no caller pays
again. A rule here is a refusal that costs a second, standing in for a service answer that
costs a build cycle, a poll timeout, or an hour spent auditing a correct IAM policy.

**Scope.** Everything in this file is application-layer. There is no database, no ORM, and no
migration, so there are no DB-side invariants to surface. There is no UI, so there is no
form-validation layer. Request validation performed by AWS is out of scope *except* where this
client duplicates it deliberately — which it does in most places, for a measured reason: read
2026-08-07 out of botocore's `validate.py`, `VALIDATED_METADATA_ATTRS` is
`{'required', 'min', 'document', 'union'}`, so `max`, `pattern`, and `enum` violations are
serialized, sent, and answered with a `ValidationException`
(`microvms-core/src/constants.rs:11-23`). And this client never reaches that validator at all:
it signs with `aws-sigv4` and sends with `reqwest`, so **every** model constraint including
`min` is enforced by this crate or by nothing (`microvms-core/src/constants.rs:25-31`).
Deleting a local guard on the assumption that the SDK covers the model reopens the constraint
with no visible failure.

### The rules are formal requirements first, code second

`spec/core.symspec.json` carries 51 EARS requirements for `microvms-core`, every one at
`status: approved`, in six families:

| Family | Count | What it governs |
| --- | --- | --- |
| `TRAP-1` … `TRAP-13` | 13 | closures over platform behavior that misleads |
| `STATE-1` … `STATE-12` | 12 | the VM lifecycle |
| `COST-1` … `COST-10` | 10 | cost honesty |
| `CLI-1` … `CLI-6` | 6 | the binary's surface and its exit contract |
| `ARCH-1` … `ARCH-5` | 5 | crate boundaries |
| `BIND-1` … `BIND-5` | 5 | what a language binding may not weaken |

`spec/agentd.symspec.json` adds 6 requirements, all `status: draft`, covering the daemon's
bootstrap and control-token ladder — accept a control request whose token equals the installed
one, reject a differing one, reject any control request while none is installed, install on
first bootstrap, accept an identical replay, reject a different token.

The spec's `stateModel` names five variables: `vm_state` over
`PENDING / RUNNING / SUSPENDING / SUSPENDED / TERMINATING / TERMINATED`, plus
`token_installed`, `image_exists`, `was_terminated`, and `bootstrap_count` bounded `0..3`, all
`frame: stable`. Three lifecycle invariants over that model are proved in Z3 by
`mise run spec:core`, whose recorded run reports 3 constraints proved under hypotheses, 0
violated, 0 unknown (`mise.toml:224-226`) — that task needs a symspec v5 CLI at an absolute
path, so it is deliberately outside `mise run check` (`mise.toml:222-227`). The runnable half is
`stateright`, which restates the same three over every interleaving in
`model/src/client.rs:554-569` and passes under `cargo test -p agentd-model`. One waiver exists,
`GTWR_R6_MISSING_UNITS` against TRAP-5, because the linter's unit list does not include bytes.

### The strength ladder, and how to read the failure-mode column

Every closure is ranked, strongest first, at `microvms-core/src/lib.rs:21-40`:

- **S1, inexpressible** — the mistake cannot be written down: a closed enum, a newtype with no
  conversion, an absent parameter. An S1 closure cannot regress without a compile error.
- **S2, expressible but rejected** — the mistake can be written, and the client refuses it
  locally before any control-plane call, with an error naming the `docs/PLATFORM.md` finding.
  Weaker, because the guard is code that can regress, but the cost is seconds rather than a
  build cycle. Every boundary where a bare integer or string still has to be judged lands here.
- **S3, correct by default and overridable** — weakest, because it protects only the caller who
  accepts the default. An S3 closure must state what the override costs. There is exactly one.

Two conventions follow. **Every error message names its finding** rather than restating the
constraint, because the guards exist so a reader can reach the measurement
(`microvms-core/src/lib.rs:42-48`). **Every guard has a demonstrated way to fail** — a named
plausible edit that must turn a specific test red; "delete the feature and the test fails" does
not count (`microvms-core/src/lib.rs:50-57`).

## Validations

All row citations are the enforcement site. Where a rule is S1, the "failure mode" column says
what a caller cannot write rather than what they are told.

### Control-plane request shapes

Nine `require_*` functions in one module, each guarding a member the pinned service model
constrains and the SDK does not check.

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| `maximumDurationInSeconds` outside `1..=28800` is refused | Launch | `microvms-core/src/control/mod.rs:477-494`, constant at `microvms-core/src/constants.rs:263` | `ERR_INVALID_ARG` saying a longer session needs a second VM, not a larger number. 28800 is eight hours and the hard ceiling on any one VM's life |
| `idlePolicy.maxIdleDurationSeconds` under 60 is refused | Launch | `microvms-core/src/control/mod.rs:748-778`, constant at `microvms-core/src/constants.rs:241-258` | `ERR_INVALID_ARG`. The model states no maximum and the client adds none; the bound that ends a VM's life is `maximumDurationInSeconds` |
| A `Version` value that is empty, over 2048 characters, or carries whitespace anywhere is refused | Image build, Launch | `microvms-core/src/control/mod.rs:496-542`, constants at `microvms-core/src/constants.rs:121`, `:128` | Three separate `ERR_INVALID_ARG` messages. The pattern is `[^\s]+`, so a version pasted with a trailing newline satisfies "non-empty" and fails; the message names the character it found |
| A `NonBlankString` member (`codeArtifact.uri`, `baseImageArn`, `nameFilter`, `imageVersion`, `buildId`) that is empty, over 2048 characters, or carries whitespace is refused | Image build | `microvms-core/src/control/mod.rs:544-596`, constants at `microvms-core/src/constants.rs:141`, `:145` | `ERR_INVALID_ARG` naming the character. A blank `nameFilter` rides in the query string, where it either 400s or filters differently from what was meant |
| An identifier that is empty or over 256 characters is refused | Every operation | `microvms-core/src/control/mod.rs:598-651`, constants at `microvms-core/src/constants.rs:174`, `:183` | `ERR_INVALID_ARG`. An empty identifier is the case that pays for this guard: ten of these members are URI parameters, so an empty one collapses `/microvms/<id>` onto the listing and a `DELETE` on a collapsed path is worse |
| A `RoleArn` under 20 characters, over 2048, or off-pattern is refused | Image build, Launch | `microvms-core/src/control/mod.rs:653-708`, constants at `microvms-core/src/constants.rs:209`, `:212`, `:225` | Three messages. The short case says a value that short is almost always a role *name*; the pattern case names the twelve account digits |
| A port of 0 is refused; there is no ceiling branch | Image build, Session | `microvms-core/src/control/mod.rs:710-746`, constants at `microvms-core/src/constants.rs:232`, `:239` | `ERR_INVALID_ARG`. Zero means "let the kernel choose" to a listener and is not a port the platform can forward to. `PortNumber.max` equals `u16::MAX`, so a ceiling branch would be unreachable — pinned instead by `microvms-core/src/constants.rs:1244` |
| A tag key that is empty, over 128 characters, or off-pattern is refused; a tag value over 256 or off-pattern is refused | Image build | `microvms-core/src/control/mod.rs:780-846`, constants at `microvms-core/src/constants.rs:186`, `:189`, `:206` | `ERR_INVALID_ARG` naming the offending key. An empty tag *value* is legal and an empty key is not, and the two ceilings differ by 2x |
| An image name that is empty, over 64 characters, or outside `[a-zA-Z0-9-_]+` is refused | Image build | `microvms-core/src/control/mod.rs:848-878`, constants at `microvms-core/src/constants.rs:104`, `:112` | Three messages, because the pattern message ("no dots, no slashes") misleads for a 70-character name containing neither |
| More than 10 network connectors on a launch is refused | Networking | `microvms-core/src/control/microvm.rs:391-399`, constant at `microvms-core/src/constants.rs:290` | `ERR_INVALID_ARG`. The image-level egress list caps at **1**, not 10 (`microvms-core/src/constants.rs:292-304`), pinned by `microvms-core/src/constants.rs:897` |

`ControlPlane::run_microvm` runs the identifier, duration, idle-duration, version, and role-ARN
guards before it builds a wire body (`microvms-core/src/control/microvm.rs:356-374`).

### Image build and Dockerfile agreement

Five guards compare what this client sends against what the caller's Dockerfile declares. Every
one of them defends against the same failure shape: a build that succeeds, a daemon that logs
that it started, and an image that still lands in `CREATE_FAILED` naming nothing.

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| `inherit_workdir` against a base that declares no `WorkingDir` and a Dockerfile that sets none is refused | Image build | `microvms-core/src/control/artifact.rs:234-262` | `ERR_INVALID_ARG`. Measured 2026-08-05: `al2023-minimal`, `python:3.12-slim`, and `node:20-slim` all leave `WorkingDir` empty, so inheritance inherits `/` and every relative path resolves somewhere the caller did not mean |
| A Dockerfile whose `FROM` is not the selected base image's `docker_ref` is refused | Image build | `microvms-core/src/control/artifact.rs:524-548` | `ERR_INVALID_ARG`. The build runs the Dockerfile on top of `baseImageArn`, so a mismatch builds against a base none of the measured platform behavior describes |
| A Dockerfile whose `AGENTD_PORT` disagrees with the `hooks.port` this client sends is refused; an absent variable is checked against the daemon's own default of 9000 | Image build | `microvms-core/src/control/artifact.rs:305-348` | `ERR_INVALID_ARG`. Silence is not neutral: the daemon keeps its default for an unset variable, so the absent variable produces the failure for a caller who never typed a port |
| A Dockerfile `AGENTD_SSE_KEEPALIVE_SECS` at or above the client's stream idle timeout is refused | Image build | `microvms-core/src/control/artifact.rs:405-445` | `ERR_INVALID_ARG`. Equality is refused too, since an interval equal to the timeout races. The failure it prevents reports the client's own 60s as though it were the keepalive interval |
| A Dockerfile with no `CMD`, or with a non-empty `ENTRYPOINT`, is refused | Image build | `microvms-core/src/control/artifact.rs:483-522` | `ERR_INVALID_ARG`. Weak-form on purpose: it does not check that the `CMD` names a copied path. The unenforceable half — a base image starting its own process before bootstrap — stays with whoever builds the image |
| The daemon entry in the build artifact carries mode `0o755` explicitly | Image build | `microvms-core/src/control/artifact.rs:1-13`, `:36-39` | Structural. A non-executable binary produces an image whose `CMD` fails, and the symptom is a run-hook timeout that says nothing about permissions |
| The agent token has no path into the build artifact | Image build | `microvms-core/src/control/artifact.rs:15-23` | Unwritable (S1). `build_artifact` has no parameter that could carry one. The artifact becomes a shared image snapshot, so a per-VM secret in it is a secret shared with every VM; a test scans the produced zip's raw bytes rather than reviewing the API |

### Trap closures — control plane

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| TRAP-1: an image-create or run token is derived from a per-attempt nonce; there is no caller-supplied token parameter | Idempotency | `microvms-core/src/control/token.rs:101`, `:111`, `:120-148` | Unwritable (S1). The parameter does not exist. Minted at `microvms-core/src/control/image.rs:187-190` and `microvms-core/src/control/microvm.rs:415` |
| TRAP-1: the scope label is truncated at its **tail**, never its head, and the nonce is never truncated | Idempotency | `microvms-core/src/control/token.rs:55-71`, `:150` | Silent truncation of the label only. 64-byte scope plus an 8-byte hex nonce stays under the 128-character `clientToken` ceiling (`microvms-core/src/constants.rs:423`) |
| TRAP-2: an image in `CREATING` past the stall grace with builds listed, non-empty, and **every** build still `PENDING` fails the wait | Image build | `microvms-core/src/control/image.rs:318-324`, `:338-390` | `ERR_BUILD_WEDGED`, naming the `clientToken` replay signature. A `clientToken` is a permanent idempotency key, so a replayed create is a no-op: the image sits in `CREATING`, cannot be deleted, and its only version cannot be dropped. Two images were wedged this way for ~15 hours |
| TRAP-3: guest identity repair is a `bool` intent; the client injects the one accepted enum value `["ALL"]` | Image build | `microvms-core/src/control/image.rs:181-185`, `microvms-core/src/constants.rs:276-280` | Unwritable (S1). There is no capability list a caller can populate, and no way to ask for `CAP_SYS_ADMIN` alone |
| TRAP-4: a connector is an enumerated intent that derives a fully-qualified ARN for the request region | Networking | `microvms-core/src/control/connector.rs:39-47`, `:60-83` | Unwritable (S1). Two intents (`AllIngress`, `Egress`), no free-form string. `ConnectorIntent::ALL` at `:54` is the complete set a test can enumerate |
| TRAP-5: a `runHookPayload` over 4096 bytes is refused locally before any control-plane call | Launch | `microvms-core/src/control/microvm.rs:161-185`, constant at `microvms-core/src/constants.rs:83` | `ERR_INVALID_ARG` naming the service-model ceiling. Inclusive, measured 2026-08-07: 4096 passes, 4097 fails. Bytes, not characters. `docs/STRATEGY.md`, `docs/TRUST.md`, and the model's own documentation string all claim 16 KB (`microvms-core/src/constants.rs:97`), which is wrong by 4x in the dangerous direction — the shape `RunMicrovmRequestRunHookPayloadString` is the authority |
| TRAP-6: a region outside the five that carry MicroVMs is refused before the first control-plane call | Region | `microvms-core/src/region.rs:38-63`, `:137-164` | S1 for a held `Region`, S2 at the `FromStr` boundary. `ERR_INVALID_ARG` naming the null-message `AccessDeniedException` finding |
| TRAP-8: a VM reaching a state in `fail_on` before the wanted one fails the wait with state **and** `stateReason` attached | Launch | `microvms-core/src/control/microvm.rs:461-466`, `:482-500` | `ERR_LAUNCH_DIED`. Fails fast rather than polling to the deadline. Both facts, because either alone is unactionable: the state says the VM is gone, the reason is the only evidence that survives it |
| TRAP-10: a `minimumMemoryInMiB` that is not one of the five documented baselines is refused locally | Sizing | `microvms-core/src/sizing.rs:25-31`, `:146-161` | S1 for a held `SizeClass`, S2 at `from_baseline_mib`. Refused, never snapped to a neighbour: the two plausible service behaviors for 1500 differ in both the memory the guest gets and the rate it is billed at |
| TRAP-11: `CreateMicrovmShellAuthToken` is never called and `SHELL_INGRESS` is never requested | Networking | `microvms-core/src/control/connector.rs:16-28`, `microvms-core/src/control/mod.rs:27-31` | Unwritable (S1). No enum variant renders it and no method on `ControlPlane` calls it. The test counts the calls a full lifecycle makes rather than asserting a refusal |
| Two hook-timeout families cannot be interchanged: run/resume/suspend/terminate cap at 60s, ready/validate at 3600s | Hooks | `microvms-core/src/hooks.rs:56-82`, `:84-105`, constants at `microvms-core/src/constants.rs:268`, `:271` | Unwritable across families (S1) — no `From`, no shared trait. S2 within a family: `ERR_INVALID_ARG` naming **both** ceilings, because the caller who hits it nearly always picked a build-hook number |
| A hook port outside `1..=65535` is refused | Hooks | `microvms-core/src/hooks.rs:141-149` | `ERR_INVALID_ARG` naming the model range and version |
| An architecture other than `ARM_64` cannot be requested | Image build | `microvms-core/src/control/image.rs:168-172`, `microvms-core/src/constants.rs:282-287` | Unwritable (S1). The enum has one value, so the field is injected rather than accepted — a field could only ever express a request AWS rejects, after the upload |
| `ENABLED` on all six hooks is a typed enum value, not a `&str` literal | Image build | `microvms-core/src/control/mod.rs:880-909` | Compile error. The literal appeared six times with no constant naming either value, so a typo in one was a `ValidationException` on a call made after the artifact upload |

### Trap closures — in-VM session

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| TRAP-7: the proxy token is read out of the `authToken` **map**, never as a string | Session | `microvms-core/src/control/microvm.rs:268-283`, `:300-326`, `microvms-core/src/session/proxy.rs:14-19` | S1: `ProxyToken` exposes no `as_str`, no `Display`, no `Deref`; the auth value comes out through `auth_value()`, which names the header it reads (`microvms-core/src/session/proxy.rs:177`). A missing key is `WireKind::AuthTokenMint`, which is retryable |
| TRAP-7: every endpoint request sends **both** `X-aws-proxy-auth` and `X-aws-proxy-port` | Session | `microvms-core/src/control/microvm.rs:328-337`, `microvms-core/src/session/proxy.rs:428-434`, `:436-461` | Structural: `headers()` returns a two-element array. One without the other is rejected indistinguishably from a bad token, so the header that is wrong is not the header the error mentions |
| A WebSocket handshake carries the same two facts as three subprotocols, minted through the same cache | Session | `microvms-core/src/session/proxy.rs:39-52`, `:463-496` | Structural. The browser `WebSocket` constructor cannot set a header, so the platform moves both facts into `Sec-WebSocket-Protocol` and strips all three before forwarding. A second token path would be a second place TRAP-9 has to be got right |
| TRAP-9: the token is minted inside the request path, through exactly one mint function | Session | `microvms-core/src/session/proxy.rs:498-506`, `:508-560` | Structural: `headers`, `headers_for_port`, and `subprotocols` all reach the control plane through `token_for`. A cache miss is two conditions — stale, or out of scope for the requested port |
| A refresh interval at or above the 60-minute ceiling is refused at construction | Session | `microvms-core/src/session/proxy.rs:356-375`, constants at `:107`, `:109-111` | S2, `ERR_INVALID_ARG`. `DEFAULT_REFRESH_AFTER` is 30 minutes — **half** the ceiling, not just under it, because refreshing at fifty-nine minutes puts the expiry inside the window between building the headers and the proxy validating them |
| A mint asks for every port already cached plus the new one | Session | `microvms-core/src/session/proxy.rs:532-545` | A superset rather than a replacement. Measured 2026-08-15: a token minted for the agent port does not authorize 8080, and reusing it produces `403 Access to port denied` — whose WebSocket form is an unreasoned 1006 |

### Cost inputs

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| COST-1: every duration carries a `measured` or `projected` provenance label; there is no unlabelled constructor | Cost | `microvms-core/src/cost.rs:419-425` | Unwritable (S1). `DurationP` is an enum whose every variant names its provenance; no `From<Duration>`, no `Default`, both pinned by `compile_fail` doctests at `:395-408` |
| COST-2: an estimated dollar amount has no coercion to a bare float | Cost | `microvms-core/src/cost.rs:546-578` | Unwritable (S1). Private field, no `From`, no `Into<f64>`, no `Deref`. Three `compile_fail` doctests at `:519-545`, each pinning its own error code |
| COST-3: an unpriced quantity is a distinct `Unpriced` variant carrying a reason, never zero dollars | Cost | `microvms-core/src/cost.rs:614-625` | S1 by exhaustive `match`. Zero is a claim about the bill; unpriced is a claim about the documentation |
| COST-6: `gb_decimal` is the only place an `f64` becomes a `Decimal`, and it is fallible | Cost | `microvms-core/src/cost.rs:138-152` | `ERR_INVALID_ARG`. A negative size would render as a credit; `NaN`, an infinity, and a magnitude past 28 digits have no decimal reading. `EstimatedUsd::new` takes a `Decimal` so it cannot become a third boundary (`:553-561`) |
| COST-9: a rate catalog whose ARM compute line is missing is rejected rather than substituted | Cost | `microvms-core/src/cost.rs:1191-1199`, `:1200-1258`, `:1268-1302` | S1 for direct construction — the rate fields are private and there are exactly two doors. S2 at `from_catalog`, which refuses four ways: a missing ARM line whose x86 sibling is present, a missing line with no sibling, a restated unit, and two products where there was one. The ARM message names the x86 rate it will not substitute and the ~18% error that substituting would introduce |
| A calendar date arriving from outside the crate is validated against its month's real length | Cost | `microvms-core/src/cost.rs:232-241` | S2. `2026-02-30` would otherwise yield a day number for March 2nd and an age two days out |

### Daemon authorization

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| A control request whose token equals the installed agent token is accepted | Daemon auth | `agentd/src/state.rs:241-249`, `agentd/src/auth.rs:75-79` | Constant-time byte comparison via `subtle::ConstantTimeEq` (`agentd/src/auth.rs:28-33`) |
| A control request whose token differs is rejected | Daemon auth | `agentd/src/auth.rs:78` | 401 |
| A control request while no token is installed is rejected **distinguishably** from a bad credential | Daemon auth | `agentd/src/state.rs:241-249`, `agentd/src/auth.rs:73`, `:76` | 503, never 401 and never 404 — a client maps 404 onto "file not found", turning a protocol error into a phantom missing artifact |
| A bootstrap request presenting a token while none is installed installs it | Daemon auth | `agentd/src/state.rs:202-212`, `agentd/src/routes.rs:214-223` | 200 |
| A bootstrap request presenting a token **identical** to the installed one is accepted | Daemon auth | `agentd/src/state.rs:213-215`, `agentd/src/routes.rs:224-229` | 200. The platform may retry its own hook, and answering 409 would fail a launch that is fine |
| A bootstrap request presenting a **different** token is refused and changes nothing | Daemon auth | `agentd/src/state.rs:216-218`, `agentd/src/routes.rs:230-233` | 409, installed token unchanged |
| A malformed run-hook body, an absent `runHookPayload`, or a payload that does not parse are each refused | Daemon auth | `agentd/src/routes.rs:182-211` | 400, never 404. The token and the payload carrying it are never logged; the refusal is loggable because `RunHookError` names a key or a shape and never a value |
| The `Authorization` header is parsed and compared on **raw bytes**, never decoded as UTF-8 first | Daemon auth | `agentd/src/auth.rs:40-47` | No token extracted, or a mismatch. Never a crash — proved over arbitrary header bytes by `agentd/tests/proptest_tar.rs:867`, `:905`, `:931`, `:955` |
| Authorization is decided **before** the request body is polled, and a rejected body is drained under a cap | Daemon auth | `agentd/src/auth.rs:62-89`, cap at `agentd/src/config.rs:20-24` | An unauthenticated caller cannot make the daemon allocate. Draining lets a client's pooled connection survive an error response; draining without a cap is itself a denial-of-service vector |
| An unmatched path falls through to the 404 fallback rather than being answered 401 | Daemon auth | `agentd/src/routes.rs:61-76`, `:78-79` | 404. `route_layer` applies the guard only to matched routes: answering 401 for a typo sends a client chasing credentials |
| `ready`, `validate`, `suspend`, and `terminate` hooks answer 200 without regard to bootstrap state | Daemon lifecycle | `agentd/src/routes.rs:237-258`, `:292-295` | Always 200. `ready` and `validate` are image-*build* hooks called before any instance exists; gating them on a token fails the build rather than the run |

### Daemon filesystem and resources

Confinement here is **two layers**, and the second is the kernel's.

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| A tar member path that is rooted, carries a prefix component, or pops past depth zero is refused | File transfer | `agentd/src/fs.rs:198-216`, refusal at `:682` | Member rejected, 400. Resolution is lexical component walking, never `realpath`/`canonicalize`. A leading `/` is refused rather than stripped: CPython strips it from a member name, but a caller learns nothing from a rewrite |
| Every member is created relative to one root descriptor with `openat2` and `RESOLVE_BENEATH \| RESOLVE_NO_SYMLINKS \| RESOLVE_NO_MAGICLINKS` | File transfer | `agentd/src/fs.rs:45-67` | `ELOOP` for a symlink component, `EXDEV` for a resolution leaving the root; both become a 400 naming the member. This is the layer the lexical one cannot cover: issue #15's `V/a/..` case is judged in-tree by name and lands one level shallower on disk |
| An absolute symlink or hard-link target is refused outright | File transfer | `agentd/src/fs.rs:228-231`, `:726-729` | Member rejected naming "absolute link target" |
| A relative link target must resolve under the extraction root, from its own base depth | File transfer | `agentd/src/fs.rs:218-245` | Member rejected. A symlink resolves from its own directory, a hard link from the archive root — different bases, both confirmed against CPython 3.14's `_get_filtered_attrs` |
| An in-tree symlink is created as a symlink with its target preserved verbatim | File transfer | `agentd/src/fs.rs:20-43` | Accepted. Refusing every link member would break `upload_dir` for any skills tree or test directory containing one — a worse outcome than the traversal hole it guarded |
| Device and FIFO members are refused; member count and total uncompressed size are capped | File transfer | `agentd/src/fs.rs:41-42`, `:664`, `:777`, caps at `agentd/src/config.rs:37-40` | Member or archive rejected, 413. Boundary behavior proved exactly by `agentd/tests/proptest_tar.rs:776` |
| The extraction root must be absolute | File transfer | `agentd/src/fs.rs:1433-1448` | 400. A relative root resolves against the daemon's own working directory, which the caller cannot see |
| A write that would take the filesystem below the configured reserve is refused before it starts | Disk | `agentd/src/disk.rs:66-80`, `agentd/src/fs.rs:97-106`, `:804`, reserve at `agentd/src/config.rs:63-69` | 507 naming the actual free space. Not 500: a 500 is indistinguishable from a daemon defect, so a client retries it — correct for a defect and actively harmful for a full disk. Zero disables the guard |
| Every buffer, output capture, stdin write, linger, TTL, and stream window is bounded | Resources | `agentd/src/config.rs:11-71` | Truncation with a marker, or a bounded refusal. A stdin write that blocks past its timeout gives up rather than pinning a connection for the life of the VM |

Confinement is deliberately asymmetric, and the reasoning was argued with a reviewer
(`agentd/src/fs.rs:4-18`). The single-file routes `PUT`/`GET /v1/fs/file` are **not** confined
to a root. The same bearer token authorizes `POST /v1/exec/start`, which runs arbitrary commands
as root by design, so a root prefix would add no security while breaking real behavior:
harnesses write credentials into home directories, drop config into `/etc`, and stage scratch in
`/tmp`. The confinement that matters is on `PUT /v1/fs/tar`, where member paths come out of an
uploaded archive rather than from a caller who named them. That gap is where the entire traversal
class lives.

### CLI parser closures

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| CLI-5: `--memory` is a closed value set over the five documented baselines | CLI | `microvms-cli/src/cli.rs:231-250`, reasoning at `:4-19` | Unparseable (S1 at the parser). 1500 never reaches a handler. The difference between refusing it at the parser and refusing it in core is a build cycle |
| CLI-5: `--region` is a closed value set over the five MicroVM regions | CLI | `microvms-cli/src/cli.rs:266-284` | Unparseable. `--unlisted-region` is the named way out, declared `conflicts_with = "region"` once on a flattened struct so the relationship cannot be forgotten on the twelfth command (`:298-331`) |
| The domains are spelled out rather than generated, and a test asserts the enum equals the size table | CLI | `microvms-cli/src/cli.rs:13-19`, `microvms-cli/tests/manifest.rs:90` | A domain computed at runtime is invisible to `--help`, to shell completion, and to the manifest's `choices` field. A sixth size class that does not reach `cli.rs` fails the test rather than shipping unreachable |
| No `--client-token`, `--capabilities`, `--connector`, or `--architecture` flag exists | CLI | `microvms-cli/src/cli.rs:21-29` | Unwritable. Core has no such parameter, so there is nothing to forward. Absence asserted by `microvms-cli/tests/thinness.rs:426` and the manifest cross-check |
| A `microvm cp --mode` conflicts with `--tar`; `--poll` conflicts with every writing flag; `--detach` conflicts with the shapes that must not return early | CLI | `microvms-cli/src/cli.rs:660`, `:680`, `:805` | Unparseable, pinned by `microvms-cli/src/cli.rs:1214`, `:1266` |

## Invariants

### VM lifecycle (the twelve STATE requirements)

Enforced in `microvms-core/src/sandbox.rs`, whose `Lifecycle` enum is the spec's `vm_state`
verbatim and which carries the other four spec variables beside it. Every one of those fields is
private and every mutation happens in one of the five lifecycle methods, which is what makes the
Z3 and `stateright` proofs proofs about *this struct's* reachable states
(`microvms-core/src/sandbox.rs:9-17`, `:96-110`, `:504-520`).

| Invariant | Where enforced | Citation |
| --- | --- | --- |
| STATE-1: an accepted launch moves the lifecycle to PENDING and records the image as existing | Application, `Sandbox::run`, after the wire call | `microvms-core/src/sandbox.rs:698-706` |
| STATE-2: the platform reporting a successful run hook is what marks the lifecycle RUNNING and the token installed — not the launch call | Application, `Sandbox::run`, after the wait | `microvms-core/src/sandbox.rs:720-724` |
| STATE-3: the agent token is installed at most once per VM lifetime | Application, both sides | `microvms-core/src/sandbox.rs:649-661` (a second `run` on one sandbox is refused); `agentd/src/state.rs:202-221` (the daemon's one-shot bootstrap) |
| STATE-4: a suspend accepted from RUNNING moves the lifecycle to SUSPENDING before the wait, and after the call | Application, `Sandbox::suspend` | `microvms-core/src/sandbox.rs:769-778` |
| STATE-5: no suspend call is issued while the lifecycle is not RUNNING | Application, before the wire | `microvms-core/src/sandbox.rs:758-767` |
| STATE-6: the platform reporting suspension complete marks the lifecycle SUSPENDED; a VM that dies while suspending is recorded as terminated instead | Application, `Sandbox::suspend` | `microvms-core/src/sandbox.rs:790-809`, wanted set at `microvms-core/src/control/microvm.rs:650` |
| STATE-7: a resume is issued only from SUSPENDED, reuses the installed token, and re-delivers **no** run-hook payload | Application, `Sandbox::resume` | `microvms-core/src/sandbox.rs:849-854`, `:859` |
| STATE-8: a completed resume invalidates the cached proxy token, through the endpoint the service just reported | Application, `Session::rebind` | `microvms-core/src/sandbox.rs:875-883`, `microvms-core/src/session/mod.rs:314-320` |
| STATE-9: an accepted terminate moves the lifecycle to TERMINATING and records the VM as terminated — **before** the call | Application, `Sandbox::terminate` | `microvms-core/src/sandbox.rs:945-949` |
| STATE-10: the platform reporting termination complete marks the lifecycle TERMINATED | Application, `Sandbox::terminate`, only when a wait was asked for | `microvms-core/src/sandbox.rs:972-976` |
| STATE-11: a terminated VM never returns to RUNNING, checked before the window check and before any call | Application, `Sandbox::resume` | `microvms-core/src/sandbox.rs:840-848` |
| STATE-12: a resume past the launch-time suspended window is refused with the elapsed window named | Application, before `ResumeMicrovm` | `microvms-core/src/sandbox.rs:856-857`, `:898-926` |
| The suspended-window clock is stamped after the suspend call and before the wait, and cleared on a successful resume | Application, `Sandbox` | `microvms-core/src/sandbox.rs:775-778`, `:884-887` |
| The three Z3-proved invariants hold over every interleaving: bootstrap at most once, no suspend outside RUNNING, a terminated VM never reaches RUNNING | `stateright` model | `model/src/client.rs:554-569` |
| A locally refused call costs **zero** wire calls — resume after terminate, resume with the window closed, and the payload count matching the launch count are all checked as counters, not as end states | `stateright` model | `model/src/client.rs:584-598`, `:623-640` |
| The installed token is never replaced and survives a suspend/resume cycle | `stateright` model | `model/src/client.rs:599-616` |
| `image_exists` is true exactly when a launch was accepted, and a bootstrapped token implies one | `stateright` model | `model/src/client.rs:570-582` |

### Daemon

| Invariant | Where enforced | Citation |
| --- | --- | --- |
| The launch environment is installed only on the *first* bootstrap, under the token lock | Application, `AppState::bootstrap` | `agentd/src/state.rs:196-210`. A racer that loses the token cannot win the environment: the winner's map is in place before any other caller can observe a token installed |
| The agent token and the launch environment are separate parameters, so no code path can move a byte from one into the other | Application, by signature | `agentd/src/state.rs:187-194`, `agentd/src/routes.rs:166-177` |
| An exec id already present in the registry returns success **without spawning a second child**, decided under the registry lock | Application, `agentd` exec | `agentd/src/exec.rs:363-377` |
| A poisoned lock is recovered rather than propagated, and the soundness argument is per-lock | Application, `agentd` state | `agentd/src/state.rs:8-66`, `:76-92` |
| An arbitrary archive never escapes its root and never panics | `proptest`, 256 cases | `agentd/tests/proptest_tar.rs:566` |
| A symlink cannot redirect a member that arrives after it | `proptest` | `agentd/tests/proptest_tar.rs:600` |
| Plain members and in-tree symlinks are always accepted | `proptest` | `agentd/tests/proptest_tar.rs:720` |
| The attacker is never authorized; bootstrap is one-shot; only the installed token is accepted; the control API is closed before bootstrap | `stateright` model | `model/src/lib.rs:443-465` |
| Output is never released before an ack; a retried start never spawns twice; there is one exec entry per id | `stateright` model | `model/src/lib.rs:466-481` |
| Every safety property has a `sometimes` property beside it, so a green run cannot mean a state space that never reached the interesting states | `stateright` model | `model/src/lib.rs:482-515` |

### Cross-crate and cost

| Invariant | Where enforced | Citation |
| --- | --- | --- |
| COST-4: an unpriced line routes the whole total to a lower-bound variant that names its unpriced items | Application, one `Total::of` | `microvms-core/src/cost.rs:706-751`. `Total::AtLeast` holds the floor *beside* the reasons, and `Add` is implemented only `EstimatedUsd + EstimatedUsd`, so summing an `Amount` is a compile error |
| COST-5: each compute line item is computed from the size-class **baseline**, never from the peak the guest reports | Application, by reachability | `microvms-core/src/cost.rs:1584-1616`, accessors at `microvms-core/src/sizing.rs:184-205`. `compute_lines` reaches only `baseline_gb`/`baseline_vcpu`; reading the peak would overstate the memory line exactly 4x |
| COST-7: a rate table older than 90 days attaches a staleness warning to every report computed from it | Application, on the report | `microvms-core/src/cost.rs:90-95`, `:961-987`, `:1510-1512`. Carried on the report rather than logged, so a library caller with a log filter and a CLI writing only stderr do not each lose it |
| COST-8: the one-week minimum retention floor applies to every snapshot storage line item | Application, a field on the rate row | `microvms-core/src/cost.rs:104-109`, `:1627-1657` |
| COST-10: every duration in a plan estimate is marked `projected` | Application, by type | `microvms-core/src/cost.rs:1871-1880`, `:1908-1913`. `PlanUsage` fields are bare `f64` seconds, so there is no field a `Measured` duration could be written into |
| ARCH-2: protocol drift between client and daemon fails compilation | Cargo dependency graph | `microvms-cli/tests/dependency_direction.rs:179` |
| ARCH-3 / ARCH-4 / BIND-1: `cli -> core -> protocol`, bindings depend only on core, core depends on neither | Test over `cargo_metadata` | `microvms-cli/tests/dependency_direction.rs:68`, `:95`, `:219` |
| ARCH-5: the CLI exports no library target at all | Test over `cargo_metadata` | `microvms-cli/tests/dependency_direction.rs:126` |
| CLI-2: the CLI reaches the control plane and the endpoint proxy only through core, and the guard names *which* seam door was entered | Injected refusing seam | `microvms-cli/src/guards.rs:403`, `:487`; source scan at `microvms-cli/tests/thinness.rs:426` |
| The CLI's direct dependency set is exactly the six allowlisted crates | Test over `cargo_metadata` | `microvms-cli/tests/thinness.rs:145` |
| Only the envelope module and two named exceptions in `main` write to stdout | Source scan | `microvms-cli/tests/thinness.rs:503` |
| CLI-4: one JSON envelope per invocation on stdout, on success, on failure, and on a stream that died before its first event | Spawned-binary test | `microvms-cli/tests/exit_codes.rs:154`, `:198`, `:233` |
| BIND-5: both bindings preserve provenance-labelled durations, estimate-typed dollars, and the distinct `Unpriced` value | Application, by absent constructors | `microvms-py/src/cost.rs:9`, `:23-27`, `:220-241`; `microvms-js/src/cost.rs:20-31`, `:52-57`. `new Duration(3600)` is a `TypeError`, `Amount.usd` is null for an unpriced line, and `to_json`/`to_dict` omit the key entirely rather than emitting a null anything permissive sums as zero |
| Every constant in `constants::as_json` is checked against the pinned botocore service model by the build gate (TRAP-12), and the key set is pinned by a test | Build gate plus a key-set test | `microvms-core/src/constants.rs:33-46`, `:589`, `:693` |
| `DEAD_STATES` is a strict subset of `TERMINAL_STATES`, and `SUSPENDED` is terminal but not dead | Application, pinned by test | `microvms-core/src/constants.rs:448`, `:455`, `:878` |
| The model-backed and tolerated image-ready state sets are disjoint | Application, pinned by test | `microvms-core/src/constants.rs:431`, `:441`, `:941` |
| A ledger file is removed only when nothing is outstanding; leaked identifiers are recorded **before** the delete is attempted | Application, on disk | `microvms-cli/src/ledger.rs:1-22` |
| CLI-3: every failure class exits with its own integer and `ERR_*` string, distinct from `ERR_UNEXPECTED` | Spawned-binary test plus a classification test | `microvms-cli/src/exit.rs:78-101`, `microvms-cli/tests/exit_codes.rs:29`, `microvms-cli/src/guards.rs:2949`, `:3063`; published table cross-checked at `microvms-cli/tests/manifest.rs:161` |
| Retryability is derived from the error kind rather than stored, so the two cannot drift | Application, one `matches!` | `microvms-core/src/error.rs:111-118`, mapping at `:358-397` |

The lifecycle is deliberately **runtime-checked rather than typestate**
(`microvms-core/src/sandbox.rs:19-32`). A `Sandbox<Running>` returning a `Suspended` handle
would make STATE-5's wrong call a compile error, which is strictly stronger on the ladder. But a
type whose Rust identity changes on every transition cannot be one `#[pyclass]`, so it would be
re-erased into a runtime-checked enum at the binding boundary — and the binding's copy is the one
most callers hit. The part of the typestate idea that costs nothing is kept: the check happens
**before** the wire call, and the test asserts the control-plane call count, which is the
observable that distinguishes the two designs.

## Calculations

| Calculation | Inputs | Output | Citation |
| --- | --- | --- | --- |
| Compute cost for a phase, as two separate line items | size class, labelled duration, rate table, phase | vCPU-seconds and GB-seconds line items with estimated dollars | `microvms-core/src/cost.rs:1584-1616` |
| Snapshot storage for a hold, with the retention floor applied | phase, GB, labelled hold, rate table | GB-months line item, note naming the floor when it applied | `microvms-core/src/cost.rs:1627-1657` |
| Snapshot transfer (write on suspend, read on launch or resume) | phase, line, GB, cycle count, rate table | GB line item, no time component | `microvms-core/src/cost.rs:1661-1683` |
| Per-GB-month storage rate, derived from the API's per-GB-hour quote | catalog entry USD per GB-hour | Decimal USD per GB-month | `microvms-core/src/cost.rs:1235-1242`, `:88` |
| A report's total | every line item's phase and amount | `Total::Exact`, or `Total::AtLeast` with named unpriced lines | `microvms-core/src/cost.rs:726-751` |
| Residency ratio: how many times more a running VM costs than a suspended one | two cost reports | Decimal multiplier | `microvms-core/src/cost.rs:1987-1991` |
| Per suspend/resume cycle cost | size class baseline GB, write rate, read rate | `EstimatedUsd` | `microvms-core/src/cost.rs:1993-1999` |
| Break-even suspended hold | size class, rate table | Decimal seconds | `microvms-core/src/cost.rs:2013-2027` |
| Rate table age and staleness verdict | retrieval date, today | days, bool, optional warning text | `microvms-core/src/cost.rs:961-987` |
| Proleptic-Gregorian day number, for date subtraction without a date crate | year, month, day | `i64` days since 1970-01-01 | `microvms-core/src/cost.rs:279-291` |
| Exact seconds from a `Duration`, without a lossy float step | `std::time::Duration` | `Decimal` seconds | `microvms-core/src/cost.rs:120-124` |
| Idempotency token assembly | verb, scope label, 8 random bytes | `<verb>-<tail-64-of-label>-<16 hex>` | `microvms-core/src/control/token.rs:120-148` |
| Connector ARN | intent, region | fully-qualified ARN string | `microvms-core/src/control/connector.rs:60-83` |
| Available bytes on a write target's filesystem | path | `u64` bytes | `agentd/src/disk.rs:66-80` |

### Compute cost per phase

Both figures read the **baseline**, never the peak (`microvms-core/src/cost.rs:1593-1597`).
`vcpu_quantity = baseline_vcpu × seconds`, priced at `rates.vcpu_second()`.
`memory_quantity = baseline_gb × seconds`, priced at `rates.gb_second()`. They are two line items
rather than one blended GB-second because that is how the pricing page prices them, and a blended
figure cannot be reconciled against a Cost Explorer breakdown that keeps them apart
(`microvms-core/src/cost.rs:940-944`). The guest reports the peak and bursts to it, but the peak
is charged only for the seconds above baseline actually consumed; this client cannot observe those
seconds, so the peak is left out rather than guessed at. The 2 GB class reports 8 GB in the guest
(`microvms-core/src/sizing.rs:81-86`), so reading the peak would overstate the memory line exactly
4x.

A suspended VM gets **no compute line at all**, rather than a compute line multiplied by zero. A
zeroed line would reappear the moment someone changed how a duration is derived
(`microvms-core/src/cost.rs:1827-1835`).

### Snapshot storage with the retention floor

`billed_seconds = max(held_seconds, floor_seconds)` where the floor is one week
(`microvms-core/src/cost.rs:1635-1637`). Then
`quantity = gb × billed_seconds / SECONDS_PER_MONTH`, priced at `rates.storage_gb_month()`.
`SECONDS_PER_MONTH` is `2628000`, which is `730 × 3600` — AWS's own month. It is spelled out
because 30-day and calendar-month conventions both give plausible-looking answers that disagree
with the worked examples by a few percent (`microvms-core/src/cost.rs:74-80`).

When the floor applies, the note quotes the day count off the rate row rather than dividing by
86,400 beside the message. The rate-row field is the only thing that knows how long the window is,
so a division written beside the message would keep saying "7-day" after a rate row moved to a
fortnight (`microvms-core/src/cost.rs:1639-1648`, `:918-927`). Not applying the floor would
understate the one line item that dominates a create-and-destroy suite by four orders of
magnitude: a 2 GB image deleted after sixty seconds still bills about four cents
(`microvms-core/src/cost.rs:104-109`).

### Break-even suspended hold

The least trivial formula in the module (`microvms-core/src/cost.rs:2013-2027`).
`running_per_sec = baseline_vcpu × vcpu_rate + baseline_gb × gb_rate`.
`storage_per_sec = baseline_gb × storage_gb_month / SECONDS_PER_MONTH`.
`churn = baseline_gb × (write_rate + read_rate)`.

The solve is **piecewise**, because the storage charge behaves differently on each side of the
minimum-retention window. Inside the window the storage charge is a constant, so the equation is
linear in the hold and the candidate is
`(churn + floor_sec × storage_per_sec) / running_per_sec`. Past the window, storage grows with the
hold and the slope changes, so the answer is `churn / (running_per_sec − storage_per_sec)`
instead. Solving only one branch returns a number in the wrong regime.

This is the figure a pool scheduler needs, and a bare "100x cheaper" headline does not show it.
Below the break-even hold, suspending and resuming costs more than leaving the VM running, so the
conclusion the comparison supports is "avoid churn" rather than "avoid residency"
(`microvms-core/src/cost.rs:1993-1995`).

### Why the sizing table is data, not arithmetic (TRAP-13)

Every documented peak is exactly four times its baseline, which makes `baseline × 4` look like
the obvious simplification. The sizing module must not compute it that way. The regularity belongs
to AWS's current table rather than to the service's contract, so a sixth row that broke the pattern
would get the pattern applied to it, reporting a burst ceiling the service does not offer
(`microvms-core/src/sizing.rs:13-23`).

So `SIZE_CLASSES` (`microvms-core/src/sizing.rs:64-99`) is the only place any of the twenty
numbers appears, and every accessor reads a row out of it through one lookup. To make the guard
falsifiable, `row_in` and `class_for_baseline_in` take the table as a **parameter**
(`microvms-core/src/sizing.rs:247`, `:255`) so a test can drive the accessors over a table whose
peak is *not* 4x its baseline. A test against the shipped table could not tell a lookup from an
arithmetic derivation, because every shipped peak is 4x.

### Two float boundaries, and only two (COST-6)

`seconds_of` (`microvms-core/src/cost.rs:120-124`) is exact rather than a lossy conversion: a
`Duration` is a whole-seconds count plus a nanosecond remainder, both integers, and the nanosecond
division is by a power of ten.

`gb_decimal` (`microvms-core/src/cost.rs:138-152`) is the **only** place an `f64` becomes a
`Decimal`. It goes through the float's decimal *string* rather than its binary value, because
`Decimal::try_from(0.1f64)` would carry the binary error into every downstream figure. It is
fallible rather than lossy: `NaN`, an infinity, and a magnitude past 28 digits have no decimal
reading, and a money figure derived from one of them would be a number nobody could reconcile.
`EstimatedUsd::new` deliberately takes a `Decimal` and not an `f64`, so it cannot become a third
boundary (`microvms-core/src/cost.rs:553-561`).

### The day-number formula

`CalendarDate::day_number` (`microvms-core/src/cost.rs:279-291`) is an era-based
proleptic-Gregorian conversion so that date subtraction needs no date crate. The year is shifted to
be March-based, which puts February's variable length last; `719468` is the day number of
1970-01-01 in the era count. Rate-table age is `today.days_since(retrieved)` over that number
(`microvms-core/src/cost.rs:962-965`).

## Policy and gates

- **Absent parameters are the primary policy mechanism.** The strongest closures are things that
  do not exist: no `client_token` parameter (TRAP-1), no capability list (TRAP-3), no
  `SHELL_INGRESS` variant and no `mint_shell_auth_token` method (TRAP-11), no architecture field,
  no conversion between hook-timeout families, no `f64` accessor on a dollar figure (COST-2), no
  unlabelled duration constructor (COST-1), no `Measured` field on a plan (COST-10). Where a
  requirement is about an impl being *absent*, the check is a program that fails to build — as a
  `compile_fail` doctest with a **pinned error code**, because a bare `compile_fail` passes for any
  build failure including a typo in the test. `microvms-core/src/cost.rs:395-408`, `:519-545`.

- **Local refusal before the wire, always.** Every S2 guard fires before the first control-plane
  call, and where the distinction is observable the test asserts the control-plane **call count**
  rather than just the error. `microvms-core/src/sandbox.rs:29-32`; `model/src/client.rs:584-589`
  states it as a model property.

- **One guard list, runnable before the artifact upload.** `create_image` runs after the caller
  has already uploaded the artifact, so a locally-refusable request refused from inside it has
  cost the caller the S3 PUT. `ControlPlane::preflight` is that same list extracted as a pure
  function of the request, and `create_image` delegates to it rather than keeping a copy, so the
  two cannot drift. A caller who skips preflight loses only the upload.
  `microvms-core/src/control/image.rs:144-158`, `:206-266`; asserted with a zero-call count at
  `microvms-cli/src/guards.rs:1468`.

- **Every refusal names its measurement.** A guard's error message cites the `docs/PLATFORM.md`
  section rather than restating the constraint, because the guards exist so a reader can reach the
  measurement. `microvms-core/src/lib.rs:42-48`.

- **The one S3 escape hatch, and what it costs.** `Region::unlisted` accepts a region this client
  has not seen carry MicroVMs, because AWS adds regions faster than the list is re-read and a
  client that refuses a region AWS just launched in is its own kind of wrong. The override costs
  the diagnostic: if the region does not carry MicroVMs, the first control-plane call answers
  `AccessDeniedException` with a null message and the caller spends an hour reading a correct IAM
  policy. It is a visible enum **variant** rather than a hidden flag, so a reader of a call site
  can see someone opted in, and a supported name handed to it comes back as its proper variant so
  nothing downstream handles two spellings. `microvms-core/src/region.rs:38-63`, `:94-113`.

- **The region list can be wrong in two directions, and one direction is worse.** A *missing*
  region refuses a launch AWS would have accepted — the safer direction, still wrong, and what
  `unlisted` is for. An *extra* region is worse, because it re-opens the null-message trap for a
  name nothing will reject. No API answers the question: `get_available_endpoints` returns an empty
  list while `get_available_regions` returns all 34 Lambda regions, so the list is kept by hand and
  keeping it right is the whole correctness condition. `eu-central-1` does not carry MicroVMs and
  is one of three regions measured returning the null-message denial (2026-08-07).
  `microvms-core/src/region.rs:20-31`.

- **A best-effort probe only raises when it has the evidence.** TRAP-2's stall probe fires once,
  past the grace, and raises only when builds are listed, the list is non-empty, and **every**
  build is `PENDING`. A listing failure returns `Ok(())`, so the wait continues and the caller gets
  a plain timeout. Unknown is not empty, and a wedge claim made on a throttled API call sends the
  reader after the wrong cause. The state field read is `buildState`, not `state`, and the
  deserializer refuses the other spelling. `microvms-core/src/control/image.rs:338-390`.

- **Fail-fast state sets are per-call-site, not global.** `wait_for_state` takes `fail_on` as a
  parameter because different callers fail on different states. Suspend *wants* `SUSPENDED` and
  tolerates `TERMINATED`; resume must pass the *dead* states only, because failing on `SUSPENDED`
  would fail every resume — that is the state the call is made from. This is why `constants.rs`
  carries both `TERMINAL_STATES` and `DEAD_STATES`.
  `microvms-core/src/control/microvm.rs:440-456`, `microvms-core/src/sandbox.rs:860-873`.

- **Token minting lives inside the retry path, and a mint failure is retryable.** A proxy token
  capped at sixty minutes and minted once at construction expires mid-trial, and the rejection is
  indistinguishable from a dead daemon. Refresh is at **half** the ceiling rather than just under
  it, because refreshing at fifty-nine minutes puts the expiry inside the window between building
  the headers and the proxy validating them. A control-plane throttle at minute thirty must not
  kill a trial that is otherwise healthy. `microvms-core/src/session/proxy.rs:21-37`, `:107-111`,
  `:547-560`.

- **Credentials never reach a log line, by construction rather than by care.** `RunHookPayload`
  and both `ProxyToken` types have hand-written `Debug` impls that print the byte count or the
  header names instead of the value. Because `RunMicrovmRequest` keeps its derive, every struct and
  error chain that formats one inherits the behavior. The values that do carry a credential — the
  header pairs and the subprotocol strings — are returned to a caller and stored on nothing, so
  there is no type whose `Debug` could leak one; a caller that logs what it was handed is outside
  what the module can prevent, and both accessors say so.
  `microvms-core/src/control/microvm.rs:70-82`, `:285-298`,
  `microvms-core/src/session/proxy.rs:54-60`. The daemon logs the launch-env variable **count**,
  never the keys or values (`agentd/src/routes.rs:214-221`).

- **Each authorization failure has its own status code.** The daemon answers 503 while no token is
  installed, 401 for a wrong token, 409 for a bootstrap conflict, and 400 for a malformed hook. It
  never answers 404, because clients map 404 onto "file not found" and turn a protocol error into a
  phantom missing artifact. `agentd/src/auth.rs:69-79`.

- **A retried bootstrap of the identical token is success, not a conflict.** The platform may retry
  its own hook, and answering 409 there would fail a launch that is fine.
  `agentd/src/state.rs:94-106`, `agentd/src/routes.rs:224-229`.

- **The bootstrap route is unauthenticated on purpose, and its defense is arity.** The platform has
  no credential to present, and its request arrives over loopback indistinguishably from an in-VM
  process. The defense is that the route can only succeed once.
  `agentd/src/routes.rs:166-171`.

- **Build hooks are ungated on purpose.** `ready` and `validate` are image-*build* hooks called
  before any instance exists and therefore before any token has been delivered. Gating them on
  bootstrap state fails the build rather than the run, which is a confusing place to discover the
  mistake. `agentd/src/routes.rs:237-258`, `microvms-core/src/control/mod.rs:880-886`.

- **Health is reachable before bootstrap, and `busy` is the orchestrator's signal rather than a
  keepalive.** A client needs the contract before it holds a token. And the platform measures
  idleness by inbound traffic through the endpoint proxy, which terminates *outside* the guest, so
  a request an in-VM process sends to this port never reaches the thing counting traffic — an
  in-guest keepalive route would keep nothing alive and would be discovered as broken by a
  multi-hour run auto-suspending mid-work. So an orchestrator outside the VM polls, which is real
  inbound traffic, and reads `busy` to decide whether to keep polling. The assertion of liveness is
  repeated and is the caller's. `agentd/src/routes.rs:297-340`.

- **A resume that finds no installed token is logged loudly rather than treated as routine.**
  Measured 2026-08-05 in us-east-1: the in-memory agent token, the filesystem, exec records, and
  backgrounded processes all survive a suspend/resume cycle, and the endpoint URL is unchanged. So
  the normal case is a VM that needs nothing, and the absence of a token would mean the resume
  behaved like a cold start. The one thing that does change is the guest's view of time: it
  observes the whole suspension as a single jump, so any timeout, lease, or TLS session a running
  command holds expires at once. `agentd/src/routes.rs:260-290`.

- **A poisoned lock is recovered rather than propagated, and the reasoning is per-lock.** The
  daemon is the only channel into the VM — no SSH, no supervisor, no console — so `.expect()` on a
  poisoned mutex converts one handler bug into a permanently unreachable VM. The `token` lock is
  sound in the strong sense: every write is a whole-value assignment, and recovery cannot *install*
  a token, so poisoning is not a bootstrap bypass. `launch_env` is the same argument and carries
  nothing that authorizes anything. The `execs` lock is sound for a narrower reason — the map is
  not left internally corrupt by a panic in a caller's closure, but one exec entry may be
  semantically inconsistent, which limits the blast radius to one exec id.
  `agentd/src/state.rs:8-66`, `:76-92`.

- **A panic is caught at the outermost layer, and that is narrow on purpose.** Without the catch, a
  panicking handler drops the connection and the client sees a transport error it cannot
  distinguish from a dead VM; with it the client gets a 500 and the connection survives. It does
  not undo the panic, and any `std::sync::Mutex` the handler held is now poisoned — which is why it
  pairs with the recovery above. `agentd/src/routes.rs:86-95`.

- **The version header is stamped outside `route_layer`, so it covers every response.** Handler
  bodies, the 401/503 the auth middleware returns before a handler runs, the 413 the body-limit
  layer injects, and the 404 fallback. A version header a client only sometimes receives is one it
  cannot use as a precondition. `agentd/src/routes.rs:78-85`.

- **The extractor-level body limit is disabled and the wire-level layer is the real cap.**
  `DefaultBodyLimit` does not apply to bodies consumed as a stream, so keeping both would silently
  truncate JSON control bodies at 2 MiB while leaving tar uploads unbounded.
  `agentd/src/routes.rs:71-76`.

- **Teardown never raises, and order matters.** `Sandbox::terminate` returns a `TeardownReport`
  rather than a `Result`. It runs where a caller's `finally` would, and an error raised there
  replaces the original failure, which is the one worth reading. The order is VM, then image
  (retrying, because an image in `CREATING` refuses deletion), then the log group **last**, because
  the service can recreate a group deleted before its image. The log group is *named* rather than
  deleted: CloudWatch Logs is not in this crate's dependency set.
  `microvms-core/src/sandbox.rs:45-53`, `:932-935`, `:986-1010`.

- **There is no `Drop` that tears down.** Rust has no context manager and `Drop` cannot await. A
  blocking `Drop` would deadlock inside a runtime and a spawning one would race process exit. So
  `Drop` only warns, naming the id, and the rule is that a caller calls `terminate` explicitly.
  `microvms-core/src/sandbox.rs:55-60`, `:1060-1061`.

- **Leaked identifiers are recorded before the delete is attempted, not after.** Recording after
  loses the identifier when the process dies inside the call, which is exactly the interrupt case
  the ledger exists for. The file is removed only when nothing is outstanding, because a leftover
  file is how `microvm ls` knows there is something to tell the operator about. For a wedged image
  and a service-created log group the identifier **is** the remedy; there is no second way to find
  them. `microvms-cli/src/ledger.rs:1-22`; CLI-6's teardown-on-interrupt guarded at
  `microvms-cli/src/guards.rs:824`, `:917`.

- **Exec idempotency is opt-in, which deliberately inverts TRAP-1's shape.** The default is a
  generated exec id, because `microvm exec` is one shot and an id reused by accident means the
  second invocation is answered from the first's record. The *stable* id is the flag, and what it
  buys is a retry that is safe across the caller's own restart. This differs from a control-plane
  `clientToken`, whose replay wedges an image permanently and which this CLI does not have at all.
  `microvms-cli/src/cli.rs:634-652`, `agentd/src/exec.rs:363-377`; both halves guarded at
  `microvms-cli/src/guards.rs:2004`, `:2045`.

- **Local constants are checked against the pinned service model in the build gate (TRAP-12), and
  the key names are a contract with a script.** `constants::as_json` publishes every hardcoded
  constraint keyed with the names `scripts/check-model-drift.py` reads, and the key set is pinned by
  a test — because a rename here does not fail compilation, it makes a check silently stop
  comparing. The two values no model states, `MICROVM_REGIONS` and `SIZE_CLASSES`, are compared
  against pinned literals in the script instead, since a value compared only against itself passes
  by construction. The gate hard-fails when `MODEL_API_VERSION` disagrees with the service directory
  it resolves, rather than skipping: a constraint checked against a different API version is a
  constraint that was not checked. `microvms-core/src/constants.rs:33-46`, `:52-57`, `:589`, `:693`.

- **A disk write is refused before it starts rather than after ENOSPC.** ENOSPC arrives after the
  filesystem is already full, so by then every other writer in the VM is broken too, including the
  ones that cannot report anything. It also arrives as a generic io error, so the caller cannot
  distinguish "the disk is full" from "the daemon is broken" — and retrying is correct for the
  second case and makes the first worse. `agentd/src/disk.rs:4-30`.

- **A rate table's staleness warning is a fallback rather than the primary defence.** The warning
  can only say that nobody has looked; a drift check against the Pricing API is what tells you
  whether a rate moved. Ninety days is the same order as the interval at which AWS has historically
  restructured Lambda pricing, and the cost of the warning when nothing changed is one line of
  output. `microvms-core/src/cost.rs:50-56`, `:90-95`.

- **A rate table is all-or-nothing, and a fetched one is authoritative on rates while still
  hand-read on rules.** A partial table would price a run at less than it costs with no way for the
  caller to see which field was left stale. The catalog prices line items and says nothing about the
  one-week storage minimum, a per-request charge, a billing increment, or a free tier, so the
  retention floor is carried from the constant rather than from the fetch — dropping it there would
  understate a create-and-destroy suite by four orders of magnitude.
  `microvms-core/src/cost.rs:1191-1199`, `:1253-1258`.

- **Four `WireKind`s collapse onto one exit code deliberately, and `Unauthorized` is not one of
  them.** A shell cannot act differently on "the daemon rejected the request on its merits" in four
  flavours, and a caller that can reads `data.kind`. But a 401's remedy is a credential rather than
  a wait, so it maps to `ERR_CREDENTIALS`: retrying a 401 forever and failing a launch that was
  200 ms from ready are the two mistakes the classification exists to prevent. A `match` on a closed
  enum has no ordering to get wrong. `microvms-core/src/error.rs:358-397`,
  `microvms-cli/src/exit.rs:8-9`, `:41`.

## See also

- [impact analysis](impact-analysis.md) — 23 shared source citations
- [contract map](contract-map.md) — 22 shared source citations
- [debugging guide](debugging-guide.md) — 17 shared source citations
- [processes](../behavior/processes.md) — 14 shared source citations
- [public api](../reference/public-api.md) — 13 shared source citations

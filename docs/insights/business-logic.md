# microvms-agentd · Business logic

## What counts as business logic here, and why the shape is unusual

This codebase has almost no business domain in the usual sense. There are no users, no
orders, no billing accounts. What it has instead is a set of **trap closures**: rules that
exist because someone measured AWS Lambda MicroVMs behaving in a way that points away from
its own cause, and paid for that measurement once so no caller pays again. Seventeen of
those measurements are recorded in `docs/PLATFORM.md`; fifteen are actionable by a client.

So the domain rules in this file are:

- The **13 TRAP requirements** — closures over platform behavior that misleads.
- The **10 COST requirements** — cost-honesty rules, because the client can time a
  suspension to the millisecond and still only infer its price.
- The **12 STATE requirements** — the VM lifecycle, mechanically checked against a
  `stateright` model.
- The **6 agentd requirements** — the daemon's one-shot token ladder.

Each rule carries three things a reader needs: **where it is enforced** (`path:LOC`), **what
it protects against** (the `docs/PLATFORM.md` finding), and its **strength**.

### The strength ladder

Every closure is ranked, strongest first, at `microvms-core/src/lib.rs:21-40`:

- **S1, inexpressible** — the mistake cannot be written down. A closed enum, a newtype with
  no conversion, an absent parameter. Cannot regress without a compile error somewhere.
- **S2, expressible but rejected** — the mistake can be written, and the client refuses it
  locally before any control-plane call, with an error naming the `docs/PLATFORM.md`
  finding. Weaker because the guard is code that can regress, but it costs seconds rather
  than a build cycle. Every boundary where a bare integer or string still has to be judged
  lands here.
- **S3, correct by default and overridable** — weakest, because it protects the caller who
  does nothing and abandons the one who overrides. An S3 closure must state what the
  override costs. There is exactly one in this codebase.

Two conventions follow from the ladder and are worth stating before the tables.

**Every error message names its finding.** A local reject explains itself by citing the
`docs/PLATFORM.md` section that measured the behavior, because the guards exist precisely so
a reader can go to the measurement rather than to a constraint
(`microvms-core/src/lib.rs:42-48`). "region 'eu-central-1' is invalid" sends someone to check
their spelling; the message that is actually raised sends them to the null-message finding.

**A guard that cannot fail is worse than no guard.** Every guard has a named falsification —
a specific plausible edit that must turn a specific test red. "Delete the feature and the
test fails" does not count (`microvms-core/src/lib.rs:50-57`).

### Out of scope

There is no database and no ORM, so there are no DB-side invariants. Wire-level request
validation performed by AWS is out of scope except where this client duplicates it
deliberately — and it duplicates a lot, for a reason recorded at
`microvms-core/src/constants.rs:11-23`: botocore's `VALIDATED_METADATA_ATTRS` is only
`{required, min, document, union}`, so `max`, `pattern`, and `enum` violations are
serialized, sent, and answered with a `ValidationException`. These guards are load-bearing,
and the obvious future simplification — "the SDK validates the model already, delete these" —
silently reopens all of them.

## Validations

### Trap closures — control plane

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| TRAP-1: an image-create or run token is derived from a per-attempt nonce; there is no caller-supplied token parameter | Idempotency | `microvms-core/src/control/token.rs:101`, `:111`, `:120` | Unwritable (S1). The parameter does not exist |
| TRAP-1: the scope label is truncated to 64 bytes at its **tail**, never its head, and the nonce is never truncated | Idempotency | `microvms-core/src/control/token.rs:71`, `:150` | Silent truncation of the label only; the token stays under the 128-char ceiling |
| TRAP-2: an image in `CREATING` past the stall grace with **every** build still `PENDING` fails the wait | Image build | `microvms-core/src/control/image.rs:291-322` | `ERR_BUILD_WEDGED`, naming the clientToken replay signature |
| TRAP-3: guest identity repair is a `bool` intent; the client injects the one accepted enum value `["ALL"]` | Image build | `microvms-core/src/control/image.rs:185-189`, `microvms-core/src/constants.rs:94` | Unwritable (S1). No capability list a caller can populate |
| TRAP-4: a connector is an enumerated intent that derives a fully-qualified ARN for the request region | Networking | `microvms-core/src/control/connector.rs:39-47`, `:77-83` | Unwritable (S1). Two intents, no free-form string |
| TRAP-5: `runHookPayload` over 4096 bytes is refused locally before any control-plane call | Launch | `microvms-core/src/control/microvm.rs:104-127`, `microvms-core/src/constants.rs:61` | `ERR_INVALID_ARG`, naming the service-model ceiling. Inclusive: 4096 passes, 4097 fails |
| TRAP-6: a region outside the five that carry MicroVMs is refused before the first control-plane call | Region | `microvms-core/src/region.rs:45-63`, `:146-164` | S1 for the enum, S2 at the `FromStr` boundary. `ERR_INVALID_ARG` naming the null-message `AccessDeniedException` |
| TRAP-8: a VM reaching a terminal state before RUNNING fails the launch with state **and** `stateReason` attached | Launch | `microvms-core/src/control/microvm.rs:347-350`, `:398-415` | `ERR_LAUNCH_DIED`. Fails fast rather than polling to the deadline |
| TRAP-10: a `minimumMemoryInMiB` that is not one of the five documented baselines is refused locally | Sizing | `microvms-core/src/sizing.rs:146-161` | S1 for a held `SizeClass`, S2 at `from_baseline_mib`. Refused, never snapped to a neighbour |
| TRAP-11: `CreateMicrovmShellAuthToken` is never called and `SHELL_INGRESS` is never requested | Networking | `microvms-core/src/control/connector.rs:16-28`, `microvms-core/src/control/mod.rs:27-31` | Unwritable (S1). No enum variant renders it and no method on `ControlPlane` calls it |
| Two hook-timeout families cannot be interchanged: run/resume/suspend/terminate cap at 60s, ready/validate at 3600s | Hooks | `microvms-core/src/hooks.rs:56-82`, `:84-105` | Unwritable across families (S1) — no `From`, no shared trait. S2 within a family: `ERR_INVALID_ARG` naming **both** ceilings |
| A hook port outside 1..=65535 is refused | Hooks | `microvms-core/src/hooks.rs:141-149` | `ERR_INVALID_ARG` naming the model range |
| `maximumDurationInSeconds` outside 1..=28800 is refused | Launch | `microvms-core/src/control/mod.rs:371-386` | `ERR_INVALID_ARG` saying a longer session needs a second VM, not a larger number |
| An image name is checked against `[a-zA-Z0-9-_]+` and a 64-character ceiling before the artifact upload | Image build | `microvms-core/src/constants.rs:158-164`, `microvms-core/src/control/mod.rs:390+` | `ERR_INVALID_ARG`, with three separate messages because the pattern message misleads for a 70-character name containing no dots or slashes |

`IdlePolicy.maxIdleDurationSeconds` is the deliberate counter-example: it has **no** local
guard, because its constraint is `min: 60`, which botocore does enforce locally with a clear
message (`microvms-core/src/constants.rs:25-27`, `microvms-core/src/control/mod.rs:330-331`).
The absence is a decision, not a gap.

### Trap closures — in-VM session

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| TRAP-7: the proxy token is read out of the `authToken` **map**, not as a string | Session | `microvms-core/src/control/microvm.rs:239-262`, `microvms-core/src/session/proxy.rs:69-77` | S1: `ProxyToken` has no `as_str`, no `Display`, no `Deref`. A missing key is `WireKind::AuthTokenMint`, which is retryable |
| TRAP-7: every endpoint request sends **both** `X-aws-proxy-auth` and `X-aws-proxy-port` | Session | `microvms-core/src/control/microvm.rs:268-274` | Structurally: `headers()` returns a two-element array. One without the other is rejected indistinguishably from a bad token |
| TRAP-9: the proxy token is minted inside the request retry path, at an interval strictly below the 60-minute ceiling | Session | `microvms-core/src/session/proxy.rs:62`, `:67`, `:288-296` | S2: an interval `>= MAX_TOKEN_LIFETIME` is refused at construction |

### Cost honesty

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| COST-1: every duration carries a `measured` or `projected` provenance label; there is no unlabelled constructor | Cost | `microvms-core/src/cost.rs:419-425` | Unwritable (S1). `DurationP` is an enum whose every variant names its provenance; no `From<Duration>`, no `Default`, both pinned by `compile_fail` doctests at `:395-408` |
| COST-2: an estimated dollar amount has no coercion to a bare float | Cost | `microvms-core/src/cost.rs:546-578` | Unwritable (S1). Private field, no `From`, no `Into<f64>`, no `Deref`. Three `compile_fail` doctests at `:519-545`, each pinning its own error code |
| COST-3: an unpriced quantity is a distinct `Unpriced` variant carrying a reason, never zero dollars | Cost | `microvms-core/src/cost.rs:614-625` | S1 by exhaustive `match`. A consumer matching an `Amount` must handle it |
| COST-4: an unpriced line routes the whole total to a lower-bound variant that names its unpriced items | Cost | `microvms-core/src/cost.rs:706-751` | S1. `Total::AtLeast` holds the floor *beside* the reasons, so reaching the floor means seeing them. `Add` is implemented only `EstimatedUsd + EstimatedUsd`, so summing an `Amount` is a compile error |
| COST-5: each compute line item is computed from the size-class **baseline**, never from the peak the guest reports | Cost | `microvms-core/src/cost.rs:1584-1616`, `microvms-core/src/sizing.rs:195` | Structurally: `compute_lines` reaches only `baseline_gb`/`baseline_vcpu`. Reading the peak would overstate the memory line exactly 4x |
| COST-6: money arithmetic is decimal end to end; a float is converted exactly once at the boundary | Cost | `microvms-core/src/cost.rs:120-124`, `:138-152` | Two boundaries only, both named. `gb_decimal` is fallible: `NaN`, infinity, and a magnitude past 28 digits are refused |
| COST-7: a rate table older than 90 days attaches a staleness warning to every report computed from it | Cost | `microvms-core/src/cost.rs:95`, `:975-987`, `:1510-1512` | Warning text carried **on the report**, not logged, so a library caller with a log filter and a CLI writing only stderr do not each lose it |
| COST-8: the one-week minimum retention floor applies to every snapshot storage line item | Cost | `microvms-core/src/cost.rs:109`, `:1627-1657` | The floor is a field on the rate row, so it applies to anything stored there. A floored line item says so in its note |
| COST-9: compute is priced from the ARM rate only; a catalog whose ARM line is missing is rejected rather than substituted | Cost | `microvms-core/src/cost.rs:1200-1258`, `:1268-1302` | S1 for direct construction — the rate fields are private and there are exactly two doors. S2 at `from_catalog`, which refuses four ways and names the x86 sibling it will not substitute |
| COST-10: every duration in a plan estimate is marked `projected`, so the report is distinguishable from a measured one | Cost | `microvms-core/src/cost.rs:1871-1880`, `:1908-1913` | Unwritable (S1). `PlanUsage` fields are bare `f64` seconds — there is no field a `Measured` duration could be written into |
| A calendar date arriving from outside the crate is validated against its month's real length | Cost | `microvms-core/src/cost.rs:232-241` | S2. `2026-02-30` would otherwise yield a day number for March 2nd and an age two days out |

### Daemon authorization

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| A control request with a token equal to the installed agent token is accepted | Daemon auth | `agentd/src/state.rs:189-193`, `agentd/src/auth.rs:75-79` | Constant-time byte comparison via `subtle::ConstantTimeEq` |
| A control request with a differing token is rejected | Daemon auth | `agentd/src/auth.rs:78` | 401 |
| A control request while no token is installed is rejected **distinguishably** from a bad credential | Daemon auth | `agentd/src/state.rs:185-188`, `agentd/src/auth.rs:73`, `:76` | 503, never 401 and never 404 — a client maps 404 onto "file not found", turning a protocol error into a phantom missing artifact |
| A bootstrap request presenting a token while none is installed installs it | Daemon auth | `agentd/src/state.rs:163-169`, `agentd/src/routes.rs:201-204` | 200 |
| A bootstrap request presenting a token **identical** to the installed one is accepted | Daemon auth | `agentd/src/state.rs:170-174`, `agentd/src/routes.rs:205-210` | 200. The platform may retry its own hook, and answering 409 would fail a launch that is fine |
| A bootstrap request presenting a **different** token is refused and changes nothing | Daemon auth | `agentd/src/state.rs:174-175`, `agentd/src/routes.rs:211-214` | 409, installed token unchanged |
| A malformed run-hook body, an absent `runHookPayload`, a non-JSON payload, or an empty `agent_token` are each refused | Daemon auth | `agentd/src/routes.rs:177-198` | 400, never 404. The token and the payload carrying it are never logged |
| The `Authorization` header is parsed and compared on **raw bytes**, never decoded as UTF-8 first | Daemon auth | `agentd/src/auth.rs:40-47`, `:28-33` | No token extracted, or a mismatch. Never a crash |
| Authorization is decided **before** the request body is polled | Daemon auth | `agentd/src/auth.rs:62-89` | An unauthenticated caller cannot make the daemon allocate |
| `ready` and `validate` hooks answer 200 without regard to bootstrap state | Daemon lifecycle | `agentd/src/routes.rs:218-234` | Always 200. They are image-*build* hooks called before any instance exists; gating them on a token fails the build rather than the run |

### Filesystem and resource guards

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| A tar member path that is rooted, carries a prefix component, or pops past depth zero is refused | File transfer | `agentd/src/fs.rs:167-184` | Member rejected. Resolution is **lexical only** — never `realpath`/`canonicalize` |
| An absolute symlink or hard-link target is refused outright | File transfer | `agentd/src/fs.rs:196-198`, `:355-359` | Member rejected naming "absolute link target" |
| A relative link target must resolve under the extraction root, from its own base depth | File transfer | `agentd/src/fs.rs:196-212` | Member rejected. A symlink resolves from its own directory, a hard link from the archive root — different bases, both confirmed against CPython 3.14 |
| Device and FIFO members are refused; member count and total uncompressed size are capped | File transfer | `agentd/src/fs.rs:41-42`, `agentd/src/config.rs:38-40` | Member or archive rejected |
| The extraction root must be absolute | File transfer | `agentd/src/fs.rs:842-848` | Rejected |
| A write that would take the filesystem below the configured reserve is refused before it starts | Disk | `agentd/src/disk.rs:66-80`, `agentd/src/config.rs:65-71` | 507 naming the actual free space, rather than an ENOSPC surfacing as an indistinguishable 500 |
| Every buffer, output capture, stdin write, linger, TTL, and stream window is bounded | Resources | `agentd/src/config.rs:11-71` | Truncation with a marker, or a bounded refusal. Every bound exists because its unbounded version was a defect in the Python predecessor |

Note the deliberate **asymmetry** in filesystem confinement, argued with a reviewer rather
than overlooked (`agentd/src/fs.rs:4-18`). The single-file routes `PUT`/`GET /v1/fs/file` are
**not** confined to a root: the same bearer token authorizes `POST /v1/exec/start`, which
runs arbitrary commands as root by design, so a root prefix would add no security while
breaking real behavior. The confinement that matters is on `PUT /v1/fs/tar`, where member
paths come out of an uploaded archive rather than from a caller who named them — that gap is
the entire traversal class.

### CLI parser closures

| Rule | Domain | Citation | Failure mode |
| --- | --- | --- | --- |
| CLI-5: `--memory` is a closed value set over the five documented baselines | CLI | `microvms-cli/src/cli.rs:225-236`, reasoning at `:6-11` | Unparseable (S1 at the parser). 1500 never reaches a handler, pinned at `:893-905` |
| CLI-5: `--region` is a closed value set over the five MicroVM regions | CLI | `microvms-cli/src/cli.rs:258-270` | Unparseable, pinned at `:887-891`. `--unlisted-region` (`:300`) is the named way out, conflicting with `--region` |
| No `--client-token`, no `--capabilities`, no `--connector` flag exists | CLI | `microvms-cli/src/cli.rs:23-26` | Unwritable. Core has no such parameter, so there is nothing to forward |

## Invariants

| Invariant | Where enforced | Citation |
| --- | --- | --- |
| STATE-1: an accepted launch moves the lifecycle to PENDING and records the image as existing | Application, `Sandbox::run` | `microvms-core/src/sandbox.rs:567-569` |
| STATE-2: the platform reporting a successful run hook is what marks the lifecycle RUNNING and the token installed — not the launch call | Application, `Sandbox::run` | `microvms-core/src/sandbox.rs:589-593` |
| STATE-3: the agent token is installed at most once per VM lifetime | Application (both sides) | `microvms-core/src/sandbox.rs:525-534` (client refuses a second `run`); `agentd/src/state.rs:163-178` (daemon's one-shot bootstrap) |
| STATE-4: a suspend accepted from RUNNING moves the lifecycle to SUSPENDING before the wait | Application, `Sandbox::suspend` | `microvms-core/src/sandbox.rs:638-640` |
| STATE-5: no suspend call is issued while the lifecycle is not RUNNING | Application, before the wire | `microvms-core/src/sandbox.rs:628-636` |
| STATE-6: the platform reporting suspension complete marks the lifecycle SUSPENDED | Application, `Sandbox::suspend` | `microvms-core/src/sandbox.rs:656-676` |
| STATE-7: a resume from SUSPENDED reuses the installed token and re-delivers **no** run-hook payload | Application, `Sandbox::resume` | `microvms-core/src/sandbox.rs:684-688`, `:715-720` |
| STATE-8: a completed resume invalidates the cached proxy token | Application, `Session::rebind` | `microvms-core/src/sandbox.rs:747-749`, `microvms-core/src/session/mod.rs:256` |
| STATE-9: an accepted terminate moves the lifecycle to TERMINATING and records the VM as terminated — **before** the call | Application, `Sandbox::terminate` | `microvms-core/src/sandbox.rs:814-815` |
| STATE-10: the platform reporting termination complete marks the lifecycle TERMINATED | Application, `Sandbox::terminate` | `microvms-core/src/sandbox.rs:278` |
| STATE-11: a terminated VM never returns to RUNNING | Application, checked before the window check and before any call | `microvms-core/src/sandbox.rs:708-714` |
| STATE-12: a resume past the launch-time suspended window is refused with the elapsed window named | Application, before `ResumeMicrovm` | `microvms-core/src/sandbox.rs:768-792` |
| The lifecycle is one of exactly six states, and every one of the five state variables is private and mutated only in the five lifecycle methods | Application, by field privacy | `microvms-core/src/sandbox.rs:96-110`, `:9-17` |
| An exec id already present in the registry returns success **without spawning a second child**, decided under the registry lock | Application, `agentd` exec | `agentd/src/exec.rs:362-375` |
| A ledger file is removed only when nothing is outstanding; leaked identifiers are recorded **before** the delete is attempted | Application, on disk | `microvms-cli/src/ledger.rs:11-21` |
| Every constant in `constants::as_json` is checked against the pinned botocore service model by the build gate (TRAP-12) | Build gate + a pinned key-set test | `microvms-core/src/constants.rs:176-208`, `:222-251`; `microvms-cli/src/cli.rs:209-213` |
| `DEAD_STATES` is a strict subset of `TERMINAL_STATES`, and `SUSPENDED` is terminal but not dead | Application, pinned by test | `microvms-core/src/constants.rs:137-149`, `:312-320` |
| The model-backed and tolerated image-ready state sets are disjoint | Application, pinned by test | `microvms-core/src/constants.rs:326-333` |
| A poisoned lock is recovered rather than propagated, because the daemon is the only channel into the VM | Application, `agentd` state | `agentd/src/state.rs:69-83`, reasoning at `:8-54` |

The STATE-* invariants are not only enforced in code — they are the `stateModel` in
`spec/core.symspec.json`, whose five variables (`vm_state`, `token_installed`,
`image_exists`, `was_terminated`, `bootstrap_count` bounded 0..3) are the `Sandbox` struct's
private fields verbatim. The Z3/stateright proofs over that model (bootstrap at most once,
suspend from non-RUNNING unreachable, TERMINATED never returns to RUNNING) are proofs about
*this struct's* reachable states, which is only worth something because the transitions in
`sandbox.rs` are the only way to move it (`microvms-core/src/sandbox.rs:9-17`).

One design decision worth recording: the lifecycle is **runtime-checked rather than
typestate**, deliberately (`microvms-core/src/sandbox.rs:19-32`). A `Sandbox<Running>`
returning a `Suspended` handle would make STATE-5's wrong call a compile error — strictly
stronger on the ladder — but a type whose Rust identity changes on every transition cannot be
one `#[pyclass]`, so it would be re-erased into a runtime-checked enum at the binding
boundary, and the binding's copy is the one most callers actually hit. What is kept from the
typestate idea is the part that costs nothing: the check happens **before** the wire call, and
the test asserts the control-plane call count, which is the observable that distinguishes the
two.

## Calculations

| Calculation | Inputs | Output | Citation |
| --- | --- | --- | --- |
| Compute cost for a phase, as two separate line items | size class, labelled duration, rate table, phase | vCPU-seconds and GB-seconds line items with estimated dollars | `microvms-core/src/cost.rs:1584-1616` |
| Snapshot storage for a hold, with the retention floor applied | phase, GB, labelled hold, rate table | GB-months line item, note naming the floor when it applied | `microvms-core/src/cost.rs:1627-1657` |
| Snapshot transfer (write on suspend, read on launch or resume) | phase, line, GB, cycle count, rate table | GB line item | `microvms-core/src/cost.rs:1659-1683` |
| Per-GB-month storage rate, derived from the API's per-GB-hour quote | catalog entry USD per GB-hour | Decimal USD per GB-month | `microvms-core/src/cost.rs:1236-1240`, `:88` |
| A report's total | every line item's phase and amount | `Total::Exact` or `Total::AtLeast` with named unpriced lines | `microvms-core/src/cost.rs:734-751` |
| Residency ratio: how many times more a running VM costs than a suspended one | two cost reports | Decimal multiplier | `microvms-core/src/cost.rs:1987-1989` |
| Per suspend/resume cycle cost | size class baseline GB, write rate, read rate | `EstimatedUsd` | `microvms-core/src/cost.rs:1994-1999` |
| Break-even suspended hold: how long a VM must stay suspended for a cycle to pay for itself | size class, rate table | Decimal seconds | `microvms-core/src/cost.rs:2011-2027` |
| Rate table age and staleness verdict | retrieval date, today | days, bool, optional warning text | `microvms-core/src/cost.rs:961-987` |
| Proleptic-Gregorian day number, for date subtraction without a date crate | year, month, day | `i64` days since 1970-01-01 | `microvms-core/src/cost.rs:279-291` |
| Idempotency token assembly | verb, scope label, 8 random bytes | `<verb>-<tail-64-of-label>-<16 hex>` | `microvms-core/src/control/token.rs:120-144` |
| Connector ARN | intent, region | fully-qualified ARN string | `microvms-core/src/control/connector.rs:77-83` |
| Available bytes on a write target's filesystem | path | `u64` bytes | `agentd/src/disk.rs:66-80` |

### Compute cost per phase

Both figures read the **baseline**, never the peak (`microvms-core/src/cost.rs:1591-1595`).
`vcpu_quantity = baseline_vcpu × seconds`, priced at `rates.vcpu_second()`.
`memory_quantity = baseline_gb × seconds`, priced at `rates.gb_second()`. They are two line
items rather than one blended GB-second because that is how the pricing page prices them, and
a blended figure cannot be reconciled against a Cost Explorer breakdown that keeps them apart
(`microvms-core/src/cost.rs:937-944`). The guest reports the peak and bursts to it, but the
peak is charged only for the seconds above baseline actually consumed — which this client
cannot observe, so it is left out rather than guessed at. Reading the peak would overstate the
memory line exactly 4x, since the 2 GB class reports 8 GB in the guest.

A suspended VM gets **no compute line at all** — not a compute line multiplied by zero, which
would reappear the moment someone changed how a duration is derived
(`microvms-core/src/cost.rs:1825-1835`).

### Snapshot storage with the retention floor

`billed_seconds = max(held_seconds, floor_seconds)` where the floor is one week
(`microvms-core/src/cost.rs:1633-1635`). Then
`quantity = gb × billed_seconds / SECONDS_PER_MONTH`, priced at `rates.storage_gb_month()`.
`SECONDS_PER_MONTH` is 2,628,000 — that is `730 × 3600`, AWS's own month, spelled out because
30-day and calendar-month conventions both give plausible-looking answers that disagree with
the worked examples by a few percent (`microvms-core/src/cost.rs:74-80`).

When the floor applied, the note quotes the day count off the rate row rather than dividing
by 86,400 beside the message — the field is the only thing that knows how long the window is,
and a division written beside the message would keep saying "7-day" after a rate row moved to
a fortnight (`microvms-core/src/cost.rs:1639-1646`, `:916-927`).

Not applying the floor would understate the one line item that dominates a
create-and-destroy suite, by four orders of magnitude: a 2 GB image deleted after sixty
seconds still bills about four cents (`microvms-core/src/cost.rs:104-109`).

### Break-even suspended hold

The one genuinely non-trivial formula (`microvms-core/src/cost.rs:2011-2027`).
`running_per_sec = baseline_vcpu × vcpu_rate + baseline_gb × gb_rate`.
`storage_per_sec = baseline_gb × storage_gb_month / SECONDS_PER_MONTH`.
`churn = baseline_gb × (write_rate + read_rate)`.

It is a **piecewise** solve, and the piecewise-ness is the point. Inside the
minimum-retention window the storage charge is a constant, so the equation is linear in the
hold: `candidate = (churn + floor_sec × storage_per_sec) / running_per_sec`. Past the window
storage grows with the hold and the slope changes, so if the candidate falls outside the
window the answer is `churn / (running_per_sec − storage_per_sec)` instead. Solving only one
branch would return a number in the wrong regime.

This is the figure a pool scheduler actually needs, and the one a bare "100x cheaper"
headline hides — below it, suspending and resuming costs more than having left the VM running
(`microvms-core/src/cost.rs:2001-2011`). The conclusion the comparison supports is "avoid
churn", not "avoid residency" (`microvms-core/src/cost.rs:1936-1945`).

### Why the sizing table is data, not arithmetic (TRAP-13)

Every documented peak is exactly four times its baseline, which makes `baseline × 4` look
like the obvious simplification. It is the one thing the sizing module must not do: that
regularity is AWS's and not ours, and a sixth row that broke the pattern would silently get
the pattern applied to it, reporting a burst ceiling the service does not offer
(`microvms-core/src/sizing.rs:13-23`).

So `SIZE_CLASSES` (`microvms-core/src/sizing.rs:68-99`) is the only place any of the twenty
numbers appears, and every accessor reads a row out of it through one lookup. The
falsifiability device is that `row_in` and `class_for_baseline_in` take the table as a
**parameter** (`microvms-core/src/sizing.rs:247`, `:255`) purely so a test can drive the
accessors over a table whose peak is *not* 4x its baseline — a test against the shipped table
could not tell a lookup from an arithmetic derivation, because every shipped peak *is* 4x
(`microvms-core/src/sizing.rs:298-314`).

### Two float boundaries, and only two (COST-6)

`seconds_of` (`microvms-core/src/cost.rs:120-124`) is exact rather than a conversion: a
`Duration` is a whole-seconds count plus a nanosecond remainder, both integers, and the
nanosecond division is by a power of ten.

`gb_decimal` (`microvms-core/src/cost.rs:138-152`) is the **only** place an `f64` becomes a
`Decimal`. It goes through the float's decimal *string* rather than its binary value, because
`Decimal::try_from(0.1f64)` would carry the binary error into every downstream figure. It is
fallible rather than lossy: `NaN`, an infinity, and a magnitude past 28 digits have no decimal
reading, and a money figure derived from one of them would be a number nobody could
reconcile. `EstimatedUsd::new` deliberately takes a `Decimal` and not an `f64`, so it cannot
become a third boundary (`microvms-core/src/cost.rs:553-561`).

## Policy and gates

- **Absent parameters are the primary policy mechanism.** The strongest closures in this
  codebase are things that do not exist: no `client_token` parameter (TRAP-1), no capability
  list (TRAP-3), no `SHELL_INGRESS` variant and no `mint_shell_auth_token` method (TRAP-11),
  no conversion between hook-timeout families, no `f64` accessor on a dollar figure (COST-2),
  no unlabelled duration constructor (COST-1), no `Measured` field on a plan (COST-10). Where
  a requirement is about an impl being *absent*, the check is a program that fails to build —
  `compile_fail` doctests with pinned error codes, because a bare `compile_fail` passes for
  any build failure including a typo in the test itself. `microvms-core/src/cost.rs:395-408`,
  `:519-545`.

- **Local refusal before the wire, always.** Every S2 guard fires before the first
  control-plane call, and where the distinction is observable the test asserts the
  control-plane **call count** rather than just the error. The cost is seconds; the
  alternative is a build cycle, a poll timeout, or an answer about the wrong cause.
  `microvms-core/src/sandbox.rs:28-32`, `:621-623`.

- **Every refusal names its measurement.** A guard's error message cites the
  `docs/PLATFORM.md` section rather than restating the constraint, because the guards exist so
  a reader can reach the measurement. `microvms-core/src/lib.rs:42-48`.

- **The one S3 escape hatch, and what it costs:** `Region::unlisted` accepts a region this
  client has not seen carry MicroVMs, because AWS adds regions faster than the list is
  re-read and a client that refuses a region AWS just launched in is its own kind of wrong.
  The override costs exactly the diagnostic: the first control-plane call answers
  `AccessDeniedException` with a null message and the caller spends an hour reading a correct
  IAM policy. It is a visible enum **variant** rather than a hidden flag, so a reader of a
  call site can see someone opted in, and a supported name handed to it comes back as its
  proper variant so nothing downstream handles two spellings.
  `microvms-core/src/region.rs:51-62`, `:94-113`.

- **The region list's correctness condition runs in both directions, and one direction is
  worse.** A *missing* region refuses a launch AWS would have accepted — the safer direction,
  still wrong, and what `unlisted` is for. An *extra* region is worse, because it re-opens the
  null-message trap for a name nothing will reject. `eu-central-1` was on this list until
  2026-08-07 and does not carry MicroVMs; it is the named falsification case.
  `microvms-core/src/region.rs:20-31`, `:190-214`.

- **A best-effort probe must not make a claim it cannot support.** TRAP-2's stall probe fires
  once, past the grace, and raises only when builds are listed, the list is non-empty, and
  **every** build is `PENDING`. A listing failure returns `Ok(())` — the wait continues and
  the caller gets an honest timeout — rather than either breaking the wait or reporting
  "everything is fine". Unknown is not empty, and a wedge claim made on a throttled API call
  sends the reader after the wrong cause. `microvms-core/src/control/image.rs:276-322`.

- **Fail-fast state sets are per-call-site, not global.** `wait_for_state` takes `fail_on` as
  a parameter because suspend *wants* `SUSPENDED` and tolerates `TERMINATED`, while resume
  must pass the *dead* states only — failing on `SUSPENDED` would fail every resume, since
  that is the state the call is made from. This is why `constants.rs` carries both
  `TERMINAL_STATES` and `DEAD_STATES`. `microvms-core/src/control/microvm.rs:353-361`,
  `microvms-core/src/constants.rs:137-149`.

- **Token minting lives inside the retry path, and a mint failure is retryable.** A proxy
  token capped at sixty minutes and minted once at construction expires mid-trial, and the
  rejection is indistinguishable from a dead daemon. Refresh is at **half** the ceiling rather
  than just under it: refreshing at fifty-nine minutes puts the expiry inside the window
  between building the headers and the proxy validating them. A control-plane throttle at
  minute thirty must not kill a trial that is otherwise healthy.
  `microvms-core/src/session/proxy.rs:21-39`, `:62-67`.

- **Credentials never reach a log line.** `RunHookPayload` and both `ProxyToken` types have
  hand-written `Debug` impls that print the byte count or the header names, never the value —
  and because `RunMicrovmRequest` keeps its derive, the safety is inherited by every struct
  and error chain that formats one. `microvms-core/src/control/microvm.rs:79-83`, `:218-233`.
  The daemon likewise never logs the token or the payload carrying it
  (`agentd/src/routes.rs:189`).

- **Unbootstrapped is not unauthorized, and neither is missing.** The daemon answers 503 while
  no token is installed, 401 for a wrong token, 409 for a bootstrap conflict, and 400 for a
  malformed hook — never 404, because clients map 404 onto "file not found" and turn a
  protocol error into a phantom missing artifact. Collapsing 503 and 401 was a real defect
  class. `agentd/src/auth.rs:70-79`, `agentd/src/state.rs:236-242` (the test that pins it).

- **A retried bootstrap of the identical token is success, not a conflict.** The platform may
  retry its own hook, and answering 409 there would fail a launch that is fine.
  `agentd/src/state.rs:90-96`, `agentd/src/routes.rs:205-210`.

- **Build hooks are ungated on purpose.** `ready` and `validate` are image-*build* hooks
  called before any instance exists and therefore before any token has been delivered.
  Gating them on bootstrap state fails the build rather than the run, which is a confusing
  place to discover the mistake. `agentd/src/routes.rs:218-234`.

- **Health is reachable before bootstrap.** A client needs the contract before it holds a
  token: the bootstrap token arrives at the platform's `/run` hook, so between launch and
  bootstrap there is a window in which a gated health route would answer 503 — which a client
  reads as "the daemon is broken" rather than "not yet bootstrapped".
  `agentd/src/routes.rs:307-316`.

- **A poisoned lock is recovered, not propagated, and the reasoning is per-lock.** The
  daemon is the only channel into the VM — no SSH, no supervisor, no console — so
  `.expect()` on a poisoned mutex converts one handler bug into a permanently unreachable VM.
  The `token` lock is sound in the strong sense (every write is a whole-value assignment, and
  recovery cannot *install* a token, so poisoning is not a bootstrap bypass). The `execs`
  lock is sound for a narrower reason: the map is not left internally corrupt, but one exec
  entry may be semantically inconsistent — a blast radius of one exec id against a dead VM.
  `agentd/src/state.rs:8-54`, `:69-83`.

- **Teardown never raises, and order matters.** `Sandbox::terminate` returns a
  `TeardownReport` rather than a `Result`, because it runs where a caller's `finally` would
  and an error raised there replaces the real failure — the real one being the one worth
  reading. Order is VM, then image (retrying twenty times, because an image in `CREATING`
  refuses deletion), then the log group **last**, because the service can recreate a group
  deleted before its image. `microvms-core/src/sandbox.rs:45-53`, `:78-86`, `:801+`.

- **There is no `Drop` that tears down.** Rust has no context manager and `Drop` cannot
  await: a blocking `Drop` would deadlock inside a runtime and a spawning one would race
  process exit. So `Drop` only warns, naming the id, and the rule is that a caller calls
  `terminate` explicitly. `microvms-core/src/sandbox.rs:55-60`.

- **Leaked identifiers are recorded before the delete is attempted, not after.** The other
  order loses the identifier when the process dies inside the call — exactly the interrupt
  case the ledger exists for. And the ledger file is only removed when nothing is
  outstanding, because a leftover file is how `microvm ls` knows there is something to tell
  the operator about. For a wedged image and a service-created log group the identifier **is**
  the remedy; there is no second way to find them. `microvms-cli/src/ledger.rs:5-21`.

- **Exec idempotency is opt-in, inverting TRAP-1's shape deliberately.** The default is a
  generated exec id, because `microvm exec` is one shot and an id reused by accident means the
  second invocation is answered from the first's record. The *stable* id is the flag. What it
  buys is a retry safe across the caller's own restart: the daemon returns success for a known
  id without spawning a second child, decided under the registry lock. This is explicitly not
  a control-plane `clientToken`, whose replay wedges an image permanently and which this CLI
  therefore does not have at all. `microvms-cli/src/cli.rs:487-509`,
  `agentd/src/exec.rs:362-375`.

- **The parser is the outermost gate (CLI-5).** `--memory` and `--region` are closed value
  sets, so an off-table baseline or an unsupported region is unparseable rather than refused a
  call later. The difference between refusing 1500 at the parser and refusing it in core is a
  build cycle. `microvms-cli/src/cli.rs:216-236`, `:893-905`.

- **Local constants are checked against the pinned service model in the build gate
  (TRAP-12).** `constants::as_json` publishes every hardcoded constraint keyed with names the
  drift script reads, and the key set is pinned by a test — because a rename here does not
  fail compilation, it makes a check silently stop comparing. The two values no model states,
  `MICROVM_REGIONS` and `SIZE_CLASSES`, are compared against pinned literals in the script
  instead, since a value compared only against itself passes by construction.
  `microvms-core/src/constants.rs:29-42`, `:176-208`, `:222-251`.

- **A disk write is refused before it starts rather than after ENOSPC.** ENOSPC arrives after
  the filesystem is already full, so by then every other writer in the VM is broken too
  including the ones that cannot report anything — and it arrives as a generic io error, so
  the caller cannot distinguish "the disk is full" from "the daemon is broken", and retrying
  (correct for the second) makes the first worse. Grounded in a documented incident,
  anthropics/claude-code#59856. `agentd/src/disk.rs:4-30`.

- **A rate table's staleness warning is a fallback, not the defence.** It can only say that
  nobody has looked; a drift check against the Pricing API is what tells you whether a rate
  moved. Ninety days is the same order as the interval at which AWS has historically
  restructured Lambda pricing, and the cost of the warning when nothing changed is one line of
  output. `microvms-core/src/cost.rs:50-56`, `:90-95`.

- **A rate table is all-or-nothing.** A partial table would price a run at less than it costs
  with no way for the caller to see which field was left stale, so `from_catalog` refuses four
  ways — a missing ARM compute line whose x86 sibling is present, a missing line with no
  sibling, a restated unit, and two products where there was one. The ARM case gets its own
  message naming the rate it refuses and the magnitude of the error substituting it would
  introduce (~18%), because the tempting fix is to use the sibling and every estimate would
  inflate while nothing said so. `microvms-core/src/cost.rs:1191-1199`, `:1268-1302`.

## See also

- [microvms-agentd · Debugging guide](debugging-guide.md)
- [microvms-agentd · Processes](../behavior/processes.md)
- [microvms-agentd · Contract map](contract-map.md)
- [microvms-agentd · Public API](../reference/public-api.md)
- [microvms-agentd · Impact analysis](impact-analysis.md)

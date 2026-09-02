# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions are [semantic](https://semver.org/spec/v2.0.0.html); the wire contract in
`docs/PROTOCOL.md` is the public surface that versioning applies to.

## Unreleased

### Added

- **`cost --max-cost <USD> --on-breach <warn|abort>` (#77).** A budget gate over
  the report's total. The compared figure is the priced total, which is a *lower
  bound* whenever any line is unpriced — a detected breach has therefore already
  been exceeded by an unknown margin, which is why warn-vs-abort is a required
  flag rather than a default. The verdict ships in `data.budget`
  (`{maxUsd, onBreach, basis, breached, overageAtLeastUsd}`), in both text
  renderings, and as a stderr warning `--quiet` does not swallow; `abort` exits
  `ERR_PRECONDITION` (12, an existing row — no new exit vocabulary) after the
  full report is written, so a CI gate still receives the figures it is gating
  on.

- **`microvm shell` — an interactive PTY into a running VM (#69).** A thin
  client over the platform's shell WebSocket (Option B): `session_init`, then
  binary terminal bytes both ways, resize as a validated JSON control frame,
  close code 1000 as the clean end. No exit-status channel exists on the wire,
  so the command reports transport facts and never fakes an exit code. Control
  frames are validated client-side before sending, because the server injects
  unrecognized control frames into the shell as literal keystrokes.
- **`run --shell` and the `shell` `microvm.toml` key.** A launch that wants a
  shell requests the measured `[HTTP_INGRESS, SHELL_INGRESS]` connector pair
  (replacing `ALL_INGRESS`, which the platform refuses to combine with finer
  ingress). The Python and JS SDKs gain the matching `shell` parameter.
- **Connector variants `HTTP_INGRESS` and `SHELL_INGRESS` (#114).** Requested
  sets are validated client-side before anything launches or bills:
  `ALL_INGRESS` cannot combine with finer ingress, and `SHELL_INGRESS` without
  `HTTP_INGRESS` is refused.
- **`run --auto-resume` (#68).** The platform resumes a suspended VM on an
  incoming request; flag, `auto-resume` toml key, and wire field
  `idlePolicy.autoResumeEnabled`. Its idle-timer interaction ships as a
  live-conformance scenario pending its first run.

### Changed

- **`microvm logs` succeeds with the working read command (#79).** The command
  used to exit `ERR_PRECONDITION` with the `aws logs tail` invocation as a
  suggestion on the failure; it now succeeds, printing the build log group, the
  runnable `aws logs tail <group> --since 1h --format short` command
  (`data.tailCommand`), and the version floor (`data.tailRequires`): the
  subcommand is **AWS CLI v2 only** — it does not exist in v1 (verified present
  in 2.35.7). `data.lines` stays explicitly `null`, never `[]`, so a consumer
  cannot mistake "this client did not read the group" for "the group has no
  events". The CLI still carries no CloudWatch client; both thinness guards are
  untouched.

### Added

- **`ls --watch` (#78).** Fleet watching on the command that already enumerates
  it, `port-forward` style: snapshots on stderr, one summary envelope at the end
  (`data.watch`), Ctrl-C exits 0; `--interval-sec` (default 2, floor 0.1) and
  `--max-refreshes` bound it for scripts. The loop is ledger-only — zero
  platform calls per refresh, no `/v1/health` — so it resets no idle timer and
  bills nothing, and the output says so in both machine (`data.watch.calls`)
  and human form, with the measured reason: an outside `/v1/health` poll DOES
  reset the idle timer (a polled VM stayed RUNNING through 311s of a 60s window
  while the unpolled control suspended at 66s, `docs/PLATFORM.md`), so a
  health-polling watcher would keep every watched VM alive and billing. There
  is deliberately no `status` subcommand.

- **Terraform read grant for build logs (#79).** `conformance/infra/main.tf`
  now ships a standalone managed policy granting `logs:FilterLogEvents`,
  `logs:GetLogEvents`, and `logs:DescribeLogStreams` on
  `/aws/lambda-microvms/*`, exported as `logs_read_policy_arn`. Attach it to
  the identity that runs `aws logs tail`; previously those three actions were
  granted to nobody, so the printed command failed with
  `AccessDeniedException` on a fresh install.

## [0.5.0] — 2026-08-30

### Added

- **Self-provisioning daemon: `microvm run --exec "…"` needs no binary path.** `run`
  and `build` with no positional resolve `agentd` themselves: `$MICROVM_AGENTD`, then
  the version-matched cache under the state directory, then a fetch of the CLI's own
  version's release asset — `gh` + `gh attestation verify` when available (provenance),
  `curl` + the release's `SHA256SUMS` otherwise (integrity, fail-closed). Fetched bytes
  pass the same aarch64 ELF gate `doctor` runs before they are cached; the install is
  partial-write-then-rename so an interrupted download cannot poison the cache; and the
  run/build envelopes report `{path, source, verified}` under a new `agentd` key. The
  fetch is a subprocess behind a scripted-in-tests seam — the CLI still carries no HTTP
  client, and every existing behavioral guard now also proves its command never fetches.

- **`microvm quickstart` (#75).** Zero to a live exec: provision the daemon, build,
  launch, run a hello-world (or `--exec`), report the cost, tear everything down. It is
  `run` with every decision pre-made — the arguments come from parsing `run`'s own
  command line, so the defaults cannot drift — and its envelope keeps the `microvm.run`
  discriminant. Prebuilt cross-account base images (the other half of #75) are declined:
  image ARNs are account-scoped (`docs/PLATFORM.md`) and the measured API carries no
  image-sharing operation, so the provisioned default build is the portable equivalent.

- **Host CLI release assets and `cargo binstall microvms-cli` (#76).** The release now
  ships `microvm-<target>` archives for Linux x86_64/aarch64 (cargo-zigbuild, glibc 2.17
  floor), macOS arm64/x86_64, and Windows x64 — each attested with the same sigstore +
  intoto pair the daemon asset carries — plus one `SHA256SUMS` over every binary asset
  (the file the provisioning curl fallback verifies against). `crates-io` publishing now
  also waits on the CLI builds, keeping every buildable failure ahead of the first
  irreversible publish. Homebrew is deliberately absent: a tap is a second repository to
  maintain, and binstall + `cargo install` cover the same machines.

## [0.4.0] — 2026-08-30

### Added

- **Configurable build-log destination with a per-build stream discriminator (#98).**
  `microvm build`/`run` accept `--log-group`/`--log-stream` (and `log-group`/`log-stream`
  in `microvm.toml`), sent as the image-build `logging` config. The platform treats
  `logStream` as an **exact** stream name — no prefix support — so a fixed name would
  collapse every build's three streams (docker build, snapshot per chipset generation)
  into one; this client therefore never sends the configured name verbatim and appends
  `/<16 hex>` fresh per create attempt. The build envelope reports the resolved stream,
  `microvm logs` now labels the three-VM build topology in `data.streams`, and
  `docs/PLATFORM.md` records the measured finding. Verified live (us-east-1,
  2026-08-30): a configured build produced exactly `suite-build/<16 hex>` in the
  configured group, non-empty, with no verbatim-named stream; four permanent
  conformance checks cover the surface and the suite deletes the group it creates.

- **Lifecycle-hook observations in health and history (#80).** agentd records every
  platform hook invocation (ready/validate/run/suspend/resume/terminate) in memory —
  capped at 64, first-N-wins, drops counted — and reports them on `GET /v1/health`
  (`hooks`/`hooks_dropped`, `#[serde(default)]` for old daemons). The CLI appends
  unseen observations to the per-VM history JSONL as `hookObserved` events at three
  moments (`run` teardown, `resume`, `microvm health`), deduplicated on
  (hook, firedAt). No guest-printed byte reaches a history line, preserving the
  forgery property; the additive caveat — the unauthenticated loopback hook routes let
  a workload *add* observations, never remove real ones — is documented in
  PROTOCOL.md and the history docs. Verified live (us-east-1, 2026-08-30): the run
  hook appeared on health with a wall-clock-bracketed timestamp, suspend/resume
  observations surfaced after the thaw, history carried the cycle exactly once each,
  and a build-time `ready` observation survived from the snapshot VM's memory into
  the launched VM — the snapshot-survival design expectation, now measured.

### Changed

- **Sizing vocabulary matches the confirmed platform model (#99).** The service team
  confirmed (2026-08) there is no vertical scaling: a VM is provisioned at 4x the
  requested minimum from the start, the minimum is the bill floor (25%), above-minimum
  usage bills by consumption, and nothing resizes. The repo's "burst" vocabulary —
  which implies a scaling event that does not exist — is replaced throughout
  (`sizing.rs`, CLI help, both bindings, `docs/PLATFORM.md`, README) with
  always-provisioned language, and README's "What it costs" gains the one-paragraph
  rule: min = floor, 4x min = fixed ceiling, so peaky agent workloads should pick a
  low minimum and let peaks ride the always-present headroom. The size JSON gains a
  static `headroomMib` (peak − baseline, read from the table per TRAP-13, never
  computed). `SizeClass::DEFAULT` stays 2048, re-argued around the ceiling. The cost
  engine's formula is unchanged — baseline per second, a labelled lower bound, since
  above-minimum consumption is real but unobservable to this client. A new example
  `examples/coding-agents-on-bedrock/microvm.toml` ships the low-minimum sizing rule
  (`memory = 1024`), loader-tested.

- **Dependency rollup (2026-08-29).** 62 lockfile entries moved; two manifest bumps and one
  deliberate hold, plus the toolchain:
  - `tokio-tungstenite` 0.24 → 0.30 in `microvms-core` and agentd's dev-dependencies, which
    also collapses the tree to one tungstenite line (axum already carried 0.29+). The 0.30
    API takes `Bytes` in `Message::Binary`; call sites converted. Verified live: the layer-3
    identity handshake and verified relay ran against a real VM through the real endpoint
    proxy on the new client (1 served, 0 refused), and the account was left clean.
  - `astro` 7.2.7 → 7.2.9 and `@astrojs/starlight` 0.41.9 → 0.41.10 in the docs site.
  - **Held: `x25519-dalek` stays 2.x.** The 3.0 line moves to `curve25519-dalek` 5 while
    `snow` 0.10 still pins 4.x, so taking it puts two copies of the same curve arithmetic in
    the tree — the exact duplication the crate was chosen to avoid (one x25519
    implementation shared with the Noise resolver). The hold is recorded in both manifests;
    revisit when snow bumps. Tried, built green, and reverted on the tree shape rather than
    on a failure.
  - **Held: `typescript` stays 5.9.** Astro's language server (`@astrojs/check`) asserts a
    compatible TypeScript major at startup and refuses 7.x outright — `docs:check` fails at
    `AstroCheck.assertCompatibleTypeScript`. Tried and reverted on the measured refusal.
  - `rustup update stable`: the local toolchain moved 1.97 → 1.98, closing the gap that let
    two `result_large_err` lints reach CI before being visible locally.
  - Notable transitives: `h2` 0.4.16 → 0.4.19, `rustls-webpki` 0.103.13 → 0.103.15,
    `hyper` 1.11.0 → 1.11.1, the whole `icu` 2.2 → 2.3 family. `cargo deny check` green
    (advisories, bans, licenses, sources); full local gate green; conformance self-test
    41/41.

### Added

- **`microvm port-forward` (#70, layer 1).** `port-forward LOCAL[:GUEST]` binds a local
  listener and serves a guest port through the VM's endpoint: each connection is re-issued
  with the port-scoped proxy headers minted per hop through the session's existing
  `ProxyAuth` cache, which is what carries a tunnel across the platform's 60-minute token
  ceiling — `proxyTokenMints` in the envelope is the observable that says a refresh
  happened. HTTP is what this command carries today; the WebSocket upgrade path relays a 101
  and splices bytes correctly against a local upstream but **does not yet reach a guest
  server through the endpoint proxy** (see below). The proxy's
  403-vs-502 pair — scope mistake vs nothing listening — is answered to the local client as
  a readable response naming which one it was. The relay lives in
  `microvms-core::session::forward` because the CLI's thinness guard forbids an HTTP stack
  in the binary; the CLI reaches it through the `ForwardClient` newtype. Ctrl-C ends the
  tunnel and exits 0, `--max-connections` bounds a scripted check, and the caller's own
  `X-aws-proxy-*` headers are stripped rather than forwarded — the minted pair is the only
  pair. Raw TCP is deliberately not this command (issue #70 layer 2: it needs a guest-side
  relay agentd does not yet serve).

  Verified live (us-east-1, 2026-08-29): a guest `python3 -m http.server 8080` answered
  through `port-forward 18080:8080 --name pf-smoke` with the correct body, a guest 404
  passed through unreinterpreted, one token minted across three connections, and a forward
  to a dead port answered the local client with the 502 nothing-listening sentence.
  Teardown confirmed by `live:verify-clean`.

- **`microvm tunnel` — arbitrary TCP to a guest port (#70, layer 2).** Each local connection
  becomes a WebSocket carrying raw TCP to a new bearer-authed `GET /v1/tcp?port=<n>` route on
  the daemon, which dials `127.0.0.1:<port>` inside the guest. So `psql`, `ssh`, or any other
  wire protocol reaches a server in the VM. The endpoint proxy has no CONNECT method and
  refuses an upgrade replayed over its HTTPS path, so riding inside binary frames is the only
  way arbitrary TCP crosses it — and that binary frames survive a port-scoped token byte-exact
  is measured rather than assumed (see below).

  Three deliberate limits, each with its reason in the code. **No multiplexing:** one
  WebSocket carries one TCP connection, because a mux protocol would reimplement per-stream
  ids, flow control, and close handshakes that the platform already provides per connection.
  **Loopback only:** the relay never resolves a name, so a stolen agent token cannot turn the
  daemon into an open proxy inside the VM. **The dial happens after the upgrade**, because on
  the endpoint path every WebSocket failure a caller can observe is 1006 with no reason — an
  HTTP 502 for a dead port would be invisible, so a refusal arrives as close code 4502 naming
  the port instead. The codes live in `protocol::tunnel::close` (RFC 6455's private range, so
  they cannot be confused with the platform's 1006), and the marker subprotocol is one `const`
  aliased across crates rather than two that can drift — the client offers it and the daemon
  echoes it, and an offered-but-unechoed subprotocol makes an RFC 6455 client refuse the
  handshake outright.

  Verified live (us-east-1, 2026-08-29) against a length-prefixed binary protocol in the
  guest — deliberately not HTTP and not line-based, so framing, byte fidelity, and
  request/response interleaving on one connection are all exercised. Through
  `tunnel 15434:5432`: an ascii round trip, a `0x00`/`0xFF` payload byte-exact, 200 KiB across
  the chunk boundary, and a second independent connection all passed on one minted token.

  One bug the live run caught, which no local test would have: the client minted its token
  scoped to the **guest** port. `subprotocols(guest_port)` reads correct — every other
  port-scoped call in the crate names the port it wants to reach — but this request terminates
  at the daemon, and the daemon dials the guest port from inside the VM, so the proxy only ever
  sees a request for the daemon's port. A token scoped to 5432 authorized a port the request
  never addressed, and the refusal was close code 1006 with no reason: indistinguishable from a
  dead server. Fixed to `subprotocols(auth.port())`, with the reason recorded at the call site
  and a regression test that fails if it is reverted.

  Verified with 10 daemon-side tests over a real WebSocket against a real TCP server, and 8
  client-side end-to-end tests: bytes reach an upper-casing server and come back transformed
  (an echo could pass without leaving the process), a `0x00`/`0xFF` payload survives byte-exact
  in both directions, 200 KiB crosses the 64 KiB chunk boundary in order, an unauthenticated
  or wrong-token upgrade is refused 401, a dead port closes 4502, `?port=0` closes 4400, and
  two tunnels to one port get independent connections. Four guards were deliberately broken to
  confirm they fail: the route's `Auth::Bearer`, guest-to-client byte fidelity, framing
  re-derivation, and upgrade detection.

- **Tunnel identity: `run --identity` + `tunnel --verify-identity` (#70, layer 3).** The
  tunnel can now prove, cryptographically and without trusting the endpoint proxy, that the
  far end is the exact VM the caller launched — with every frame after the proof encrypted
  end to end. At launch the host generates two x25519 seeds and delivers the VM's seed plus
  the host's public key in the `runHookPayload` beside the agent token (137 bytes of the
  measured 4096-byte budget, asserted by test against the real composition). The daemon
  derives its key in memory, installs it under the same one-shot bootstrap rule as the token
  — only the winning bootstrap installs identity — and `?identity=true` on `/v1/tcp` runs a
  Noise KK handshake before the guest dial. What persists on the host is least-privilege by
  construction: the registry record and the `run` envelope carry the host's secret and the
  VM's **public** pin, and the VM's own secret is dropped the moment the payload is built,
  so nothing outside the VM can ever impersonate it.

  **The issue specified rustls with SPKI pinning; this ships Noise KK instead, on a measured
  constraint.** Both rustls crypto providers (`aws-lc-sys` and `ring`) compile C through
  `cc-rs`, which requires `aarch64-linux-musl-gcc` for the daemon's static
  `aarch64-unknown-linux-musl` shipping target — a tool absent on the devbox and in CI,
  whose workflow installs the *gnu* cross compiler (measured 2026-08-29: both providers fail
  the release build outright). snow's pure-Rust resolver cross-compiles with no C at all,
  and `KK` is the stronger fit anyway: both static keys are pre-known (the host *generated*
  them), a wrong key fails the handshake's own decryption rather than depending on verifier
  code that could forget a check, and there is no CA, hostname, or certificate machinery to
  hold wrong. `default-features = false` on snow is load-bearing — its `std` feature spells
  `ring/std`, not `ring?/std`, so enabling it force-enables the C dependency. The full
  argument lives in `protocol/src/identity.rs`; the honest ptrace limit ("this proves the
  right VM, not an uncompromised VM") is in `docs/TRUST.md`'s new tunnel-identity section.

  Fails closed on every path in #70's acceptance list, each pinned by a test: a VM launched
  without a seed refuses `identity=true` with close code 4401 rather than downgrading; a
  caller holding the agent token but not the host key is refused 4403 (the token is a bearer
  credential the proxy transports — it must not be enough); a pin from a different VM — the
  replayed-record case — fails the handshake with a diagnosis naming the pin; and a refused
  caller never causes a guest connection (the handshake runs before the dial, asserted by an
  accept-counting listener). Key material stays out of the image artifact by construction —
  the seed is delivered at launch, after the snapshot — and out of child environments via
  the existing `env_clear` guarantee; `Debug` renderings of every identity-bearing type are
  tested to print no key bytes.

  Verified with 5 daemon-side end-to-end tests over real WebSockets (encrypted round trip
  through an upper-casing server, both refusal codes, no-downgrade, no-dial-on-refusal), 2
  client-side tests against a wire-contract stand-in (verified round trip, wrong-pin
  fail-closed), 8 unit tests of the host-side material lifecycle, and a compile-time guard
  proven able to fail: raising the verified chunk to 64 KiB — which would make every
  encrypted send return `Error::Input` at runtime, since plaintext plus the 16-byte tag must
  fit one 65535-byte Noise message — is a build error, watched red before shipping. One
  RFC 6455 trap found by test: a close frame's reason is capped at 125 bytes, and the
  daemon's original refusal sentence exceeded it, so tungstenite reported
  `ControlFrameTooBig` and the caller saw a transport error instead of the 4403 code. The
  wire reason is now short; the full sentence lives in `close::explanation`.

  Verified live (us-east-1, 2026-08-29): `run --keep --identity` delivered the seed through
  the real run hook and reported the pair on the envelope and in the registry;
  `tunnel 18443:9000 --verify-identity` completed the Noise handshake through the real
  endpoint proxy and served the daemon's schema document decrypted end to end (1 served,
  0 refused, 1 mint); one flipped pin character mid-base64 failed closed with nothing
  served and the handshake named in the error; the restored record verified again; a
  seedless launch was refused locally with `ERR_PRECONDITION` when the record carried no
  material, and refused by the daemon (close 4401, "launched without an identity seed")
  when material was pasted at it anyway. Both VMs terminated with nothing leaked, the
  account verified empty, and the smoke image deleted. `drive_tunnel_identity` keeps six
  of these checks in the live suite permanently (104 checks total).

### Measured

- **The endpoint proxy refuses an upgrade replayed over the HTTPS path, and carries binary
  frames on a port-scoped `wss://` handshake** (us-east-1, 2026-08-29, guest RFC 6455 echo
  server on 8090). Re-issuing a client's `GET` with `Upgrade: websocket` as request headers
  is answered `400` by the proxy, with the guest logging no handshake, while the identical
  handshake from inside the guest to `127.0.0.1:8090` answers 101. On the endpoint the
  WebSocket credential travels as `Sec-WebSocket-Protocol` values rather than as the two
  proxy headers, so a working tunnel must open a real `wss://` handshake offering
  `connect_subprotocols(port)`'s three values. Doing that: the handshake answers **101**,
  the proxy consumes all three values (the guest saw no `sec-websocket-protocol` header),
  and **binary** frames survive **byte-exact** both ways on a **port-scoped** token —
  including a `0x00`/`0xFF` payload a silent utf-8 round trip would corrupt, and a 300-byte
  frame exercising the extended-length header (guest observed opcode `2`). This closes the
  open question `docs/PLATFORM.md` left between the SHELL_INGRESS binary-frame finding
  (portless shell token) and the 2026-08-15 text-frame run, and it is the transport premise
  the raw-TCP tunnel (#70 layer 2) rests on.

## [0.3.0] — 2026-08-29

### Added

- **Project config file (#73).** A per-project `microvm.toml` beside the invocation (or
  named via `--config <PATH>`; `--no-config` ignores any file): every key already exists
  as a `run` flag, so the file adds no capability, only persistence — `microvm run` in a
  configured project needs zero flags. Precedence is flag > file > built-in default,
  decided per knob, with "the flag was typed" read off the parse rather than the value;
  the success envelope's new `resolvedConfig` key reports each knob's winning value and
  source (`flag`/`config`/`env`/`default`). Unknown keys are refused by name, and every
  domain the matching flag enforces is validated in the loader — the memory size table,
  the region closed set, the eight-hour duration ceiling, env key shapes, artifact-glob
  compilation — so `doctor` (which validates through the same loader, as its new first
  check) and `run` cannot disagree about a file. A file that cannot be used is the
  **appended exit row 15 / `ERR_CONFIG`**, refused locally before any billable call. A
  typed `BINARY` positional suppresses the file's `image` (a file that silently won that
  pair would run the caller's tests against a stale pinned image), and a relative
  `binary` in the file resolves against the file's own directory.

- **`run <DIR>` sync mode (#72).** A positional that names a directory switches `run`
  into pack-run-collect: the tree is packed deterministically (`.git`, `target`,
  `node_modules`, `.venv` skipped whole; sockets/fifos/devices skipped individually;
  symlinks preserved as links), uploaded to `/workspace`, the exec runs with
  `/workspace` as its working directory, and the members matching the config file's
  `artifacts` globs come back afterwards — including when the exec failed, because a
  failing run's report is the artifact CI most wants. The pack is budgeted against the
  daemon's own caps (512 MiB, 100 000 members) during the walk, refusing by name before
  any archive bytes exist. Extraction writes only glob-matched regular-file members
  through `unpack_in`'s traversal refusal — and never under `.git`, because a
  workload-written hook would execute on the host. A download failure never discards the
  exec's result (it lands in `sync.error`); with no globs configured the workdir is not
  downloaded at all. Local pack/extract failures are the **appended exit row 16 /
  `ERR_SYNC`**. The envelope gains a `sync` key. `microvms-core` gains
  `Sandbox::with_session_backend`, the daemon-side sibling of `with_control_plane`'s
  transport seam (test-only; the default backend is unchanged).

  Both verified live (us-east-1, 2026-08-28) and under permanent conformance coverage:
  `drive_config_and_sync` launches its VM entirely through a microvm.toml and asserts
  the round trip in eight named checks (the suite is 98, no SKIP).

## [0.2.0] — 2026-08-28

### Added

- **Named VMs (#67).** `run --keep --vm-name <NAME>` registers a local name→VM record
  beside the run ledger; every identifier-taking command then accepts the name. The
  lifecycle positionals (`suspend`, `resume`, `terminate`, `history`) resolve it
  client-side following the `run --image <name>` pattern — prefix passthrough with zero
  extra calls, a local `ERR_PRECONDITION` on a miss. The attached commands (`exec`,
  `health`, `ack`, `stdin`, `cp`) take `--name` as a stand-in for the whole
  `--endpoint`/`--agent-token`/`--microvm-id` triple, launch region included, so one word
  replaces four pasted values.

  Reusing a live name is refused locally with the **appended exit row 14 /
  `ERR_NAME_TAKEN`** — its own row because the remedy differs from `ERR_INVALID_ARG`
  (terminate the holder, or pick another name, rather than edit the flag), and the first
  row with no core `ErrorKind` behind it, since the registry is the CLI's own file. The
  refusal costs zero billable calls, asserted by a behavioral guard that counts seam
  doors. An accepted terminate releases the name by VM id, whichever spelling addressed
  it. The registry lives at `<state-dir>/names/<NAME>.json`, one file per live name,
  owner-only on Unix because the record carries the agent token; a torn record reads as
  *taken*, because its VM may still be billing.

  Verified live (us-east-1, 2026-08-28): the full round trip — register, exec/health by
  name, collision, suspend/resume by name, terminate-by-name with the name released —
  and now covered permanently by `drive_named_vm` in `conformance/run_rs.py` (5 checks;
  the suite is 90).

  The first live run caught what every scripted test missed: the fixtures spell MicroVM
  ids `mvm-*`, the real service spells them `microvm-*`, and a resolution path keyed on
  the fixture prefix refused every real id. Both prefixes are now excluded from the name
  grammar, the grammar test pins the real prefix with the measurement date, and
  `CLAUDE.md` carries the policy this earned: the local gate is the definition of done
  for a change, a live run is the definition of done for a task.

- **Five `microvm` subcommands, and the live coverage they unlock.** `microvm health`,
  `microvm ack`, `microvm stdin`, `microvm cp` (with `--tar` and `--mode`), and five new
  shapes on `exec` — `--exec-id` for an idempotent retry, `--poll` for a read-only status
  read, `--detach` to start without waiting or acking, `--stream` for output as it arrives,
  `--stdin` for feeding a child. All five go through `microvms-core`'s existing session
  surface and through the one `attach_session` seam; no daemon, protocol, or core change was
  needed, which is what `docs/CLI-COVERAGE-PLAN.md` predicted and is the reason it was one
  task rather than four.

  `--detach` was added after the first live round rather than designed in, and it is worth
  its own note: every other `exec` shape ends in start-wait-**ack**, and that ack releases
  the output irreversibly — a second one is a 409 and a later poll reports `acked` with
  nothing. A caller who wants to own an exec's lifecycle (start now, poll later, ack when
  ready, possibly from another process, since the record lives in the VM) needs a start that
  stops after starting.

  `cp --tar` is asymmetric and the asymmetry is the design: the `vm:` side is a **directory**
  that the daemon packs or extracts through `/v1/fs/tar`, and the local side is a `.tar`
  **file**, because neither this binary nor `microvms-core` carries a tar library — which
  keeps the daemon's confined extractor the only extractor in the system. Nothing outside the
  daemon packs or unpacks, including the guest, whose base image may have no `tar` at all.

  `exec --stream` is the one documented exception to "exactly one envelope on stdout": it
  emits NDJSON — one event object per line, the envelope last — under a **different**
  discriminant, `microvm.exec.stream`. `microvm manifest` publishes the alternate shape as
  `exec`'s `alternateResponse` and states the exception in its `conventions`, so a consumer
  discovers it rather than encountering it. Stream chunks are the command's *output*, not
  progress, which is why they cannot go on stderr; buffering them to keep stdout one
  document would remove the only reason to stream.

  This added a direct dependency on `futures-util` for the `Stream` trait
  `ExecHandle::stream_with` returns, with a paragraph in `tests/thinness.rs` justifying it
  and a note that a callback driver in `microvms-core` was the preferred fix. That fix has
  since landed — see `ExecHandle::for_each_event` below — and the dependency is gone again,
  so the CLI is back to six direct dependencies.

- **A unit-test tier for both bindings.** 198 Python tests across five files plus a
  `conftest.py`, and 152 Node tests across four files plus a `support/` directory, organized
  to mirror `src/` module for module: cost, errors, exec, region/session. Where the two
  pre-existing smoke files assert what the surfaces *refuse* — no `valueOf`, no `__float__`,
  no client-token parameter — these assert what they **answer**, which is not the same
  coverage: a binding whose `EstimatedUsd` correctly coerces to `NaN` can still report the
  wrong dollar figure or omit a line item, and neither throws anything. Cost figures are
  compared as exact decimal strings (BigInt in Node, `Decimal` in Python) rather than parsed
  to a float, because parsing would perform the laundering step the types exist to prevent.

  The exec/stream surface gets real behavioural coverage rather than argument checks, through
  an offline SSE server on loopback in each suite: scripted frame lists, one list per attach,
  so a two-element script *is* a cut-and-reconnect and the offset a second attach asks for is
  assertable. Both helpers state their own boundary rather than leaving it to be discovered —
  the frame shapes are that suite's transcription of what `microvms-core/src/session/sse.rs`
  parses, so nothing there proves `agentd` emits them; if the daemon's framing changed these
  would stay green while the conformance suite went red. `microvms-js/package.json` gains
  `test:smoke` and `test:unit` so the two tiers run apart; CI's `bindings` job already globbed
  the directory, so no workflow change was needed.

  Two pre-existing JS defects were found by writing them, both now fixed with the measurement
  recorded at the site. `stream()` was spawning onto `napi::tokio::spawn` — tokio's own
  `spawn`, which needs an *ambient* runtime — from a synchronous `#[napi] fn` called on the JS
  main thread, producing `there is no reactor running` and then `fatal runtime error: failed
  to initiate panic`: a panic across the FFI boundary, which takes Node with it. It is now
  `napi::bindgen_prelude::spawn`, onto napi's managed runtime, which needs no ambient context.
  And a stream rejection was rebuilt as a bare `napi::Error` from the error's reason string,
  which dropped the cause chain and left `err.cause.message` `undefined` on the one rejection
  a caller is most likely to branch on — while `src/errors.rs` documents `cause.message` as
  the uniform rule. It goes through `js_async` now, like every other path. `__test__/exec.mjs`
  is the regression for both.

- **`ExecHandle::for_each_event`** in `microvms-core`: a callback driver over the same
  reconnecting stream state machine `stream_with` runs, taking a
  `FnMut(ExecEvent) -> ControlFlow<()>` and returning a `StreamEnd` that names *why* the
  stream ended — `Exited`, `Stopped` (the callback broke), or `Cut` (a body with no terminal
  event) — plus core's own cursor to resume at. Both types are `std`, so a consumer no longer
  has to name the crate that defines `Stream` in order to advance one.

  `microvm exec --stream` moved onto it and `microvm`'s direct dependency on `futures-util`
  came out with it; `microvms-cli/tests/thinness.rs` now asserts that edge stays out and
  names the replacement API in its failure message. The bindings could not make the same
  move, for a reason about backpressure rather than about the trait — see
  `ExecHandle::for_each_event_async` below, which is what let them.

- **`ExecHandle::for_each_event_async`** in `microvms-core`: `for_each_event` for a callback
  that **awaits**. Same loop, same `advance` state machine, same three endings; the only
  difference is that the per-event callback answers a future which is `.await`ed before the
  next attach read starts — so a slow consumer slows the stream rather than buffering behind
  it, which is what makes it usable as backpressure. `for_each_event` is now written in terms
  of it through `std::future::ready`, so the ordering, the cursor read, and the `Break` arm
  have one implementation rather than two that agree until one is edited.

  This is what the bindings were waiting for. Both consume a stream by pushing into a
  **capacity-1** channel a foreign-language iterator drains, and capacity 1 means the channel
  is full whenever the host consumer is even slightly behind — normally. With a synchronous
  callback the only available send is `blocking_send`, which would park the runtime worker the
  driver itself runs on; `send(..).await` inside the callback yields it instead. Both bindings
  moved onto it and `futures-util` came out of both manifests, with its absence documented
  where the dependency used to be. Four tests cover the new driver, the load-bearing one being
  a capacity-1 channel with a deliberately slow consumer that loses none of five events.

  The signature is `FnMut(ExecEvent) -> Fut` and **not** `AsyncFnMut`, which is measured rather
  than stylistic: a caller cannot `tokio::spawn` a drive that uses `AsyncFnMut` without the
  unstable `async_fn_traits` feature, because proving the returned future `Send` requires
  naming `F::CallRefFuture<'a>` under a `for<'a>` bound. Both bindings spawn this drive, so
  that is not a corner they can avoid. The cost is a per-event `Sender::clone` instead of a
  borrow — one atomic increment — and it is written down beside the signature.

- **`impl FromStr for CostPhase`** and **`CostPhase::ALL`** in `microvms-core`. Both
  bindings judged a bare phase string against their own hand-written seven-element table —
  two parallel lists over one closed enum. They now parse through core, and a phase added to
  the enum appears in the refusal message without an edit. A round-trip test covers every
  variant by exhaustive match, so adding one without adding it to `ALL` fails to compile.

- **`RateTable::minimum_retention_days`**, so the floored-storage note reads its day count
  off the rate row instead of dividing `as_secs()` by 86,400 beside the message.

### Fixed

- **A per-port credential was minted for the wrong port, so `connect_headers` and
  `connect_subprotocols` handed back credentials the proxy refuses.** Both accessors reused
  the session's cached proxy token, which the control plane scopes at mint time to the agent
  port alone — so they answered a correct-looking `X-aws-proxy-port: 8080` (or
  `lambda-microvms.port.8080`) behind a JWE authorizing only 9000. Found by the first live
  WebSocket run, 2026-08-15: the handshake failed with close code **1006** and the HTTPS form
  with **403 `Access to port denied`**.

  Invisible to every local test, and the reason is worth stating: the strings were right.
  Format assertions, cache-reuse assertions, and the mint-count assertions all passed,
  because the defect is in a fact only the service knows. Three of those tests had in fact
  written the bug down as a requirement — "resolving a port endpoint burned a second
  control-plane call" was recorded as a *failure* message, when a second mint is the fix.

  `ops::PortSpecification` is now the model's real tagged union (`port`, `range`,
  `allPorts`), `ControlPlane::mint_auth_token_for` takes a scope, `TokenMinter` grew
  `mint_for_ports`, and `ProxyAuth`'s cache records which ports its token covers so a cache
  hit means "fresh **and** in scope". A new port extends the scope rather than replacing it,
  so warming the cache for a workload port does not evict the session's own — one extra mint
  per new port, cached thereafter. Measured after the fix: the handshake opens, frames flow
  both ways, and a wrong port still fails as 1006. `docs/PLATFORM.md` carries the
  four-token table.

- **The live tier's three suites ran in parallel, and each of the three orderings was
  wrong.** `mise run live` had `depends = ["live:rates", "live:conformance-rs",
  "live:verify-clean"]`, which mise runs concurrently. So `live:verify-clean` finished at
  t=5s of a 228-second run and reported "account us-east-1: clean" about an account nothing
  had touched yet — a false assurance in the one line a caller reads to decide whether to go
  looking for a leaked MicroVM. `live:rates`, which is free and creates nothing, raced the
  billable suite it could abort, since mise kills the surviving siblings when one member
  fails. And nothing ordered the marker write last.

  The three are now sequenced in the task body: suite, then rates, then the leak check in a
  `trap` so it also runs when the suite **fails**, which is when resources survive. mise has
  no `if: always()` — `depends_post` runs only on success and a `wait_for` chain stops at the
  first failure, both measured — so a shell trap is the only always-run primitive available,
  and it is what `.github/workflows/live-conformance.yml` says with `if: always()`.

- **`mise run live` failed at the very end of a green run, in a worktree.** The last line
  wrote its live-verified marker to the literal `.git/agentd-last-live-run`, and in a
  worktree `.git` is a *file* — so the redirect failed with "Not a directory" after all 76
  checks had passed and the account was clean. The pre-push reader in `lefthook.yml` had the
  same literal and failed more quietly and worse: it took its "no live AWS run recorded in
  this clone" branch, which is indistinguishable from the truth. Both now resolve the path
  through `git rev-parse --git-path`, which answers both repo shapes.

- **The conformance suite created a CloudWatch log group it could not delete and its own
  leak check called a leak.** Naming it is the client's job — neither `microvms-core` nor the
  CLI carries a CloudWatch client, by design — but nothing picked up the other half, so five
  groups accumulated across five runs and `mise run live` could not be green on a clean
  account no matter what the code did. The suite now deletes the groups the teardown report
  named, through the boto3 client it already builds to read the daemon's logs. The client
  still refuses CloudWatch; the thing that created the group is the thing that removes it.

- **`scripts/check-live-wiring`**, a new offline gate in `mise run check` over all three of
  those tier defects: the marker path round-trips in a plain clone *and* a worktree, and the
  `live` body — read out of `mise.toml` and run against stubbed members — is exercised green
  and red to prove the leak check runs after the suite, runs when the suite fails, runs
  exactly once, and that the marker records successes only. Free and offline, because the
  alternative is finding the next one of these a billable run at a time.

### Changed

- **The security job's four scanner downloads are version-pinned and checksummed.** betterleaks
  1.7.3, syft 1.50.0, grype 0.116.1, and osv-scanner 2.5.0, each fetched at an exact release tag
  and verified with `sha256sum -c -` before it runs. What they were: two `curl … install.sh | sh`
  off a `main` branch, a `releases/latest` tarball piped straight into `tar xz`, and a
  `releases/latest` binary written to `/usr/local/bin` — four unverified executables in the two
  jobs whose entire output is a supply-chain assurance, and the one debt cluster in the repo with
  no acceptance written anywhere. Thirty lines of the same file reason carefully about which
  action major is on Node 24; that care now reaches the tools those jobs actually execute.

  The cost is stated where the pins are: refreshing one means updating the version *and* the hash
  from that release's `checksums.txt`, by hand, because this repo still has no Dependabot. That is
  the same gap `ci.yml`'s header paragraph names for `setup-uv`, and it applies to five pins now
  rather than one. The trade is deliberate — an unpinned fetch is a live hole, a stale pin is a
  delayed patch — but it is a trade.

- **`microvms-cli` drops its direct `protocol` dependency**, naming the wire types through
  `microvms_core::protocol::` instead. The edge existed while core lacked the re-export and was
  kept afterwards because resolution is identical either way; its own manifest comment called
  dropping it "a fine future cleanup", and this is that. The crate now has exactly one door to
  everything below it, `tests/thinness.rs`'s `ALLOWED` table is six entries, and the entry that
  went away is the one whose reason string still claimed "core does not re-export it" — false
  since `microvms-core/src/lib.rs` grew `pub use protocol;`. Worth noting what did *not* change:
  the only assertion that guard makes over a reason is `reason.len() > 25`, so the mechanism that
  let a reason go stale is intact. This instance closed; the class did not.

- **Every `#[allow]` in the tree carries a `reason =` string.** Two did not — `too_many_arguments`
  on `agentd`'s `super_wait` and `type_complexity` on the turmoil harness's response-head parse —
  in a codebase where the other twelve all did. Both now argue their specific case rather than
  restating the lint.

- **Four filesystem-failure swallows gained or were confirmed to have a local reason.** The debt
  register had counted five unreasoned `let _ = std::fs::…` sites; a per-site read found two of
  them already reasoned (`identity.rs`'s pre-create `remove_file`, whose failure mode *is* the
  expected case, and `fs.rs`'s replace-on-extract, which exists so a retried partial extraction
  converges). The two that genuinely lacked one — `Ledger::clear` and `doctor`'s `TempFile::drop`
  — now have it, the second because a panic inside `Drop` during unwind is an abort, which makes
  swallowing the only correct choice rather than the convenient one.

- **Every file citing the retired Python oracle by line number says how to reach it.** Sixteen
  files carry `cli.py:<line>` citations that resolve only at `c4d396e^`, and the recovery hash was
  written down once, in a document marked as history. Each now carries a one-line anchor naming
  the `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` that resolves it. The line
  numbers stay historical on purpose: they cite a file that no longer exists, and rewriting one to
  point at live code would be a claim about code that never carried the argument.

- **CI runs on Node 24 actions throughout.** `checkout@v5`, `setup-uv@v7`,
  `upload-artifact@v6`, `setup-node@v5`, `setup-terraform@v4` — the first major of each on
  the runtime that replaced the Node 20 the runner deprecated, so every green run is now
  warning-free instead of printing a deprecation notice per step. `ci.yml`'s header records
  why `checkout` stops at v5 and why `setup-uv` stays on a rolling major. `enable-cache:
  false` on every `setup-uv` step, because there is no lockfile in this repo to key a cache
  on — every Python entry point is a `uvx` invocation or a PEP 723 script — and a cache that
  can never be invalidated would pin the boto3 whose bundled service model the drift gate
  exists to read fresh. `configure-aws-credentials@v4` was the one step still on Node 20, and
  it was the last: the rolling v6 tag has since landed on node24 — checked against the tag's
  own `action.yml` — and `live-conformance.yml` is on `@v6`. No step in the repo is on a Node
  20 action.

- **Three accepted debts now carry their reasons in the code**, rather than in a session
  document a reader cannot open: why there is no `Sandbox::attach` for the three attached
  lifecycle commands (`microvms-cli/src/commands/lifecycle.rs` — adding one would
  manufacture a second initial state, and both the symspec and stateright models declare
  exactly one, so their proofs would stop covering it); why `microvm logs` refuses to read
  CloudWatch rather than growing a reader (`commands/local.rs` — a second signing name and
  host in a transport whose single-service-ness is four readable constants, for a read no
  role in `conformance/infra` is granted); and why the JSON envelope's dollar strings may
  differ in trailing zeros from the retired Python oracle's (`render.rs` — numerically equal,
  `rust_decimal` normalizes scale differently, and rescaling would round a figure whose
  exactness is why it is a string).

- **`conformance/run_rs.py` expresses every named check.** The `UNSUPPORTED` table and its
  `unsupported()` primitive are gone: all 34 entries became real live check bodies under
  the names `conformance/run.py` gave them, so this suite's report diffs line for line
  against the last recorded oracle run in git history — `SKIP` there, `PASS` here. The four
  hostile archives are hand-built with `tarfile` (GNU tar sanitizes several of them) and
  handed to `microvm cp --tar`; the expected failure is the **daemon's** refusal surfacing
  as `data.kind: ProtocolError`, because the CLI deliberately does not pre-validate an
  archive and a byte-scan guard proves it.

  75 checks rather than the plan's 72: two of the old 38 were weak readings off the launch
  envelope and are now asserted directly against `microvm health`, and three checks are
  new. `Results.skipped` and a `skip()` primitive remain with no live caller, exercised by
  `--self-test`, because a suite that removed its own ability to report a gap is a suite
  whose next gap is silent.

  The first live round found seven failures across two clusters, both in this driver rather
  than in the five subcommands: a tar chain that shelled out to a `tar` binary the base image
  does not have (deleted — it tested the image's tooling) and pointed `--tar` at a file where
  the route wants a directory, and a start/poll/ack sequence that could not be expressed
  because `exec` acked its own output. The second is what `--detach` exists for. Fixed and
  re-verified offline; the live tier is rerun by the orchestrator.

### Removed

- **`clients/python`** — the Python client, its 83 tests, and the two conformance
  scripts that imported it (`conformance/run.py`, the 56-check oracle;
  `conformance/probe_oom.py` and `conformance/probe_suspend_resume.py`). It was
  the discovery instrument: it found and closed fifteen client-side API traps and
  measured the platform's pricing and lifecycle semantics. All of that is pinned
  elsewhere now — in `docs/PLATFORM.md`, in `spec/core.symspec.json`, and in
  `microvms-core`'s own guards — and the Rust port has driven the live suite green
  against real AWS on the same commit the oracle last passed on. Git history keeps
  every line.

  What moved rather than went away: the rate-drift check is now
  `scripts/check-live-rates` (a PEP 723 uv script with its own pinned table, held
  equal to `microvms-core`'s `pinned_rates()`), and `scripts/check-model-drift`
  pins the region list and sizing table against its own literals, since those two
  values were verified by the Python-vs-Rust cross-comparison and by nothing else.

  What was genuinely lost, and has since been recovered: 34 of the oracle's 56 checks
  had no live coverage, because the `microvm` CLI had no `cp`, `ack`, `exec --stream`,
  `stdin`, or `health` subcommand, and `conformance/run_rs.py` reported each one as SKIP
  by name. Those five subcommands landed in the same Unreleased cycle (see Added above)
  and every SKIP became a real check — so the loss lasted one release and is recorded
  here rather than edited out, because "the CLI grew the subcommand" is the outcome the
  SKIP list existed to make actionable.

## [0.1.0] — 2026-08-06

First release. Source only: there are no published binaries, and the daemon is
built from this tree.

### Added

- **`agentd/`** — the daemon, a static binary intended to run as the container
  `CMD`. One-shot token bootstrap from the platform's `/run` hook, authorization
  decided before any request body byte is read and compared in constant time on
  bytes, idempotent exec with caller-minted ids and ack-then-collect output
  capture, streaming tar upload and download with CPython `data`-filter parity,
  SSE output streaming with a byte-offset cursor, and opt-in stdin as a separate
  request with explicit EOF.
- **Operational guards** — panic recovery so a panicking handler cannot take the
  only channel into the VM with it (`panic = "unwind"` is deliberate and
  documented in `Cargo.toml`), a disk-pressure guard that refuses a write before
  it starts rather than surfacing ENOSPC mid-stream, and identity repair for VMs
  restored from a shared image (`/etc/machine-id`, hostname, `boot_id`,
  `random-seed`).
- **`model/`** — stateright model over every reachable bootstrap and exec state:
  seven safety properties plus six coverage properties, and a second
  configuration that deliberately breaks the deployment invariant and asserts the
  attack path is found.
- **`spec/`** — symspec requirements document for bootstrap and authorization,
  reporting `verified: true` under `--strict` via Z3.
- **`docs/PROTOCOL.md`** — the wire contract, with every rule traced to the
  defect that bought it.
- **`docs/PLATFORM.md`** — measured AWS behavior, each entry carrying its date,
  region, and API version.
- **`docs/schema.json`** — generated protocol schema, with a CI staleness check
  (`cargo run -p agentd --bin schema -- --check`).
- **`clients/python`** — `Session` and `ExecHandle` speak the wire protocol with
  no AWS dependency; `Sandbox` wraps the AWS lifecycle. Handles proxy-token
  minting across the 60-minute JWE ceiling, stream reconnect at the last good
  offset, and a typed error taxonomy separating a retryable 503 from a fatal 401.
- **`conformance/`** — the live suite against real Lambda MicroVMs plus a
  standalone suspend/resume probe, and the Terraform stack they need.
- **CI** — fmt, clippy `-D warnings`, `cargo test --all`, the schema staleness
  check, an `aarch64-unknown-linux-musl` cross-compile, and symspec strict.

### Verified

- **56 conformance checks passed, none failed, teardown clean** — 2026-08-05,
  us-east-1, API version `2025-09-09`. A 1.41 MB static `aarch64-musl` binary
  baked into a MicroVM image as the container `CMD` and driven through every
  protocol rule via the platform's own endpoint, including SSE surviving the
  endpoint proxy, stdin round-tripping through a child, and a suspend/resume cycle.
- **155 Rust tests across six targets and 83 Python client tests**, green as of
  2026-08-06. Every guard was verified to fail against the code without its fix.
- Two rounds of live runs found five defects no local tier could have caught, all
  of them wrong assumptions about the service rather than bugs in the daemon's
  logic: lifecycle hooks live under a fixed `/aws/lambda-microvms/runtime/v1/`
  prefix, `ready` and `validate` are called at image-build time, `runHookPayload`
  arrives wrapped in an envelope rather than as the request body, network
  connectors are ARNs, and `CreateMicrovmAuthToken` returns a header map. Each is
  in `docs/PLATFORM.md` with its date, and the transport tier was corrected so it
  fails against the old behavior.
- Suspend/resume is a freeze and restore, not a stop and start: the in-memory
  agent token, the filesystem, exec records, and running background processes all
  survive, and the endpoint URL is unchanged. This inverted what the project had
  assumed, and the daemon's resume-hook docstring had claimed the opposite by
  reasoning from where state lives rather than measuring it.

### Not yet

- **One region.** Every live measurement is us-east-1. Nothing has been re-run
  elsewhere, so no `docs/PLATFORM.md` entry should be assumed regional-invariant.
- **`Sandbox` is verified only against fakes plus the conformance run.** The
  protocol layer (`Session`, `ExecHandle`) has unit coverage; the AWS lifecycle
  wrapper has one live path and no test suite of its own.
- **No published binaries.** No release artifacts, no crates.io publish, no PyPI
  publish. CI uploads an `aarch64-musl` build artifact per run; that is not a
  release.
- **CI had never executed against a remote** when this was written. It has since:
  every job in `.github/workflows/ci.yml` runs on push and is green, cross-compile
  included. The symspec job is the one that is still unproven, and for a different
  reason — it is not in the workflow at all, because the version this repo needs is
  not installable from a registry (the comment at the end of `ci.yml` says so).
- **No fork or process-tree snapshot**, and none planned — see
  `docs/STRATEGY.md` for why it is unavailable above the hypervisor.
- **No orchestrator, no PTY, no AgentCore parity.** Deliberately out of scope.

# S3 prefetch at image build: pay the first S3 call once, in the build

Issue #81 records a measured 5–10 second penalty on a fresh VM's first S3
call, endorsed in-channel by the platform team as worth designing around
(that conversation is the figure's source; this repo's docs do not carry the
measurement). For a workload that starts by pulling model weights, a dataset,
or a toolchain from S3, that penalty — plus the whole transfer — lands on
every launch.

This recipe moves both to image-build time. The build fetches the S3 prefix
into `/opt/prefetch` while the snapshot VM is starting up, the snapshot is
captured *after* that finishes, and every VM launched from the image starts
with the data already on disk. The launched VM makes no S3 call at all — the
demo launches without `--egress` to prove it.

```bash
# from the repo root, with the Getting-started prerequisites in place:
PREFETCH_URI=s3://my-bucket/models/ bash examples/s3-prefetch-at-build/run.sh
# public bucket, zero credentials:
PREFETCH_URI=s3://a-public-bucket/prefix/ PREFETCH_NO_SIGN=1 bash examples/s3-prefetch-at-build/run.sh
```

## The mechanism, and why it is safe to rely on

An image build boots the image in a snapshot VM, calls the build-time hooks
(`/ready`, `/validate`) against the daemon, and captures the memory and disk
snapshot only after they answer — three in-repo sources pin the ordering:

- `agentd/src/state.rs` (the hook log): "build-time hooks (validate, ready)
  fire in the snapshot VM **before the snapshot is taken**, so their records
  ride the memory image into every VM launched from it."
- `agentd/src/routes.rs` (`ready_hook`): "the snapshot is taken after this
  fires."
- `docs/PLATFORM.md` (failed-build shapes): a build whose ready hook timed
  out has `codeInstallSizeInBytes` and **no snapshots** — the snapshot does
  not exist until the hooks have answered.

So anything that completes before the daemon answers those hooks is inside
the snapshot. This wrapper (`/start.sh`, materialized in the Dockerfile)
runs the `aws s3 sync` first and only then `exec`s agentd — strictly
ordered: the daemon cannot answer a hook until the sync is done, and the
snapshot cannot be captured until the daemon answers.

A note on the issue's phrasing, "prefetch inside the `/validate` hook":
agentd's `/validate` handler is not user-extensible (it records the firing
and answers 200 — and per the platform-primitives constraint, agent- and
workload-specific behavior stays out of platform code). Running the prefetch
immediately before the daemon starts serving hooks uses the same build-time
window and is captured by the same snapshot; if you ship your own daemon
instead of agentd, doing the work inside your `/validate` handler is the
equivalent placement, with up to 3600 seconds of budget
(`docs/PLATFORM.md`, hook-timeout table).

Two shapes of the same rule, observed in `docs/PLATFORM.md`:

- **Budget.** The model allows build hooks 3600 seconds, but an observed
  failure reads `Ready hook invocation timed out after PT5M` — plan the
  transfer to fit well inside five minutes, and measure before assuming the
  full hour is reachable.
- **Twice per build, never at launch.** One build runs the snapshot pass for
  each chipset generation (Graviton 3 and 4), so the prefetch downloads
  twice per image. A launch *restores* the snapshot rather than re-running
  `CMD`, so it never prefetches — that is the entire point.

## Fail loud, not empty

The wrapper is `set -e`: a failed sync means agentd never starts, the ready
hook times out, and the **build fails with a named reason** instead of
producing an image whose snapshot silently lacks the data. The failure
signature is `stateReason: Ready hook invocation timed out after PT5M` on
the **build** record (`docs/PLATFORM.md` documents which of the three shapes
carries the reason), and the wrapper's own log lines are in the build log
group — `microvm logs` prints the `aws logs tail` command that reads them. An empty `PREFETCH_URI` skips the prefetch with a log line,
so the Dockerfile also builds as checked in; `run.sh` refuses to run without
a URI so the demo cannot "pass" by fetching nothing.

## Credentials at build time, honestly

The prefetch runs inside the build's snapshot VMs, under whatever network
and credentials the build environment provides. What this repo has actually
established: the build's **docker-build** VM demonstrably has outbound
network (every `dnf install` and `npm install` in these examples runs
there). Whether the **snapshot** VMs have the same egress, and whether the
build role's credentials are visible to the app process there, has not been
measured here.

- `PREFETCH_NO_SIGN=1` (public bucket) is the zero-assumption mode: no
  credential chain involved.
- For a private bucket, the AWS CLI walks its default chain. Grant the
  build role read on the prefix and try it; if the build fails with the
  PT5M signature and the wrapper's log shows a credential or network error,
  that is a real finding about the snapshot VMs — worth an issue.

An alternative with a proven network path: run the sync as a docker `RUN`
step instead of in the wrapper. It executes in the docker-build VM (where
outbound network is demonstrated), lands in the image filesystem, and
reaches the launched VM through the same disk snapshot. The wrapper
placement is the one the issue asks for and the one that generalizes to
warm-up work that must happen at app start; the `RUN` placement is the
conservative fallback if your build environment turns out not to give the
snapshot VMs egress.

## Do not prefetch secrets

The snapshot is shared by every VM launched from the image. Anything
prefetched into it is readable by all of them — the same reasoning the
platform docs give for delivering per-VM secrets through `runHookPayload`
at launch instead of baking them into a shared snapshot
(`docs/PLATFORM.md`, "Traffic ordering around the `/run` hook"). Prefetch
data, models, and toolchains; deliver tokens and keys at launch, the way
[coding-agents-on-bedrock](../coding-agents-on-bedrock/README.md) does.

## Rebuilds and cost

`run.sh` bakes the URI into the Dockerfile it builds, so the image name —
keyed to the Dockerfile's content hash — changes when the URI does: same
URI reuses the already-prefetched image in seconds, new URI builds fresh.
The prefetched bytes live in the image snapshot, which bills storage with a
one-week minimum retention, and every launch pays a snapshot read scaled by
its size — so prefetch what the workload reads at startup, not the whole
bucket. Delete retired images with `aws lambda-microvms delete-microvm-image`.

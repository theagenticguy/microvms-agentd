// SPDX-License-Identifier: Apache-2.0
//! The command tree, and the closed sets that make CLI-5 a parser property.
//!
//! # CLI-5 is enforced here or nowhere
//!
//! "No option permits a value microvms-core rejects." The CLI is where an S1 guard is most
//! easily downgraded: `--memory 1500` typed as `u32` compiles, parses, and fails at a
//! library boundary a whole build cycle later. So every parameter whose library counterpart
//! is a closed type is a [`clap::ValueEnum`] here — [`MemoryMib`] over the five documented
//! baselines, [`RegionArg`] over the five measured regions — and clap refuses the rest
//! before any handler runs.
//!
//! The domains are **spelled out** rather than generated from
//! [`microvms_core::SizeClass::ALL`], for the same reason `cli.py:1447` spells its `Literal`:
//! a domain computed at runtime is invisible to `--help`, to shell completion, and to the
//! manifest's `choices` field, and the static half of the guarantee is the half worth
//! having. The cost of writing it twice is that the two can disagree, so
//! [`tests`] asserts the enum's domain equals the size table — a sixth class that does not
//! reach this file fails there rather than shipping unreachable.
//!
//! # What is deliberately *not* an option
//!
//! No `--client-token` (TRAP-1: core has no such parameter, so there is nothing to forward).
//! No `--capabilities` (TRAP-3: the intent is `--repair-identity`, and core injects
//! `["ALL"]` itself). No `--connector` (TRAP-4: the intent is `--egress`). No
//! `--architecture` (the model's enum has one value, so a flag could only express a rejected
//! request). Their absence is asserted by
//! `tests/thinness.rs` and by the manifest cross-check: an option added later that carries a
//! free-text S1 value shows up as a `choices: null` on a parameter the test names.
//!
//! # The escape hatch is a separate flag, not a permissive parser
//!
//! [`microvms_core::Region::unlisted`] exists because AWS adds regions faster than a
//! constant is re-read, and it costs the caller the null-message diagnostic. So it is
//! `--unlisted-region <NAME>`, conflicting with `--region`, with the cost in its help text —
//! rather than `--region` accepting a free string and quietly widening. A reader of a command
//! line can see that someone opted in.
//!
//! (cli.py line numbers resolve at `git show 'c4d396e^:clients/python/src/microvms_agentd/cli.py'` — the retired oracle.)

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use microvms_core::{Region, SizeClass};

/// `microvm`: a working sandbox in one command, and nothing microvms-core does not do.
#[derive(Debug, Parser)]
#[command(
    name = "microvm",
    version,
    propagate_version = true,
    about = "Build, run, and tear down AWS Lambda MicroVMs. A thin layer over microvms-core.",
    long_about = "Build, run, and tear down AWS Lambda MicroVMs.\n\nEvery AWS call and every \
                  trap guard belongs to microvms-core; this binary parses, renders, and exits \
                  with a code. `microvm manifest` emits the whole surface — commands, option \
                  domains, exit codes, envelope schema — derived from the command tree rather \
                  than written down.",
    // clap's own usage errors conventionally exit 2, which is this catalog's
    // ERR_INVALID_ARG. The two agree deliberately: `try_parse` maps the failure into an
    // envelope, and a caller who reads $? sees the same number either way.
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Emit the typed JSON envelope on stdout instead of human output.
    ///
    /// Wins over every other format, including an interactive terminal: an agent that asked
    /// for JSON gets JSON.
    #[arg(long, global = true)]
    pub json: bool,

    /// Token-lean output, for a consumer paying per token.
    #[arg(long, global = true)]
    pub dense: bool,

    /// Suppress progress on stderr. Warnings still print.
    #[arg(long, global = true)]
    pub quiet: bool,
}

/// The sixteen commands.
///
/// Variant order is the order `microvm --help` and the manifest list them in, which is
/// lifecycle order rather than alphabetical: a reader meeting this surface for the first time
/// wants `run` first, not `build`. The five *attached* commands — `exec`, `health`, `ack`,
/// `stdin`, `cp` — sit together after `build` because they share the same three identifiers and
/// the same door ([`crate::seam::CoreSeam::attach_session`]), which is the distinction that
/// matters when reading the list: everything above them creates or destroys, and everything in
/// that block addresses a VM that already exists.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build an image, launch a VM, run a command, report the cost, tear it down.
    ///
    /// The whole sequence as one command. Tears down by default: a CLI that leaves a
    /// billable VM running because someone closed a laptop is worse than no CLI. `--keep`
    /// opts out and prints the identifiers you have just taken responsibility for.
    Run(RunArgs),

    /// Build a MicroVM image and wait for it to be usable.
    ///
    /// Separate from `run` for the case where one image serves many launches, which is the
    /// shape that matters once a build is 45 minutes. Nothing is torn down here: an image is
    /// the durable artifact, and its one-week minimum snapshot retention means deleting it
    /// early saves nothing.
    Build(BuildArgs),

    /// Run one command in a MicroVM that is already running.
    ///
    /// The loop shape: launch once with `run --keep`, then exec against it. Needs the three
    /// identifiers `run --keep` printed, because a session holds no server-side state —
    /// every exec record and the bootstrap token live in the VM, so reattaching is just
    /// naming it.
    Exec(ExecArgs),

    /// Ask a running MicroVM's daemon whether it is up, and what its identity repair did.
    ///
    /// The one unauthenticated route: the platform forwards no external traffic until the run
    /// hook returns 200, so reaching this at all implies bootstrapped — but `identityDegraded`
    /// and `diskUnderPressure` are conditions no other command reports, and both are reasons to
    /// drain a VM rather than to keep scheduling work onto it.
    Health(HealthArgs),

    /// Release a finished exec's buffered output, which starts its collection clock.
    ///
    /// `exec` acks for you. This exists for the detached shape — a process that started an exec
    /// with `--exec-id` and came back later, possibly as a different process — where the ack is
    /// what hands over the output. A second ack is a 409, because the first one released it and
    /// answering 200 with an empty body would read as "the command produced no output".
    Ack(AckArgs),

    /// Write to a running exec's stdin, and optionally close it.
    ///
    /// Only for an exec started with `exec --stdin`; one started without it has `/dev/null` on
    /// its stdin and this answers 409. Nothing else closes the pipe: the daemon's copy outlives
    /// the child's own `wait()`, so a child blocked reading stdin hangs until its timeout unless
    /// someone sends `--eof`.
    Stdin(StdinArgs),

    /// Copy a file or a tar archive between here and a running MicroVM.
    ///
    /// `cp ./local vm:/remote` writes, `cp vm:/remote ./local` reads. `--tar` moves a whole
    /// directory tree instead of one file: the `vm:` side is then a directory the daemon packs or
    /// extracts, and the local side is a `.tar` file — because neither this binary nor
    /// `microvms-core` carries a tar library, which keeps the daemon's confined extractor the only
    /// extractor in the system.
    Cp(CpArgs),

    /// Freeze a MicroVM. It keeps its memory, filesystem, token, and endpoint.
    ///
    /// A freeze and restore, not a stop and start — measured, not assumed. A suspended 2 GB
    /// VM pays snapshot storage of about $0.16 a month against roughly $100 running, which
    /// is what makes a warm pool viable. `microvm cost --compare` prints the break-even
    /// hold, because each cycle also pays a snapshot write plus a read.
    Suspend(SuspendArgs),

    /// Thaw a suspended MicroVM and report its endpoint.
    ///
    /// The launch-time `suspendedDurationSeconds` window terminates a suspended VM once it
    /// passes, so "resume later" silently stops working and the VM is gone rather than slow.
    Resume(ResumeArgs),

    /// Tear down a MicroVM, and optionally its image and build log group.
    ///
    /// Never fails on a teardown failure — it reports the identifier instead. An identifier
    /// you can read is the only remedy for a resource that would not delete, and an image in
    /// CREATING cannot be deleted at all.
    Terminate(TerminateArgs),

    /// List what this CLI created and could not confirm it deleted.
    ///
    /// Reads the local ledger rather than asking AWS. Deliberately: the question it answers
    /// is "what did I leave behind", and the resources worth asking about are the ones a
    /// killed process never got to report — which no ListMicrovms call can attribute back to
    /// a command that died.
    Ls(LsArgs),

    /// Name an image's build log group, which is where a failed build's only evidence lives.
    ///
    /// The group is `/aws/lambda-microvms/<image-name>`, derived from the name rather than
    /// asked for: a build role granted the plausible-but-wrong `/aws/lambda/microvms/*`
    /// produces builds that write no logs at all, and every failure then reads
    /// `reason=unknown`.
    Logs(LogsArgs),

    /// What a run cost, or what a plan will cost. Every figure labelled.
    ///
    /// Dollars are estimates derived from published rates and never an invoice — only Cost
    /// Explorer knows the bill. Seconds from a real run are labelled measured; `--estimate`
    /// labels every duration projected. A line item with no published rate reads `unpriced`,
    /// never `$0.00`.
    Cost(CostArgs),

    /// Check every prerequisite and say which one is wrong.
    ///
    /// The command that saves an hour on a first attempt. Credentials, the region the
    /// connector ARN is interpolated into, the three Terraform outputs, whether the stack is
    /// applied, and whether the daemon binary is aarch64 — that last one being the failure
    /// that otherwise surfaces as a run-hook timeout, 45 minutes into a build, saying
    /// nothing about architecture.
    Doctor(DoctorArgs),

    /// Emit the whole command surface, its exit codes, and its envelope schema.
    ///
    /// Derived from the registered command tree rather than written down, so it cannot drift
    /// from what this binary actually accepts. Defaults to JSON: the only consumer that asks
    /// for a manifest is one that parses it.
    Manifest(ManifestArgs),

    /// Emit every service constraint this client believes, for the drift gate.
    ///
    /// TRAP-12's second source. `scripts/check-model-drift` compares this against the pinned
    /// botocore model and against the Python client's own constants, which is the only check
    /// available for a value no API answers.
    Constants(ConstantsArgs),
}

// ── the closed sets (CLI-5) ─────────────────────────────────────────────────

/// The five documented size-class baselines, as a set the parser enforces.
///
/// `minimumMemoryInMiB` selects a class and does not size a VM (TRAP-10), and
/// [`microvms_core::SizeClass::from_baseline_mib`] refuses anything off-table with the
/// finding attached. An option typed `u32` would accept 1500 and reach that refusal a call
/// later; this one cannot express it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum MemoryMib {
    #[value(name = "512")]
    Mib512,
    #[value(name = "1024")]
    Mib1024,
    #[value(name = "2048")]
    Mib2048,
    #[value(name = "4096")]
    Mib4096,
    #[value(name = "8192")]
    Mib8192,
}

impl MemoryMib {
    /// The core class this baseline selects. Infallible: the mapping is exhaustive.
    pub fn size_class(self) -> SizeClass {
        match self {
            MemoryMib::Mib512 => SizeClass::Mib512,
            MemoryMib::Mib1024 => SizeClass::Mib1024,
            MemoryMib::Mib2048 => SizeClass::Mib2048,
            MemoryMib::Mib4096 => SizeClass::Mib4096,
            MemoryMib::Mib8192 => SizeClass::Mib8192,
        }
    }
}

/// The five regions measured to carry MicroVMs.
///
/// An unsupported region answers `AccessDeniedException` with a **null message**, which is
/// indistinguishable from a genuine IAM denial — so the last point where the diagnostic
/// survives is before the first call, and for a CLI that means the parser. `--unlisted-region`
/// is the named way out.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RegionArg {
    #[value(name = "us-east-1")]
    UsEast1,
    #[value(name = "us-east-2")]
    UsEast2,
    #[value(name = "us-west-2")]
    UsWest2,
    #[value(name = "eu-west-1")]
    EuWest1,
    #[value(name = "ap-northeast-1")]
    ApNortheast1,
}

impl RegionArg {
    pub fn region(self) -> Region {
        match self {
            RegionArg::UsEast1 => Region::UsEast1,
            RegionArg::UsEast2 => Region::UsEast2,
            RegionArg::UsWest2 => Region::UsWest2,
            RegionArg::EuWest1 => Region::EuWest1,
            RegionArg::ApNortheast1 => Region::ApNortheast1,
        }
    }
}

/// The region flags, flattened into every command that talks to AWS.
///
/// One struct rather than two fields per command, so the `conflicts_with` relationship
/// between the closed set and the escape hatch is declared once and cannot be forgotten on
/// the twelfth command.
#[derive(Args, Debug, Default)]
pub struct RegionFlags {
    /// AWS region. Defaults to $AWS_REGION, then $AWS_DEFAULT_REGION, then us-east-1.
    #[arg(long, value_enum)]
    pub region: Option<RegionArg>,

    /// Use a region this client has not seen carry MicroVMs. Costs you the diagnostic.
    ///
    /// An unsupported region answers AccessDeniedException with a null message, which reads
    /// as an IAM denial. Use this when AWS has launched MicroVMs somewhere new.
    #[arg(long, value_name = "NAME", conflicts_with = "region")]
    pub unlisted_region: Option<String>,
}

/// The three identifiers, plus the port, that address a VM this invocation did not launch.
///
/// One struct rather than four fields repeated on six commands, and the reason is the same one
/// [`crate::seam::Attach`] gives for being a struct: three of the four are opaque strings of the
/// same shape, so `--endpoint`/`--agent-token`/`--microvm-id` written out per command is six
/// chances to document one as another. Flattened, every attached command's triple is declared
/// once — and `tests/manifest.rs` sees the same parameter names on all six, which is what makes
/// "the attached commands take the triple `exec` takes" a checkable claim.
#[derive(Args, Debug, Default)]
pub struct AttachFlags {
    /// The VM's endpoint, as reported by `run`.
    #[arg(long)]
    pub endpoint: String,

    /// The agent token delivered to the VM at launch.
    #[arg(long)]
    pub agent_token: String,

    /// The MicroVM id, needed to mint the endpoint proxy token.
    #[arg(long)]
    pub microvm_id: String,

    /// The daemon's port inside the guest.
    #[arg(long)]
    pub port: Option<u16>,
}

/// The three account-specific values the AWS commands need.
#[derive(Args, Debug, Default)]
pub struct InfraFlags {
    /// S3 bucket for the build artifact. Defaults to $MICROVM_BUCKET.
    #[arg(long)]
    pub bucket: Option<String>,

    /// Build role ARN. Defaults to $MICROVM_BUILD_ROLE_ARN.
    #[arg(long)]
    pub build_role_arn: Option<String>,

    /// Execution role ARN. Defaults to $MICROVM_EXECUTION_ROLE_ARN.
    #[arg(long)]
    pub execution_role_arn: Option<String>,
}

// ── per-command arguments ───────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct RunArgs {
    /// The aarch64 agentd binary to bake in as the image CMD.
    ///
    /// Ignored when --image names an image to launch instead of building one.
    #[arg(value_name = "BINARY")]
    pub binary: Option<PathBuf>,

    /// Launch this existing image instead of building one.
    ///
    /// The loop shape once a build is 45 minutes: `microvm build` once, then `run --image`
    /// as often as you like.
    #[arg(long, value_name = "IDENTIFIER")]
    pub image: Option<String>,

    /// Where the build artifact already is, as an s3:// URI.
    ///
    /// microvms-core builds the artifact bytes and takes the URI, but does not upload — S3
    /// is not in its dependency set. Pass this when you have uploaded already; pass --bucket
    /// to have the artifact uploaded with the `aws` CLI.
    #[arg(long, value_name = "S3_URI")]
    pub artifact_uri: Option<String>,

    /// A shell command to run in the VM.
    ///
    /// Omitted launches and tears down, which is how you check that an image boots at all.
    #[arg(long, value_name = "COMMAND")]
    pub exec: Option<String>,

    /// Image name. Defaults to a per-invocation name, because reusing one is how a
    /// clientToken replay wedges an image.
    #[arg(long)]
    pub name: Option<String>,

    /// Baseline MiB, which selects a documented size class.
    ///
    /// Defaults to the platform's own 2 GB rather than the cheapest class: baseline is also
    /// the floor of the burst range, and 0.5 GB OOM-kills a real test suite to save about
    /// three cents an hour.
    #[arg(long, value_enum, default_value = "2048")]
    pub memory: MemoryMib,

    /// A Dockerfile to use instead of the library's default. Its FROM must match the base.
    #[arg(long)]
    pub dockerfile: Option<PathBuf>,

    /// Widen the guest so `sethostname` and the boot_id bind mount work.
    ///
    /// Root in the guest is not enough for either — the MicroVM drops CAP_SYS_ADMIN.
    #[arg(long)]
    pub repair_identity: bool,

    /// Give the VM outbound network. Omitted by default — a daemon needs none.
    #[arg(long)]
    pub egress: bool,

    /// Leave the VM and image running. You are then paying for them.
    #[arg(long)]
    pub keep: bool,

    /// How long to wait for the exec, in seconds.
    #[arg(long, default_value_t = 300.0)]
    pub timeout: f64,

    /// Suspend the VM after this much inbound-traffic idleness.
    #[arg(long, default_value_t = 600)]
    pub max_idle_sec: u32,

    /// Terminate the VM after this long suspended. A resume past it cannot work.
    #[arg(long, default_value_t = 600)]
    pub suspended_sec: u32,

    /// Hard ceiling on the VM's life. Refused above 28800 (eight hours) before any call.
    #[arg(long, default_value_t = 3600)]
    pub max_duration_sec: u32,

    /// The daemon's port inside the guest.
    #[arg(long)]
    pub port: Option<u16>,

    /// Where the run ledger is written. Defaults to $MICROVM_STATE_DIR or ~/.microvm/runs.
    #[arg(long)]
    pub state_dir: Option<PathBuf>,

    #[command(flatten)]
    pub region: RegionFlags,

    #[command(flatten)]
    pub infra: InfraFlags,
}

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// The aarch64 agentd binary to bake in as the image CMD.
    #[arg(value_name = "BINARY")]
    pub binary: PathBuf,

    /// Where the build artifact already is, as an s3:// URI. See `run --artifact-uri`.
    #[arg(long, value_name = "S3_URI")]
    pub artifact_uri: Option<String>,

    /// Image name. Defaults to a per-invocation name.
    #[arg(long)]
    pub name: Option<String>,

    /// Baseline MiB, selecting a documented size class.
    #[arg(long, value_enum, default_value = "2048")]
    pub memory: MemoryMib,

    /// A Dockerfile to use instead of the library's default.
    #[arg(long)]
    pub dockerfile: Option<PathBuf>,

    /// Widen the guest so `sethostname` and the boot_id bind mount work.
    #[arg(long)]
    pub repair_identity: bool,

    /// The daemon's port inside the guest.
    #[arg(long)]
    pub port: Option<u16>,

    #[command(flatten)]
    pub region: RegionFlags,

    #[command(flatten)]
    pub infra: InfraFlags,
}

#[derive(Args, Debug)]
pub struct ExecArgs {
    /// A shell command to run in the VM. Omitted only with --poll.
    #[arg(value_name = "COMMAND", required_unless_present = "poll")]
    pub command: Option<String>,

    /// How long to wait for the command, in seconds.
    #[arg(long, default_value_t = 300.0)]
    pub timeout: f64,

    /// Working directory.
    ///
    /// Omitted inherits the image WORKDIR, which is not the same as passing `/` — most
    /// public ARM64 bases declare none.
    #[arg(long)]
    pub cwd: Option<String>,

    /// Use this exec id instead of a fresh one, making a retry idempotent.
    ///
    /// # This is TRAP-1's shape, inverted, and the inversion is the point
    ///
    /// The default is a generated id, and that default is deliberate: `microvm exec` is one shot,
    /// and an id reused by accident means the second invocation is answered from the first's
    /// record and the caller reads someone else's output. So the *stable* id is the opt-in.
    ///
    /// What it buys is the only thing an idempotency key ever buys: a retry that is safe across
    /// the caller's own restart. The daemon returns success for a known id **without spawning a
    /// second child** (`agentd/src/exec.rs:366`, decided under the registry lock), so a harness
    /// whose process died between sending the start and reading the answer can send the identical
    /// start again and get the original exec rather than a duplicate.
    ///
    /// Note what this is *not*: `--client-token` on a control-plane call, whose replay wedges an
    /// image permanently and which this CLI therefore does not have at all. This key addresses an
    /// exec record inside one VM, and the failure it prevents is a double-spawn.
    #[arg(long, value_name = "ID")]
    pub exec_id: Option<String>,

    /// Read an existing exec's status and output instead of starting anything.
    ///
    /// Read-only server-side, so it is safe to spin on and safe to repeat. A running exec answers
    /// OK with `phase: running` — polling is not a failure, and an exit code of `null` with that
    /// phase is the honest report of "not finished yet". Does not ack, so the output stays
    /// readable; `microvm ack` is what releases it.
    #[arg(long, value_name = "ID", conflicts_with_all = ["exec_id", "stream", "stdin", "cwd", "detach"])]
    pub poll: Option<String>,

    /// Start the command and return immediately, without waiting and **without acking**.
    ///
    /// # Why this is a separate flag rather than `--timeout 0`
    ///
    /// The default shape is start-wait-ack, which is right for one shot: the caller wants output
    /// and the ack is what releases it. But that ack is also the thing a caller cannot undo — a
    /// second `microvm ack` is a 409, and a poll afterwards reports `acked` with no output at all,
    /// because the daemon released it (`agentd/src/exec.rs:429`).
    ///
    /// So a caller who wants to own an exec's lifecycle — start now, poll later, ack when ready,
    /// possibly from a different process — needs a start that stops after starting. `--timeout 0`
    /// would not do it: that still waits (for zero seconds) and still acks, so it would report a
    /// timeout on a healthy exec and consume nothing.
    ///
    /// Prints the exec id and `phase: running`. Pair it with `--exec-id` to know the id in advance;
    /// without one the envelope's `execId` is the only place the generated id appears, and a
    /// detached exec whose id was not captured cannot be polled or acked by anyone.
    #[arg(long, conflicts_with_all = ["stream", "stdin"])]
    pub detach: bool,

    /// Stream output as it arrives rather than waiting for the whole thing.
    ///
    /// Under `--json` or into a pipe this writes **NDJSON**: one JSON object per event on stdout,
    /// then the envelope last. That is the one documented exception to the one-envelope rule and
    /// it is declared in the manifest as `responseType: microvm.exec.stream` — stream chunks are
    /// *output*, not progress, so they cannot go to stderr, and buffering them to keep stdout a
    /// single document would defeat the only reason to stream.
    #[arg(long)]
    pub stream: bool,

    /// Resume a stream at this byte offset. Only with --stream.
    ///
    /// The cursor the daemon replays from. A caller that read to offset N and lost its connection
    /// passes N and receives exactly what it has not seen — which is what makes an interrupted
    /// stream resumable rather than a choice between losing the tail and re-reading everything.
    #[arg(long, value_name = "BYTES", requires = "stream")]
    pub from_offset: Option<u64>,

    /// Give the command a stdin pipe and feed it this process's stdin, then close it.
    ///
    /// Opt-in because a child holding an open stdin pipe nobody will ever write to is a child
    /// that blocks forever the first time it reads. EOF is sent once local stdin ends, and
    /// nothing else closes the pipe — the daemon's copy outlives the child's own `wait()`.
    #[arg(long)]
    pub stdin: bool,

    #[command(flatten)]
    pub attach: AttachFlags,

    #[command(flatten)]
    pub region: RegionFlags,
}

#[derive(Args, Debug)]
pub struct HealthArgs {
    #[command(flatten)]
    pub attach: AttachFlags,

    #[command(flatten)]
    pub region: RegionFlags,
}

#[derive(Args, Debug)]
pub struct AckArgs {
    /// The exec whose output to release.
    #[arg(value_name = "EXEC_ID")]
    pub exec_id: String,

    #[command(flatten)]
    pub attach: AttachFlags,

    #[command(flatten)]
    pub region: RegionFlags,
}

#[derive(Args, Debug)]
pub struct StdinArgs {
    /// The exec to write to. Must have been started with `exec --stdin`.
    #[arg(value_name = "EXEC_ID")]
    pub exec_id: String,

    /// What to write. `-` reads this process's stdin; omitted writes nothing.
    ///
    /// Raw bytes either way — core base64-encodes them for the wire, so a caller never has to,
    /// and a caller who did would have their encoding double-applied.
    #[arg(long, value_name = "DATA")]
    pub data: Option<String>,

    /// Close stdin after any --data is written.
    ///
    /// The same request rather than a second one, deliberately: two round trips leave a window
    /// where the child has the bytes but not the EOF that says the input is complete, and a `cat`
    /// in that window looks identical to a hung one.
    #[arg(long)]
    pub eof: bool,

    #[command(flatten)]
    pub attach: AttachFlags,

    #[command(flatten)]
    pub region: RegionFlags,
}

#[derive(Args, Debug)]
pub struct CpArgs {
    /// Source. `vm:/path` reads from the VM, anything else is a local path.
    #[arg(value_name = "SRC")]
    pub src: String,

    /// Destination. `vm:/path` writes to the VM, anything else is a local path.
    #[arg(value_name = "DST")]
    pub dst: String,

    /// Move a whole directory tree, as an uncompressed tar archive.
    ///
    /// # The two sides are different kinds of thing, deliberately
    ///
    /// The **`vm:`** side is a **directory**. The daemon packs and extracts it: `GET
    /// /v1/fs/tar` refuses anything but a directory and builds the archive itself, and `PUT
    /// /v1/fs/tar` extracts into one through the confined extractor. So `cp vm:/workspace
    /// out.tar --tar` archives a tree, and `cp out.tar vm:/restored --tar` recreates it.
    ///
    /// The **local** side is a `.tar` **file**, and that asymmetry is a real limitation rather
    /// than a choice: `microvms-core/src/session/files.rs:112` declines to add a tar library
    /// because Rust's standard library has no equivalent of Python tarfile's `data` filter,
    /// and "an extraction that looked safe and was not is worse than none". This binary
    /// declines for the same reason plus a stronger one — the daemon's extractor is currently
    /// the *only* extractor in the system, and a second one here would be a second set of
    /// member rules to keep in step. Unpack a downloaded archive with your own `tar xf`.
    ///
    /// Members are stored relative to the packed directory, so they land flattened under the
    /// destination: a `link` inside `/workspace` extracts to `<dest>/link`. That is what makes
    /// a downloaded archive re-uploadable, which is the round trip a harness performs
    /// constantly.
    #[arg(long)]
    pub tar: bool,

    /// Permissions for an uploaded file, octal as a string (`644`, `0755`).
    ///
    /// A string because the wire field is one: `"644"` and `"0644"` mean the same mode, and an
    /// integer would be read as decimal 644 by anything that stringifies it. Only meaningful
    /// uploading a single file — a tar carries its members' own modes.
    #[arg(long, value_name = "OCTAL", conflicts_with = "tar")]
    pub mode: Option<String>,

    #[command(flatten)]
    pub attach: AttachFlags,

    #[command(flatten)]
    pub region: RegionFlags,
}

#[derive(Args, Debug)]
pub struct SuspendArgs {
    /// The MicroVM to freeze.
    #[arg(value_name = "MICROVM_ID")]
    pub microvm_id: String,

    /// How long to wait for the state transition, in seconds.
    #[arg(long, default_value_t = 300.0)]
    pub timeout: f64,

    #[command(flatten)]
    pub region: RegionFlags,
}

#[derive(Args, Debug)]
pub struct ResumeArgs {
    /// The MicroVM to thaw.
    #[arg(value_name = "MICROVM_ID")]
    pub microvm_id: String,

    /// How long to wait for RUNNING, in seconds.
    #[arg(long, default_value_t = 300.0)]
    pub timeout: f64,

    #[command(flatten)]
    pub region: RegionFlags,
}

#[derive(Args, Debug)]
pub struct TerminateArgs {
    /// The MicroVM to terminate.
    #[arg(value_name = "MICROVM_ID")]
    pub microvm_id: String,

    /// The image to delete, if --delete-image is given.
    #[arg(long)]
    pub image_identifier: Option<String>,

    /// The image's name, needed to name its build log group.
    ///
    /// The service created that group, so `terraform destroy` never removes it.
    #[arg(long)]
    pub image_name: Option<String>,

    /// Also delete the image, and name its build log group.
    #[arg(long, requires = "image_identifier")]
    pub delete_image: bool,

    /// Wait for TERMINATED rather than returning as soon as the call is accepted.
    #[arg(long)]
    pub wait: bool,

    #[command(flatten)]
    pub region: RegionFlags,
}

#[derive(Args, Debug)]
pub struct LsArgs {
    /// Where the ledgers live. Defaults to $MICROVM_STATE_DIR or ~/.microvm/runs.
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct LogsArgs {
    /// The image whose log group to name.
    #[arg(value_name = "IMAGE_NAME")]
    pub image_name: String,

    #[command(flatten)]
    pub region: RegionFlags,
}

#[derive(Args, Debug)]
pub struct CostArgs {
    /// Treat the durations as a plan rather than as timings.
    ///
    /// Every duration is labelled projected, so an estimate cannot print as a report of
    /// something that happened.
    #[arg(long)]
    pub estimate: bool,

    /// Also print running versus suspended for the same hold, with the break-even.
    #[arg(long)]
    pub compare: bool,

    /// Baseline MiB, selecting a documented size class.
    #[arg(long, value_enum, default_value = "2048")]
    pub memory: MemoryMib,

    /// Seconds the VM spent, or will spend, RUNNING.
    ///
    /// Billed at baseline whether or not anything is executing — there is no free I/O wait,
    /// which is why suspension rather than idleness is the lever.
    #[arg(long, default_value_t = 0.0)]
    pub running_sec: f64,

    /// Seconds spent suspended. Storage only — no compute line at all.
    #[arg(long, default_value_t = 0.0)]
    pub suspended_sec: f64,

    /// Seconds the image build took.
    ///
    /// Appears as an unpriced line: AWS does not publish whether the server-side build is
    /// billed as compute.
    #[arg(long, default_value_t = 0.0)]
    pub build_sec: f64,

    /// Image size in GB. Adds storage with its one-week minimum retention.
    #[arg(long)]
    pub image_gb: Option<f64>,

    /// Suspend/resume cycles, each paying a snapshot write plus a read.
    #[arg(long, default_value_t = 1)]
    pub cycles: u32,

    /// The hold to compare running against suspended over, in seconds.
    #[arg(long, default_value_t = 3600.0)]
    pub hold_sec: f64,
}

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// The agentd binary to check the architecture of.
    #[arg(long)]
    pub binary: Option<PathBuf>,

    /// The Terraform stack directory. Defaults to ./conformance/infra.
    #[arg(long)]
    pub infra_dir: Option<PathBuf>,

    #[command(flatten)]
    pub region: RegionFlags,

    #[command(flatten)]
    pub infra: InfraFlags,
}

#[derive(Args, Debug)]
pub struct ManifestArgs {
    /// Emit JSON. On by default here, unlike every other command.
    ///
    /// The global --json also works; this exists so a bare `microvm manifest` is already
    /// what its only consumer wants.
    #[arg(long, default_value_t = true)]
    pub emit_json: bool,
}

#[derive(Args, Debug)]
pub struct ConstantsArgs {
    /// Emit the raw constants object, unwrapped by an envelope.
    ///
    /// The one stdout write in this binary that is not an envelope, and the reason is its
    /// consumer: `scripts/check-model-drift` compares key-for-key against a pinned service
    /// model, so an envelope would put every comparison behind `["data"]` for no gain. The
    /// global --json wraps the same object if you want the envelope instead.
    #[arg(long)]
    pub emit_json: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// **CLI-5's static half.** The `--memory` domain is exactly the documented size table.
    ///
    /// Written out in this file and computed here, which is the point: the two can disagree,
    /// and a sixth class added to `SIZE_CLASSES` that never reaches [`MemoryMib`] would be a
    /// class the CLI cannot express. Both directions, so an extra variant here that is not a
    /// class fails too.
    #[test]
    fn the_memory_domain_is_exactly_the_documented_size_table() {
        let from_flag: Vec<u32> = MemoryMib::value_variants()
            .iter()
            .map(|variant| variant.size_class().baseline_mib())
            .collect();
        let from_core: Vec<u32> = SizeClass::ALL
            .iter()
            .map(|class| class.baseline_mib())
            .collect();
        assert_eq!(from_flag, from_core);

        // And the *spellings* clap accepts are the baselines as decimal, so `--memory 2048`
        // is what a caller writes rather than `--memory mib2048`.
        let spellings: Vec<String> = MemoryMib::value_variants()
            .iter()
            .filter_map(|variant| variant.to_possible_value())
            .map(|value| value.get_name().to_string())
            .collect();
        assert_eq!(spellings, ["512", "1024", "2048", "4096", "8192"]);
    }

    /// The `--region` domain is exactly the five measured regions.
    ///
    /// `eu-central-1` is the specific value worth naming: the Python's CLI copy of the list
    /// had drifted to include it, and measurement shows it does *not* carry MicroVMs — it
    /// was one of the three that answered the null-message denial.
    #[test]
    fn the_region_domain_is_exactly_the_five_measured_regions_and_excludes_eu_central_one() {
        let from_flag: Vec<String> = RegionArg::value_variants()
            .iter()
            .map(|variant| variant.region().as_str().to_string())
            .collect();
        let from_core: Vec<String> = microvms_core::region::MICROVM_REGIONS
            .iter()
            .map(|region| region.as_str().to_string())
            .collect();
        assert_eq!(from_flag, from_core);
        assert!(!from_flag.contains(&"eu-central-1".to_string()));

        // The parser really refuses it, rather than the domain merely omitting it.
        // `logs` rather than `ls`, because `ls` reads a local ledger and deliberately carries no
        // region flags at all — a test against it would pass for the wrong reason.
        let refused = Cli::try_parse_from(["microvm", "logs", "img", "--region", "eu-central-1"]);
        assert!(refused.is_err(), "eu-central-1 must not parse");
    }

    /// An off-table `--memory` is refused by the parser, before any handler.
    ///
    /// This is the assertion CLI-5 is: 1500 is the value TRAP-10 was measured with, and the
    /// difference between refusing it here and refusing it in core is a build cycle.
    #[test]
    fn an_off_table_memory_value_never_reaches_a_handler() {
        let refused = Cli::try_parse_from(["microvm", "cost", "--memory", "1500"])
            .expect_err("1500 is not a documented baseline");
        let rendered = refused.render().to_string();
        assert!(rendered.contains("1500"), "{rendered}");
        // clap lists the domain in its own error, so the remedy is in the message.
        assert!(rendered.contains("2048"), "{rendered}");
        // And a documented one does parse, so the guard is a comparison rather than a
        // blanket refusal.
        Cli::try_parse_from(["microvm", "cost", "--memory", "8192"]).expect("8192 is a baseline");
    }

    /// The escape hatch is a separate flag and cannot be combined with the closed set.
    ///
    /// Both together would mean two answers to one question, and the resolution would be
    /// whichever the code happened to read first.
    #[test]
    fn the_unlisted_region_escape_hatch_conflicts_with_the_closed_set() {
        Cli::try_parse_from([
            "microvm",
            "logs",
            "img",
            "--unlisted-region",
            "eu-central-1",
        ])
        .expect("the escape hatch parses on its own");
        let both = Cli::try_parse_from([
            "microvm",
            "logs",
            "img",
            "--region",
            "us-east-1",
            "--unlisted-region",
            "eu-central-1",
        ]);
        assert!(both.is_err(), "two answers to one question must not parse");
    }

    /// The global flags parse before and after the subcommand.
    ///
    /// `global = true` is what makes `microvm --json ls` and `microvm ls --json` the same
    /// invocation, and an agent will write both.
    #[test]
    fn the_global_flags_parse_on_either_side_of_the_subcommand() {
        for argv in [["microvm", "--json", "ls"], ["microvm", "ls", "--json"]] {
            let parsed = Cli::try_parse_from(argv).expect("parses");
            assert!(parsed.json, "{argv:?}");
        }
    }

    /// The tree is internally consistent enough for clap to build it.
    ///
    /// `debug_assert` runs every one of clap's own structural checks — a duplicated long
    /// flag, a `conflicts_with` naming an argument that does not exist, a `requires` cycle.
    /// Those are the failures that otherwise appear as a panic on a user's first run of one
    /// specific subcommand.
    #[test]
    fn the_command_tree_passes_claps_own_structural_checks() {
        Cli::command().debug_assert();
    }

    /// Sixteen subcommands, named as the manifest and the response table name them.
    ///
    /// The five after `exec` are the attached block — `health`, `ack`, `stdin`, `cp` beside it —
    /// and their position is asserted rather than incidental, because `--help`'s reading order is
    /// the only documentation of which commands need the identifier triple.
    #[test]
    fn the_tree_registers_the_lifecycle_commands_the_attached_block_and_the_local_ones() {
        let registered: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|sub| sub.get_name().to_string())
            .collect();
        assert_eq!(
            registered,
            [
                "run",
                "build",
                "exec",
                "health",
                "ack",
                "stdin",
                "cp",
                "suspend",
                "resume",
                "terminate",
                "ls",
                "logs",
                "cost",
                "doctor",
                "manifest",
                "constants",
            ]
        );
    }

    /// **The attached triple is one struct, so all six commands publish the same three flags.**
    ///
    /// Read off the parser rather than off `AttachFlags`, because the claim is about the
    /// commands: a seventh attached command that spelled its own `--endpoint` would parse
    /// identically today and drift the first time one of the three grew a constraint.
    #[test]
    fn every_attached_command_takes_the_same_identifier_triple() {
        let attached = ["exec", "health", "ack", "stdin", "cp"];
        for name in attached {
            let sub = Cli::command()
                .get_subcommands()
                .find(|sub| sub.get_name() == name)
                .unwrap_or_else(|| panic!("{name} is registered"))
                .clone();
            let longs: Vec<&str> = sub
                .get_arguments()
                .filter_map(|arg| arg.get_long())
                .collect();
            for flag in ["endpoint", "agent-token", "microvm-id", "port"] {
                assert!(
                    longs.contains(&flag),
                    "{name} does not take --{flag}, so it cannot address a VM it did not launch"
                );
            }
        }
    }

    /// `--poll` is read-only and therefore cannot be combined with anything that writes.
    ///
    /// Declared as a conflict rather than checked in the handler, because the two together are
    /// two answers to "what should this invocation do" and the resolution would be whichever the
    /// code read first. `--poll` with no COMMAND parses, which is the whole shape of a read.
    #[test]
    fn polling_parses_without_a_command_and_conflicts_with_every_writing_flag() {
        let attach = [
            "--endpoint",
            "https://vm.example",
            "--agent-token",
            "t",
            "--microvm-id",
            "mvm-1",
        ];
        let mut poll = vec!["microvm", "exec", "--poll", "x-1"];
        poll.extend(attach);
        Cli::try_parse_from(&poll).expect("a poll needs no command");

        // And a bare `exec` with no command and no --poll is refused: there would be nothing to
        // run and nothing to read.
        let mut bare = vec!["microvm", "exec"];
        bare.extend(attach);
        assert!(
            Cli::try_parse_from(&bare).is_err(),
            "an exec with neither a command nor --poll has nothing to do"
        );

        for writing in [
            vec!["--stream"],
            vec!["--stdin"],
            vec!["--exec-id", "x-2"],
            vec!["--cwd", "/tmp"],
        ] {
            let mut argv = vec!["microvm", "exec", "--poll", "x-1"];
            argv.extend(attach);
            argv.extend(writing.iter().copied());
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "--poll must not combine with {writing:?}: a read and a write are two answers to \
                 one question"
            );
        }
    }

    /// `--detach` cannot be combined with the two shapes that must not return early.
    ///
    /// `--stream` and `--stdin` both keep working *after* the start: one reads events until the
    /// terminal one, the other writes bytes and sends EOF. A `--detach` beside either would return
    /// before that work happened — leaving a stream nobody read, or worse, a child holding an open
    /// stdin pipe nothing will ever close, which is the exact hang `stdin: false` is the default to
    /// prevent. `--timeout` is *not* conflicted: it is simply unused, and refusing a flag that
    /// merely has no effect would break `microvm exec ... --timeout 60 --detach` in a script that
    /// sets the timeout once for every invocation.
    #[test]
    fn detaching_conflicts_with_the_shapes_that_must_not_return_early() {
        let attach = [
            "--endpoint",
            "https://vm.example",
            "--agent-token",
            "t",
            "--microvm-id",
            "mvm-1",
        ];
        for incompatible in [vec!["--stream"], vec!["--stdin"], vec!["--poll", "x-1"]] {
            let mut argv = vec!["microvm", "exec", "echo hi", "--detach"];
            argv.extend(attach);
            argv.extend(incompatible.iter().copied());
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "--detach must not combine with {incompatible:?}"
            );
        }

        // On its own, and beside the two flags it legitimately pairs with: `--exec-id` (so the
        // caller knows the id in advance) and `--timeout` (unused here, and harmless).
        let mut alone = vec![
            "microvm",
            "exec",
            "echo hi",
            "--detach",
            "--exec-id",
            "x-1",
            "--timeout",
            "60",
        ];
        alone.extend(attach);
        Cli::try_parse_from(&alone).expect("--detach pairs with --exec-id and tolerates --timeout");
    }

    /// `--from-offset` cannot be asked for without the stream it is a cursor into.
    ///
    /// Otherwise it is a number with no meaning, silently accepted — and a caller who passed it
    /// expecting a resume would get the whole output from zero with no complaint.
    #[test]
    fn a_resume_offset_requires_the_stream_it_resumes() {
        let attach = [
            "--endpoint",
            "https://vm.example",
            "--agent-token",
            "t",
            "--microvm-id",
            "mvm-1",
        ];
        let mut without = vec!["microvm", "exec", "echo hi", "--from-offset", "128"];
        without.extend(attach);
        assert!(
            Cli::try_parse_from(&without).is_err(),
            "--from-offset without --stream is a cursor into nothing"
        );

        let mut with = vec![
            "microvm",
            "exec",
            "echo hi",
            "--stream",
            "--from-offset",
            "128",
        ];
        with.extend(attach);
        Cli::try_parse_from(&with).expect("with --stream it parses");
    }

    /// `cp --mode` and `cp --tar` cannot be combined.
    ///
    /// A tar's members carry their own modes, so one mode for the whole archive is a request the
    /// daemon has no field for — refused here rather than silently dropped, because a caller who
    /// passed it believes the permissions were set.
    #[test]
    fn a_tar_copy_cannot_also_name_one_mode() {
        let attach = [
            "--endpoint",
            "https://vm.example",
            "--agent-token",
            "t",
            "--microvm-id",
            "mvm-1",
        ];
        let mut both = vec![
            "microvm", "cp", "./a.tar", "vm:/dst", "--tar", "--mode", "0644",
        ];
        both.extend(attach);
        assert!(
            Cli::try_parse_from(&both).is_err(),
            "a tar's members carry their own modes"
        );

        let mut mode_only = vec!["microvm", "cp", "./a", "vm:/dst", "--mode", "0644"];
        mode_only.extend(attach);
        Cli::try_parse_from(&mode_only).expect("a single file takes a mode");
    }

    /// **The absence half of CLI-5.** No option anywhere carries a client token, a
    /// capability list, a connector name, or an architecture.
    ///
    /// Asserted over every argument of every subcommand rather than by reading this file,
    /// because the failure this catches is a *later* edit adding one — and the four names
    /// are the four traps core closed by having no such parameter at all. A flag here would
    /// be a way to reach a value core refuses, which is exactly what CLI-5 forbids.
    #[test]
    fn no_option_carries_a_token_a_capability_a_connector_or_an_architecture() {
        let forbidden = [
            "client-token",
            "clienttoken",
            "capabilities",
            "capability",
            "connector",
            "architecture",
            "arch",
        ];
        for sub in Cli::command().get_subcommands() {
            for arg in sub.get_arguments() {
                let long = arg.get_long().unwrap_or_default().to_ascii_lowercase();
                let id = arg.get_id().as_str().to_ascii_lowercase();
                for name in forbidden {
                    assert!(
                        long != name && id.replace('_', "-") != name,
                        "{}'s --{long} reaches a value microvms-core has no parameter for",
                        sub.get_name(),
                    );
                }
            }
        }
    }

    /// Every S1-typed option reports a closed domain, and the free-text ones are the ones
    /// whose library counterpart really is a string.
    ///
    /// The manifest's `choices` field is derived from exactly this, so this test is what
    /// makes that field trustworthy rather than decorative.
    #[test]
    fn the_two_s1_options_report_closed_domains_everywhere_they_appear() {
        let mut seen = 0;
        for sub in Cli::command().get_subcommands() {
            for arg in sub.get_arguments() {
                let Some(long) = arg.get_long() else { continue };
                if long == "memory" || long == "region" {
                    let choices = arg.get_possible_values();
                    assert!(
                        !choices.is_empty(),
                        "{}'s --{long} has no closed domain, so it accepts a value core rejects",
                        sub.get_name(),
                    );
                    seen += 1;
                }
            }
        }
        assert!(
            seen >= 3,
            "the S1 options must actually appear on several commands, or this passes vacuously"
        );
    }

    /// `--delete-image` cannot be asked for without the identifier it needs.
    ///
    /// The Python raised `ERR_INVALID_ARG` from the handler for this; clap's `requires`
    /// makes it a parse failure, which is the same code by a shorter path.
    #[test]
    fn deleting_an_image_requires_naming_it() {
        let refused = Cli::try_parse_from(["microvm", "terminate", "mvm-1", "--delete-image"]);
        assert!(refused.is_err(), "--delete-image needs --image-identifier");
        Cli::try_parse_from([
            "microvm",
            "terminate",
            "mvm-1",
            "--delete-image",
            "--image-identifier",
            "arn:image",
        ])
        .expect("with the identifier it parses");
    }

    /// The default baseline is the platform's own 2 GB, not the cheapest class.
    ///
    /// A 0.5 GB default hands someone a sandbox that OOM-kills a real test suite to save
    /// about three cents an hour, and guest swap is absent so there is no paging phase to
    /// absorb it.
    #[test]
    fn the_default_baseline_is_the_platforms_own_rather_than_the_cheapest() {
        let parsed = Cli::try_parse_from(["microvm", "cost"]).expect("parses");
        let Command::Cost(args) = parsed.command else {
            panic!("expected cost");
        };
        assert_eq!(args.memory.size_class(), SizeClass::DEFAULT);
        assert_eq!(args.memory.size_class().baseline_mib(), 2048);
    }
}

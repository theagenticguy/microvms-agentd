//! Identity repair for VMs derived from a shared image.
//!
//! # The problem
//!
//! One MicroVM image is snapshotted once and then restored N times. Everything in
//! that snapshot is byte-identical across every VM, including the files whose whole
//! purpose is to be unique per machine. `systemd-random-seed` credits
//! `/var/lib/systemd/random-seed` into the kernel pool at boot, so N VMs credit the
//! *same* seed; `/etc/machine-id` is what `sd_id128_get_machine_app_specific` and
//! anything derived from it keys on. A key generated in VM 7 can then repeat a key
//! generated in VM 3, which is a security bug rather than an untidiness.
//!
//! # What is already handled, and must not be redone here
//!
//! Entropy is partly free. Each `RunMicrovm` is a Firecracker *restore*, which
//! bumps VMGenID, and Linux >= 5.18 reseeds the kernel CSPRNG from that
//! notification. So `getrandom(2)` after a restore is already distinct per VM
//! without the daemon doing anything, and `/dev/urandom` is a sound source for the
//! identifiers written below. Re-seeding the pool from userspace would add nothing
//! and would need `RNDADDENTROPY`.
//!
//! What VMGenID does *not* touch is any identifier already committed to a file. The
//! kernel does not rewrite `/etc/machine-id`, and it does not re-run
//! `systemd-random-seed`. Those are the caller's, which means they are ours.
//!
//! # What this module cannot do, honestly
//!
//! The daemon runs as root in the VM, which is necessary but not sufficient:
//!
//! * **`boot_id` needs a bind mount, and the mount can fail.**
//!   `/proc/sys/kernel/random/boot_id` is 0444 and `procfs` refuses the write even
//!   for root — it is generated per boot and has no store to write into. The only
//!   way to change what a reader sees is to mount a file over that path. That needs
//!   `CAP_SYS_ADMIN` in the current mount namespace, and it is refused outright in
//!   a container that did not ask for it. When it fails we log and continue.
//! * **A bind mount is namespace-local.** It is visible to this mount namespace and
//!   anything sharing it. A child in a fresh namespace, or an already-running
//!   process holding an open fd on the original, still sees the snapshot value.
//! * **Already-read values cannot be recalled.** Anything that read `machine-id` or
//!   `boot_id` before the daemon got here — an init system, a preloaded agent, a
//!   D-Bus daemon — has its copy. Repair is only sound because the daemon is the
//!   container `CMD` and therefore runs before any workload.
//! * **In-process state elsewhere is unreachable.** A daemon baked into the image
//!   that cached a derived identifier in memory keeps it. Only a restart fixes that,
//!   and restarting things is not this module's job.
//! * **Cached credentials cannot be enumerated in general.** Only a configured list
//!   of paths is removed; a credential in a place nobody named survives. See
//!   [`Layout::credential_paths`].
//!
//! # Why a failure is never fatal
//!
//! Every repair failure is logged loudly and then ignored. The daemon is the only
//! channel into the VM — no SSH, no supervisor, no console — so refusing to serve
//! because a bind mount was denied would strand a VM with work in it and no way to
//! reach it. A duplicate `machine-id` is a real security problem; an unreachable VM
//! is a worse one, and it is unrecoverable rather than merely bad.

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Where the identity files live, and how new values are minted.
///
/// Injected rather than hard-coded for a blunt reason: a test that repaired the
/// real `/etc/machine-id` would corrupt the machine it ran on. Every path here is
/// redirected into a tempdir by the tests below, so the repair logic is exercised
/// end-to-end without touching the host.
#[derive(Clone, Debug)]
pub struct Layout {
    /// `/etc/machine-id` in production. Rewritten with a fresh 32-hex-digit ID.
    pub machine_id: PathBuf,
    /// `/proc/sys/kernel/random/boot_id`. Read-only; needs a bind mount.
    pub boot_id: PathBuf,
    /// Where the replacement `boot_id` file is staged before being mounted over
    /// the `procfs` entry. It must be a real file on a writable filesystem,
    /// because a bind mount needs a source that exists.
    pub boot_id_source: PathBuf,
    /// `/var/lib/systemd/random-seed`. **Deleted, never rewritten** — see
    /// [`repair`].
    pub random_seed: PathBuf,
    /// Credentials and leases that are per-VM but were captured in the snapshot.
    /// Removed if present, ignored if not.
    pub credential_paths: Vec<PathBuf>,
    /// Hostname to set, derived from the fresh machine id when `None`.
    pub hostname: Option<String>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            machine_id: PathBuf::from("/etc/machine-id"),
            boot_id: PathBuf::from("/proc/sys/kernel/random/boot_id"),
            // `/run` is a tmpfs on any systemd image and is writable early, which
            // is what a bind-mount source has to be. `/tmp` would work too but is
            // a place callers put their own files.
            boot_id_source: PathBuf::from("/run/agentd-boot-id"),
            random_seed: PathBuf::from("/var/lib/systemd/random-seed"),
            // Deliberately short and deliberately not a guess at every credential
            // store that could exist. `machine-id`-derived D-Bus identity is the
            // one that is both universally present on a systemd image and
            // genuinely required to be unique.
            credential_paths: vec![PathBuf::from("/var/lib/dbus/machine-id")],
            hostname: None,
        }
    }
}

/// What one repair step did. Every variant is reported; none is fatal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StepResult {
    /// The step changed something.
    Repaired,
    /// Nothing to do — the file was already absent, or the platform had no such
    /// path. Distinguished from `Repaired` so a log reader can tell "I deleted the
    /// shared seed" from "there was no seed to delete".
    NotApplicable,
    /// The step failed. The VM keeps serving with the shared value in place.
    Failed { error: String },
}

/// One named step and its outcome.
#[derive(Clone, Debug, Serialize)]
pub struct Step {
    pub name: &'static str,
    #[serde(flatten)]
    pub result: StepResult,
}

/// The full report, surfaced on `/v1/health` so an orchestrator can see that a
/// VM is serving with an unrepaired identity instead of finding out from a
/// duplicated key months later.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    /// False when the guard was switched off by config; `steps` is then empty.
    pub attempted: bool,
    pub steps: Vec<Step>,
}

impl Report {
    /// The report for a deployment that opted out. Some callers *want* stable
    /// identity — a long-lived VM re-created from a snapshot on purpose, or a
    /// fleet keyed by machine id — so opting out is a supported configuration and
    /// not a mistake to warn about.
    pub fn skipped() -> Self {
        Self {
            attempted: false,
            steps: Vec::new(),
        }
    }

    /// Whether any step failed. `/v1/health` reports this so the condition is
    /// visible without parsing every step.
    pub fn degraded(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step.result, StepResult::Failed { .. }))
    }
}

/// How the replacement identifiers are produced and how the mount is attempted.
///
/// A trait rather than a set of function pointers because the mount and the
/// hostname are side effects a test must observe without performing, and because
/// a deterministic id generator is what lets a test assert on exact file contents.
pub trait Platform {
    /// A fresh 128-bit id as 32 lowercase hex digits, the `/etc/machine-id`
    /// format.
    fn fresh_id(&self) -> io::Result<String>;
    /// Sets the kernel hostname.
    fn set_hostname(&self, name: &str) -> io::Result<()>;
    /// Bind-mounts `source` over `target`.
    fn bind_mount(&self, source: &Path, target: &Path) -> io::Result<()>;
}

/// The real one.
pub struct Host;

impl Platform for Host {
    /// Reads `/dev/urandom` rather than hashing anything host-derived.
    ///
    /// Sound *because* of the VMGenID reseed described in the module docs: after a
    /// Firecracker restore the kernel CSPRNG has been reseeded, so this returns a
    /// distinct value per VM. Before that guarantee existed this exact code would
    /// have returned the same 16 bytes in every VM — which is worth stating,
    /// because the code looks correct either way.
    fn fresh_id(&self) -> io::Result<String> {
        // `read_exact` on a bounded buffer, never `std::fs::read`: `/dev/urandom`
        // is an infinite stream and reading it to EOF does not return.
        Ok(hex(&read_exact_bytes(Path::new("/dev/urandom"), 16)?))
    }

    fn set_hostname(&self, name: &str) -> io::Result<()> {
        nix::unistd::sethostname(name).map_err(io::Error::from)
    }

    /// `MS_BIND` with no filesystem type and no data, which is the only form that
    /// can shadow a single `procfs` file.
    fn bind_mount(&self, source: &Path, target: &Path) -> io::Result<()> {
        nix::mount::mount(
            Some(source),
            target,
            None::<&Path>,
            nix::mount::MsFlags::MS_BIND,
            None::<&Path>,
        )
        .map_err(io::Error::from)
    }
}

/// Reads exactly `n` bytes. `std::fs::read` on `/dev/urandom` would never return.
fn read_exact_bytes(path: &Path, n: usize) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; n];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Runs every repair step, logging each outcome.
///
/// Order matters in one place: the machine id is minted first because the hostname
/// is derived from it, so a VM's hostname and machine id agree in logs.
pub fn repair(layout: &Layout, platform: &dyn Platform) -> Report {
    let mut steps = Vec::new();

    // Minted once and reused. Two calls would produce a machine id and a hostname
    // that disagree, which is confusing in exactly the situation this is meant to
    // make legible.
    let fresh = match platform.fresh_id() {
        Ok(id) => Some(id),
        Err(err) => {
            // Without a fresh id there is nothing to write, so the id-derived
            // steps are all reported failed rather than silently skipped.
            let error = err.to_string();
            for name in ["machine-id", "hostname", "boot-id"] {
                steps.push(Step {
                    name,
                    result: StepResult::Failed {
                        error: error.clone(),
                    },
                });
            }
            None
        }
    };

    if let Some(id) = &fresh {
        steps.push(Step {
            name: "machine-id",
            result: write_machine_id(&layout.machine_id, id),
        });

        let hostname = layout
            .hostname
            .clone()
            // Truncated to 12 hex digits: a hostname has a 64-byte ceiling and a
            // full 32-hex-digit id is unreadable in a log line. The uniqueness that
            // matters is the machine id's; the hostname only has to not collide.
            .unwrap_or_else(|| format!("microvm-{}", &id[..12]));
        steps.push(Step {
            name: "hostname",
            result: match platform.set_hostname(&hostname) {
                Ok(()) => StepResult::Repaired,
                Err(err) => StepResult::Failed {
                    error: err.to_string(),
                },
            },
        });

        steps.push(Step {
            name: "boot-id",
            result: repair_boot_id(layout, platform, id),
        });
    }

    // Deleted rather than rewritten, and the distinction is the whole point.
    // `systemd-random-seed` credits this file into the kernel pool and then
    // refreshes it from the pool at shutdown. Writing our own bytes would credit a
    // value derived from *this* VM's entropy, which is fine, but it would also
    // leave a file that a later snapshot captures again — recreating the shared-seed
    // problem one generation down. Absent is unambiguous: systemd's load step
    // treats a missing seed as "nothing to credit" and writes a fresh one at
    // shutdown from the (already VMGenID-reseeded) pool.
    steps.push(Step {
        name: "random-seed",
        result: remove_if_present(&layout.random_seed),
    });

    for path in &layout.credential_paths {
        steps.push(Step {
            name: "cached-credential",
            result: remove_if_present(path),
        });
    }

    let report = Report {
        attempted: true,
        steps,
    };

    for step in &report.steps {
        match &step.result {
            StepResult::Repaired => tracing::info!(step = step.name, "identity repaired"),
            StepResult::NotApplicable => {
                tracing::debug!(step = step.name, "identity step not applicable");
            }
            // Warn rather than error, and explicitly say the daemon is still
            // serving: a bare error line here reads like a startup failure, and an
            // operator who kills the VM in response has destroyed the work in it
            // over a duplicate identifier.
            StepResult::Failed { error } => tracing::warn!(
                step = step.name,
                %error,
                "identity repair step FAILED; the daemon is still serving with the \
                 image's shared value in place",
            ),
        }
    }

    report
}

/// Writes a fresh machine id.
///
/// The file is 0444 on a booted system, so it is removed before being recreated:
/// opening it for write would be EACCES even as root on some filesystems, and the
/// mode is restored afterwards.
fn write_machine_id(path: &Path, id: &str) -> StepResult {
    let attempt = || -> io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        // Absence is not an error here; `remove_file` on a missing path is ENOENT
        // and the next step creates it anyway.
        let _ = std::fs::remove_file(path);
        // Trailing newline: `/etc/machine-id` is specified as 32 hex digits
        // followed by a newline, and systemd's parser accepts it either way but
        // `cat` output without it runs into the next log line.
        std::fs::write(path, format!("{id}\n"))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))
    };
    match attempt() {
        Ok(()) => StepResult::Repaired,
        Err(err) => StepResult::Failed {
            error: err.to_string(),
        },
    }
}

/// Shadows the read-only `procfs` `boot_id` with a bind mount.
///
/// `boot_id` is formatted with dashes (RFC 4122 grouping) while `machine-id` is
/// not, and readers do parse the dashes, so the same 128 bits are re-formatted
/// rather than reused verbatim.
fn repair_boot_id(layout: &Layout, platform: &dyn Platform, id: &str) -> StepResult {
    // A target that does not exist is a platform without this `procfs` entry, not
    // a failure. Mounting onto a missing path would be ENOENT reported as a defect.
    if !layout.boot_id.exists() {
        return StepResult::NotApplicable;
    }

    let attempt = || -> io::Result<()> {
        if let Some(parent) = layout
            .boot_id_source
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&layout.boot_id_source, format!("{}\n", dashed(id)))?;
        platform.bind_mount(&layout.boot_id_source, &layout.boot_id)
    };

    match attempt() {
        Ok(()) => StepResult::Repaired,
        Err(err) => StepResult::Failed {
            error: err.to_string(),
        },
    }
}

/// Formats 32 hex digits as `8-4-4-4-12`, the `boot_id` shape.
fn dashed(id: &str) -> String {
    if id.len() != 32 {
        return id.to_string();
    }
    format!(
        "{}-{}-{}-{}-{}",
        &id[0..8],
        &id[8..12],
        &id[12..16],
        &id[16..20],
        &id[20..32],
    )
}

/// Removes a path if it is there. Absence is [`StepResult::NotApplicable`] rather
/// than a failure, because a minimal image legitimately has neither systemd's seed
/// nor D-Bus's cached id.
fn remove_if_present(path: &Path) -> StepResult {
    match std::fs::remove_file(path) {
        Ok(()) => StepResult::Repaired,
        Err(err) if err.kind() == io::ErrorKind::NotFound => StepResult::NotApplicable,
        Err(err) => StepResult::Failed {
            error: err.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tempfile::TempDir;

    /// Records the side effects instead of performing them, so no test sets the
    /// host's hostname or mounts anything over the host's `procfs`.
    struct FakePlatform {
        id: RefCell<String>,
        id_fails: bool,
        hostname_fails: bool,
        mount_fails: bool,
        hostnames: RefCell<Vec<String>>,
        mounts: RefCell<Vec<(PathBuf, PathBuf)>>,
    }

    impl FakePlatform {
        fn new() -> Self {
            Self {
                // A fixed id, so a test can assert exact file contents.
                id: RefCell::new("0123456789abcdef0123456789abcdef".into()),
                id_fails: false,
                hostname_fails: false,
                mount_fails: false,
                hostnames: RefCell::new(Vec::new()),
                mounts: RefCell::new(Vec::new()),
            }
        }
    }

    impl Platform for FakePlatform {
        fn fresh_id(&self) -> io::Result<String> {
            if self.id_fails {
                return Err(io::Error::other("no entropy source"));
            }
            Ok(self.id.borrow().clone())
        }

        fn set_hostname(&self, name: &str) -> io::Result<()> {
            if self.hostname_fails {
                return Err(io::Error::from_raw_os_error(libc_eperm()));
            }
            self.hostnames.borrow_mut().push(name.to_string());
            Ok(())
        }

        fn bind_mount(&self, source: &Path, target: &Path) -> io::Result<()> {
            if self.mount_fails {
                return Err(io::Error::from_raw_os_error(libc_eperm()));
            }
            self.mounts
                .borrow_mut()
                .push((source.to_path_buf(), target.to_path_buf()));
            Ok(())
        }
    }

    /// EPERM, which is what a bind mount without `CAP_SYS_ADMIN` really returns.
    fn libc_eperm() -> i32 {
        1
    }

    /// A layout entirely inside a tempdir. Nothing here names a real system path.
    fn layout(dir: &TempDir) -> Layout {
        let root = dir.path();
        std::fs::create_dir_all(root.join("etc")).expect("mkdir etc");
        std::fs::create_dir_all(root.join("var/lib/systemd")).expect("mkdir var");
        std::fs::create_dir_all(root.join("proc")).expect("mkdir proc");
        std::fs::create_dir_all(root.join("run")).expect("mkdir run");

        // Pre-seeded with the shared values a snapshot would carry.
        std::fs::write(
            root.join("etc/machine-id"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        )
        .expect("shared machine-id");
        std::fs::write(
            root.join("var/lib/systemd/random-seed"),
            b"shared-seed-bytes",
        )
        .expect("shared seed");
        std::fs::write(
            root.join("proc/boot_id"),
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb\n",
        )
        .expect("shared boot_id");
        std::fs::write(root.join("var/lib/dbus-machine-id"), "cccc\n").expect("shared dbus id");

        Layout {
            machine_id: root.join("etc/machine-id"),
            boot_id: root.join("proc/boot_id"),
            boot_id_source: root.join("run/agentd-boot-id"),
            random_seed: root.join("var/lib/systemd/random-seed"),
            credential_paths: vec![root.join("var/lib/dbus-machine-id")],
            hostname: None,
        }
    }

    fn result_of<'a>(report: &'a Report, name: &str) -> &'a StepResult {
        &report
            .steps
            .iter()
            .find(|step| step.name == name)
            .unwrap_or_else(|| panic!("no step named {name}"))
            .result
    }

    #[test]
    fn a_full_repair_replaces_the_shared_identity() {
        let dir = TempDir::new().expect("tempdir");
        let layout = layout(&dir);
        let platform = FakePlatform::new();

        let report = repair(&layout, &platform);
        assert!(report.attempted);
        assert!(!report.degraded(), "every step succeeded: {report:?}");

        // The machine id is the fresh one, not the snapshot's.
        let written = std::fs::read_to_string(&layout.machine_id).expect("machine-id readable");
        assert_eq!(written.trim(), "0123456789abcdef0123456789abcdef");
        assert_ne!(written.trim(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

        // The hostname is derived from it, so the two agree in logs.
        assert_eq!(
            platform.hostnames.borrow().as_slice(),
            ["microvm-0123456789ab"],
        );

        // boot_id was shadowed by a mount rather than written through, because the
        // procfs entry is read-only even for root.
        assert_eq!(
            platform.mounts.borrow().as_slice(),
            [(layout.boot_id_source.clone(), layout.boot_id.clone())],
        );
        // And the staged source carries the dashed form readers expect.
        let staged =
            std::fs::read_to_string(&layout.boot_id_source).expect("bind source is a real file");
        assert_eq!(staged.trim(), "01234567-89ab-cdef-0123-456789abcdef");

        // The seed is GONE rather than rewritten: a rewritten file gets captured by
        // the next snapshot and recreates the shared-seed problem one generation on.
        assert!(
            !layout.random_seed.exists(),
            "the shared seed must be deleted, not replaced",
        );
        assert!(!layout.credential_paths[0].exists());
    }

    #[test]
    fn a_failed_bind_mount_is_reported_but_does_not_stop_the_other_steps() {
        // The documented case: no CAP_SYS_ADMIN, so boot_id cannot be shadowed. An
        // unreachable VM is worse than a duplicate boot_id, so everything else must
        // still be repaired and the daemon must still serve.
        let dir = TempDir::new().expect("tempdir");
        let layout = layout(&dir);
        let mut platform = FakePlatform::new();
        platform.mount_fails = true;

        let report = repair(&layout, &platform);
        assert!(report.degraded(), "the failure is visible on /v1/health");
        assert!(matches!(
            result_of(&report, "boot-id"),
            StepResult::Failed { .. },
        ));

        // The steps that could succeed did.
        assert_eq!(result_of(&report, "machine-id"), &StepResult::Repaired);
        assert_eq!(result_of(&report, "random-seed"), &StepResult::Repaired);
        assert!(!layout.random_seed.exists());
        assert_eq!(
            std::fs::read_to_string(&layout.machine_id)
                .expect("machine-id")
                .trim(),
            "0123456789abcdef0123456789abcdef",
        );
    }

    #[test]
    fn a_hostname_failure_is_isolated_from_the_file_repairs() {
        let dir = TempDir::new().expect("tempdir");
        let layout = layout(&dir);
        let mut platform = FakePlatform::new();
        platform.hostname_fails = true;

        let report = repair(&layout, &platform);
        assert!(matches!(
            result_of(&report, "hostname"),
            StepResult::Failed { .. },
        ));
        assert_eq!(result_of(&report, "machine-id"), &StepResult::Repaired);
        assert_eq!(result_of(&report, "boot-id"), &StepResult::Repaired);
    }

    #[test]
    fn no_entropy_source_fails_every_id_derived_step_and_still_clears_the_seed() {
        // Without a fresh id there is nothing to write, but deleting the shared
        // seed is still both possible and worth doing — it is the step that stops
        // N VMs crediting identical bytes into the kernel pool.
        let dir = TempDir::new().expect("tempdir");
        let layout = layout(&dir);
        let mut platform = FakePlatform::new();
        platform.id_fails = true;

        let report = repair(&layout, &platform);
        assert!(report.degraded());
        for name in ["machine-id", "hostname", "boot-id"] {
            assert!(
                matches!(result_of(&report, name), StepResult::Failed { .. }),
                "{name} cannot succeed without a fresh id",
            );
        }
        assert_eq!(result_of(&report, "random-seed"), &StepResult::Repaired);
        assert!(!layout.random_seed.exists());
        // The snapshot's machine id is untouched rather than truncated to nothing,
        // which would be worse than leaving it shared.
        assert_eq!(
            std::fs::read_to_string(&layout.machine_id)
                .expect("machine-id still readable")
                .trim(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
    }

    #[test]
    fn absent_files_are_not_applicable_rather_than_failures() {
        // A minimal image has no systemd seed and no D-Bus id. That is not a
        // degraded repair, and reporting it as one would train operators to ignore
        // the field.
        let dir = TempDir::new().expect("tempdir");
        let mut layout = layout(&dir);
        std::fs::remove_file(&layout.random_seed).expect("rm seed");
        std::fs::remove_file(&layout.credential_paths[0]).expect("rm dbus id");
        std::fs::remove_file(&layout.boot_id).expect("rm boot_id");
        layout.hostname = Some("explicit-name".into());

        let platform = FakePlatform::new();
        let report = repair(&layout, &platform);

        assert!(!report.degraded(), "{report:?}");
        assert_eq!(
            result_of(&report, "random-seed"),
            &StepResult::NotApplicable
        );
        assert_eq!(
            result_of(&report, "cached-credential"),
            &StepResult::NotApplicable,
        );
        // No procfs entry means nothing to shadow, and mounting onto a missing
        // path would be an ENOENT reported as a defect.
        assert_eq!(result_of(&report, "boot-id"), &StepResult::NotApplicable);
        assert!(platform.mounts.borrow().is_empty());
        // An explicit hostname overrides the derived one.
        assert_eq!(platform.hostnames.borrow().as_slice(), ["explicit-name"]);
    }

    #[test]
    fn a_skipped_report_is_distinguishable_from_a_clean_one() {
        // `attempted: false` is the opt-out, and an orchestrator must be able to
        // tell it from a repair that ran and found nothing to do.
        let skipped = Report::skipped();
        assert!(!skipped.attempted);
        assert!(!skipped.degraded());
        assert!(skipped.steps.is_empty());
    }

    #[test]
    fn the_dashed_form_groups_the_same_bits_and_passes_through_odd_lengths() {
        assert_eq!(
            dashed("0123456789abcdef0123456789abcdef"),
            "01234567-89ab-cdef-0123-456789abcdef",
        );
        // Never panics on a length it did not expect: a slicing panic here would
        // take down startup, which is the one thing this module must not do.
        assert_eq!(dashed("short"), "short");
        assert_eq!(dashed(""), "");
    }

    #[test]
    fn the_real_entropy_source_yields_distinct_well_formed_ids() {
        // Reads /dev/urandom, which is safe in a test: it is a read, and the whole
        // point is that it does not depend on any host file we could damage.
        let host = Host;
        let a = host.fresh_id().expect("urandom readable");
        let b = host.fresh_id().expect("urandom readable");
        assert_eq!(a.len(), 32, "machine-id is 32 hex digits: {a}");
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        assert_ne!(a, b, "two draws must differ");
    }

    #[test]
    fn a_read_only_machine_id_is_still_replaced() {
        // /etc/machine-id is 0444 on a booted system. Opening it for write is
        // EACCES on some filesystems even as root, so the repair unlinks first —
        // and the resulting file must carry the 0444 mode back.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().expect("tempdir");
        let layout = layout(&dir);
        std::fs::set_permissions(&layout.machine_id, std::fs::Permissions::from_mode(0o444))
            .expect("chmod 444");

        let report = repair(&layout, &FakePlatform::new());
        assert_eq!(result_of(&report, "machine-id"), &StepResult::Repaired);
        assert_eq!(
            std::fs::read_to_string(&layout.machine_id)
                .expect("readable")
                .trim(),
            "0123456789abcdef0123456789abcdef",
        );
        let mode = std::fs::metadata(&layout.machine_id)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o444, "the restrictive mode is restored");
    }
}

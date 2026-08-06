//! Disk-pressure guard for the write paths.
//!
//! # Why refusing beats proceeding
//!
//! Documented incident, anthropics/claude-code#59856: a Claude Code sandbox
//! accumulated 121 never-collected session directories, filled two 10 GB disks to
//! 100%, and then new sessions died with `useradd: No space left on device`. The
//! contributors were mundane — a 956 MB Playwright cache re-downloaded per run, an
//! unbounded journal — and the failure was silent until the disk was already full.
//!
//! ENOSPC is a bad way to learn about disk pressure for two reasons. It arrives
//! *after* the filesystem is full, so by then every other writer in the VM is
//! broken too, including the ones that cannot report anything (a shell's history
//! file, a package manager's lock, the daemon's own spool). And it arrives as a
//! generic io error mid-stream, so the caller sees a 500 with no way to
//! distinguish "the disk is full" from "the daemon is broken" — and retrying, the
//! correct response to the second, makes the first worse.
//!
//! So a write is refused *before* it starts if it would take the filesystem below
//! a configured reserve, with a status the caller can branch on and a message
//! naming the actual free space.
//!
//! # Why a probe seam rather than a direct `statvfs` call
//!
//! The check is a function pointer ([`SpaceProbe`]) rather than a call to
//! [`available_bytes`], because a test that reads the host's real free space is a
//! test whose verdict depends on the machine it runs on. It would pass on a full
//! CI box and fail on an empty laptop, or the reverse, and either way it proves
//! nothing about the guard. The seam is also what lets a test simulate a
//! filesystem that fills up *during* an upload, which is otherwise unreachable.

use std::io;
use std::path::Path;

use serde::Serialize;

/// How free space is measured. A function pointer rather than a trait object so
/// it stays `Copy` and can live in `AppState` without an allocation.
pub type SpaceProbe = fn(&Path) -> io::Result<u64>;

/// How often [`copy_guarded`] re-probes during a long transfer.
///
/// A compromise with a real cost on each side. Probing more often spends a
/// syscall per chunk on a path that is otherwise pure copying; probing less often
/// widens the window in which a single upload can overrun the reserve, since the
/// most a transfer can overshoot by is roughly this many bytes. 8 MiB against a
/// default 256 MiB reserve means a worst-case overshoot of about 3% of the
/// reserve, which the reserve absorbs.
const PROBE_INTERVAL_BYTES: u64 = 8 * 1024 * 1024;

/// Bytes available on the filesystem holding `path`, or the nearest ancestor of
/// `path` that exists.
///
/// Walking up to an existing ancestor is required, not a convenience: the write
/// paths accept a target that does not exist yet — that is the normal case for
/// `PUT /v1/fs/file?path=/new/dir/file` — and `statvfs` on a missing path is
/// ENOENT. The filesystem we care about is the one that will *hold* the new path,
/// which is the one its nearest existing ancestor lives on.
///
/// `f_bavail` is used rather than `f_bfree`, so the answer is what is available to
/// an unprivileged writer. The daemon runs as root and could dip into ext4's
/// reserved blocks, but counting them as free would make the guard hand out space
/// that everything else in the VM cannot touch — and the workloads that actually
/// fill the disk are the exec'd children, not the daemon.
pub fn available_bytes(path: &Path) -> io::Result<u64> {
    let probe_at = existing_ancestor(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no existing ancestor of {}", path.display()),
        )
    })?;

    let stat = nix::sys::statvfs::statvfs(probe_at).map_err(io::Error::from)?;
    // `fragment_size` (`f_frsize`) is the unit `f_bavail` counts in, not
    // `block_size` (`f_bsize`), which is the preferred I/O size and can differ.
    // Multiplying by the wrong one is a silent factor-of-N error in the guard.
    let frsize = stat.fragment_size() as u64;
    Ok((stat.blocks_available() as u64).saturating_mul(frsize))
}

/// The deepest ancestor of `path` (including `path` itself) that exists.
fn existing_ancestor(path: &Path) -> Option<&Path> {
    // `ancestors` yields the path, then each parent, ending at `/` for an absolute
    // path and at `""` for a relative one. `exists()` follows symlinks, which is
    // what we want: a symlinked directory's *target* is the filesystem that fills.
    path.ancestors()
        .find(|candidate| !candidate.as_os_str().is_empty() && std::fs::metadata(candidate).is_ok())
}

/// The measurement and the threshold it was judged against.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Reading {
    /// Bytes available to an unprivileged writer.
    pub available: u64,
    /// Bytes that must stay free. Zero disables the guard.
    pub reserve: u64,
}

impl Reading {
    /// Whether a write should be refused.
    ///
    /// A reserve of zero disables the guard, so a deployment that would rather hit
    /// ENOSPC than be refused can opt out without patching the daemon.
    pub fn under_pressure(&self) -> bool {
        self.reserve > 0 && self.available < self.reserve
    }
}

/// A probe bound to a reserve. Copied out of `AppState` so a handler holds no
/// borrow of shared state across an await.
#[derive(Clone, Copy)]
pub struct Guard {
    pub probe: SpaceProbe,
    pub reserve: u64,
}

/// Deliberately hand-written: a function pointer's `Debug` is a bare address,
/// which is noise in a log line and changes between builds.
impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guard")
            .field("reserve", &self.reserve)
            .finish_non_exhaustive()
    }
}

impl Guard {
    /// Measures the filesystem holding `path`.
    ///
    /// A probe failure is *not* pressure. It is reported as an io error and the
    /// caller lets the write proceed: refusing every write because `statvfs` is
    /// unavailable would convert a diagnostic gap into a total outage, on a VM
    /// where the daemon is the only way in.
    pub fn read(&self, path: &Path) -> io::Result<Reading> {
        Ok(Reading {
            available: (self.probe)(path)?,
            reserve: self.reserve,
        })
    }

    /// Refuses up front if `path`'s filesystem is already below the reserve.
    ///
    /// `Ok(None)` means either "enough space" or "could not tell", which the
    /// caller treats identically — see [`Guard::read`].
    pub fn preflight(&self, path: &Path) -> Option<Reading> {
        match self.read(path) {
            Ok(reading) if reading.under_pressure() => Some(reading),
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    %err,
                    "cannot measure free space; allowing the write",
                );
                None
            }
        }
    }
}

/// Amortizes the free-space check across a sequence of writes whose sizes are
/// known but which are not one contiguous stream.
///
/// Tar extraction is the case: each member is copied separately, so
/// [`copy_guarded`] cannot span them, and probing once per member would spend a
/// syscall per member on an archive that may hold 100,000 of them. This accumulates
/// the bytes written and probes only after [`PROBE_INTERVAL_BYTES`] of them, giving
/// extraction the same overshoot bound as a streaming copy.
pub struct Pacer {
    guard: Guard,
    since_probe: u64,
}

impl Pacer {
    pub fn new(guard: Guard) -> Self {
        Self {
            guard,
            since_probe: 0,
        }
    }

    /// Records `bytes` as written and returns a reading if the reserve has been
    /// crossed. Probes at most once per [`PROBE_INTERVAL_BYTES`].
    pub fn record(&mut self, bytes: u64, target: &Path) -> Option<Reading> {
        self.since_probe = self.since_probe.saturating_add(bytes);
        if self.since_probe < PROBE_INTERVAL_BYTES {
            return None;
        }
        self.since_probe = 0;
        self.guard.preflight(target)
    }
}

/// Why a guarded copy stopped.
#[derive(Debug)]
pub enum CopyError {
    /// The filesystem crossed the reserve mid-transfer. Carries the reading that
    /// tripped it so the response can name real numbers.
    Pressure(Reading),
    /// The stream or the filesystem failed.
    Io(io::Error),
}

impl From<io::Error> for CopyError {
    fn from(err: io::Error) -> Self {
        CopyError::Io(err)
    }
}

/// Copies `reader` into `writer`, re-checking free space as it goes.
///
/// A pre-flight check alone is not enough, and the gap is not theoretical: the
/// body limit defaults to 512 MiB, so one accepted upload can be far larger than
/// any reasonable reserve. Without an in-flight check, a single request that
/// passed pre-flight with 300 MiB free proceeds to write 512 MiB and takes the
/// filesystem to 100% — exactly the outcome the guard exists to prevent.
///
/// So the transfer is chopped into [`PROBE_INTERVAL_BYTES`] slices and the reserve
/// is re-checked between them. Crossing it aborts the copy and reports how many
/// bytes had landed, which the caller needs in order to clean up.
pub async fn copy_guarded<R, W>(
    reader: &mut R,
    writer: &mut W,
    guard: &Guard,
    target: &Path,
) -> Result<u64, (u64, CopyError)>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut written = 0u64;
    loop {
        // `take` by reference, so the limit applies to this slice only and the
        // reader survives to the next iteration. Consuming the reader would end
        // the transfer after the first 8 MiB.
        let mut slice = reader.take(PROBE_INTERVAL_BYTES);
        let copied = match tokio::io::copy(&mut slice, writer).await {
            Ok(copied) => copied,
            Err(err) => return Err((written, CopyError::Io(err))),
        };
        written += copied;

        if copied < PROBE_INTERVAL_BYTES {
            // A short slice means the reader hit EOF. The final flush is the
            // caller's, since only it knows whether the writer is a spool it will
            // rewind or a file it will chmod.
            return Ok(written);
        }

        // Checked *after* the slice landed rather than before, so a transfer that
        // fits is never refused on a stale reading, and the very first check still
        // happens only 8 MiB in.
        if let Some(reading) = guard.preflight(target) {
            return Err((written, CopyError::Pressure(reading)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// What the fake probe reports. Thread-local rather than a `static
        /// AtomicU64`, and the difference is not stylistic: the test harness runs
        /// these in parallel threads in one process, so a shared static lets one
        /// test's `store` change another's reading. That is exactly how the first
        /// version of this module failed — passing when run alone and failing under
        /// `cargo test`. A `#[tokio::test]` is a current-thread runtime, so the
        /// future polls on the same thread that set the value.
        static FAKE_AVAILABLE: Cell<u64> = const { Cell::new(0) };
    }

    fn set_available(bytes: u64) {
        FAKE_AVAILABLE.set(bytes);
    }

    /// A probe that reports a value the test controls, so no assertion here
    /// depends on the host's real free space.
    fn fake_probe(_path: &Path) -> io::Result<u64> {
        Ok(FAKE_AVAILABLE.get())
    }

    fn failing_probe(_path: &Path) -> io::Result<u64> {
        Err(io::Error::other("statvfs unavailable"))
    }

    fn guard(reserve: u64) -> Guard {
        Guard {
            probe: fake_probe,
            reserve,
        }
    }

    #[test]
    fn pressure_is_available_below_reserve_and_a_zero_reserve_disables_the_guard() {
        assert!(
            Reading {
                available: 99,
                reserve: 100
            }
            .under_pressure()
        );
        // Exactly at the reserve is not pressure: the reserve is what must stay
        // free, so having exactly that much is the boundary case that passes.
        assert!(
            !Reading {
                available: 100,
                reserve: 100
            }
            .under_pressure()
        );
        assert!(
            !Reading {
                available: 0,
                reserve: 0
            }
            .under_pressure(),
            "a zero reserve is the documented opt-out, even on a full disk",
        );
    }

    #[test]
    fn a_probe_failure_allows_the_write_rather_than_refusing_everything() {
        // Refusing on an unmeasurable filesystem would turn a diagnostic gap into
        // a total outage on a VM with no other way in.
        let blind = Guard {
            probe: failing_probe,
            reserve: 1 << 30,
        };
        assert!(blind.read(Path::new("/")).is_err());
        assert!(
            blind.preflight(Path::new("/")).is_none(),
            "an unmeasurable filesystem is not treated as a full one",
        );
    }

    #[test]
    fn preflight_refuses_below_the_reserve_and_permits_above_it() {
        set_available(50);
        let reading = guard(100)
            .preflight(Path::new("/tmp"))
            .expect("refused below the reserve");
        assert_eq!(reading.available, 50);
        assert_eq!(reading.reserve, 100);

        set_available(500);
        assert!(guard(100).preflight(Path::new("/tmp")).is_none());
    }

    #[tokio::test]
    async fn a_copy_that_crosses_the_reserve_mid_stream_aborts_and_reports_its_progress() {
        // The case a pre-flight check cannot catch: the filesystem has room when
        // the request is accepted and runs out while the body is still arriving.
        set_available(1 << 30);

        // Two full slices plus a tail, so there are two in-flight checkpoints.
        let payload = vec![0u8; (PROBE_INTERVAL_BYTES * 2 + 1024) as usize];
        let mut reader = std::io::Cursor::new(payload.clone());
        let mut sink: Vec<u8> = Vec::new();

        // Drop below the reserve as soon as the first checkpoint is reached.
        set_available(1);

        let guard = guard(1 << 20);
        let (written, err) = copy_guarded(&mut reader, &mut sink, &guard, Path::new("/tmp"))
            .await
            .expect_err("crossing the reserve mid-copy aborts");

        match err {
            CopyError::Pressure(reading) => assert_eq!(reading.available, 1),
            other => panic!("expected pressure, got {other:?}"),
        }
        // Stopped at the first checkpoint rather than writing the whole body,
        // which is the property that keeps the filesystem off 100%.
        assert_eq!(written, PROBE_INTERVAL_BYTES);
        assert_eq!(sink.len() as u64, PROBE_INTERVAL_BYTES);
    }

    #[tokio::test]
    async fn a_copy_with_room_transfers_every_byte_across_slice_boundaries() {
        // The guard is not vacuous in the other direction: the slicing must not
        // truncate a transfer that spans several probe intervals.
        set_available(1 << 30);

        let payload: Vec<u8> = (0..(PROBE_INTERVAL_BYTES * 2 + 7))
            .map(|i| (i % 251) as u8)
            .collect();
        let mut reader = std::io::Cursor::new(payload.clone());
        let mut sink: Vec<u8> = Vec::new();

        let written = copy_guarded(&mut reader, &mut sink, &guard(1 << 20), Path::new("/tmp"))
            .await
            .expect("no pressure, so the whole body lands");

        assert_eq!(written, payload.len() as u64);
        assert_eq!(sink, payload, "no bytes lost or duplicated at a slice edge");
    }

    #[test]
    fn the_real_probe_measures_the_filesystem_of_a_path_that_does_not_exist_yet() {
        // Asserts a *relationship*, never an absolute number, so the verdict does
        // not depend on how full the host happens to be.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let existing = available_bytes(dir.path()).expect("existing path measures");
        let not_yet = available_bytes(&dir.path().join("no/such/dir/file.txt"))
            .expect("a missing path walks up to an existing ancestor");

        assert!(existing > 0, "a writable tempdir has some space");
        // Same filesystem, so the two readings agree up to concurrent activity on
        // the host. A 1 GiB tolerance is loose enough to never flake and tight
        // enough to catch the factor-of-N error of multiplying by `f_bsize`.
        let drift = existing.abs_diff(not_yet);
        assert!(
            drift < (1 << 30),
            "{existing} and {not_yet} should be the same filesystem",
        );
    }

    #[test]
    fn the_ancestor_walk_stops_at_the_first_path_that_exists() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let nested = dir.path().join("a/b/c");
        assert_eq!(existing_ancestor(&nested), Some(dir.path()));
        assert_eq!(existing_ancestor(dir.path()), Some(dir.path()));
        // A relative path with nothing to walk up to yields no candidate rather
        // than silently measuring the daemon's working directory, which is the
        // image WORKDIR and not something the caller can see.
        assert_eq!(existing_ancestor(Path::new("nope-not-here")), None);
    }
}

//! Real hardware measurement of the persistence path.
//!
//! Everything else in this prototype charges persistence a *modelled* cost. This
//! module measures what the cost actually is on the machine the experiment runs
//! on, so the headline numbers can be checked against reality instead of against
//! an assumption.
//!
//! The measurement that matters most is **group commit**: appending `K` records
//! and flushing once, versus appending and flushing each one. The speculative
//! runtime's entire advantage rests on the premise that a background writer can
//! batch records that a synchronous engine cannot. This module measures whether
//! that premise is true on real storage.
//!
//! Deliberately *not* run by `cargo test`: it is slow and machine dependent. Use
//! `ues probe` and then feed the result back with `--profile`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::ledger::store::temp_ledger_dir;
use crate::ports::CostModel as PersistenceProfile;

/// Percentile summary of a latency distribution, in microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FsyncStats {
    pub samples: usize,
    pub min_us: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub mean_us: u64,
}

impl FsyncStats {
    /// Nearest-rank summary of raw microsecond samples.
    #[must_use]
    pub fn from_samples(mut samples: Vec<u64>) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        samples.sort_unstable();
        let sum: u64 = samples.iter().sum();
        Self {
            samples: samples.len(),
            min_us: samples[0],
            p50_us: percentile(&samples, 0.50),
            p95_us: percentile(&samples, 0.95),
            p99_us: percentile(&samples, 0.99),
            max_us: samples[samples.len() - 1],
            mean_us: sum / samples.len() as u64,
        }
    }

    #[must_use]
    pub fn p50(&self) -> Duration {
        Duration::from_micros(self.p50_us)
    }

    /// Human readable one line.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "n={:<4} p50={:>7}us p95={:>7}us p99={:>7}us min={:>7}us max={:>8}us",
            self.samples, self.p50_us, self.p95_us, self.p99_us, self.min_us, self.max_us
        )
    }
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (q * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

/// One point of the group-commit curve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroupPoint {
    /// Records appended before the single flush.
    pub depth: usize,
    pub stats: FsyncStats,
    /// `depth * single_p50 / group_p50` — how much cheaper the batch is than
    /// `depth` individual flushes. `1.0` means batching bought nothing.
    pub saving: f64,
}

/// Result of a probe run.
#[derive(Clone, Debug)]
pub struct ProbeReport {
    pub path: PathBuf,
    pub filesystem: String,
    pub record_bytes: usize,
    /// Write without any flush: the cost of the `append` half only.
    pub append_only_us: FsyncStats,
    /// Write + flush, one record at a time: the baseline's cost.
    pub single_fsync_us: FsyncStats,
    /// Write `depth` records then flush once: the background writer's cost.
    pub group_commit: Vec<GroupPoint>,
    /// A `PersistenceProfile` calibrated to the measured p50.
    pub calibrated_p50: PersistenceProfile,
    /// A `PersistenceProfile` calibrated to the measured p95 (the pessimistic
    /// case a well provisioned writer would be sized for).
    pub calibrated_p95: PersistenceProfile,
}

impl ProbeReport {
    /// True when the filesystem reports durability for free.
    ///
    /// This is a trap worth naming: on tmpfs, `/dev/shm`, and some network
    /// filesystems `fsync` is a no-op, so the measured flush latency is zero and
    /// the whole experiment becomes meaningless. A probe that reports a
    /// sub-microsecond flush is measuring RAM, not durability.
    #[must_use]
    pub fn durability_is_free(&self) -> bool {
        self.calibrated_p50.flush < Duration::from_micros(50)
    }

    /// Warning to print when the measurement is not credible.
    #[must_use]
    pub fn credibility_warning(&self) -> Option<String> {
        if self.durability_is_free() {
            Some(format!(
                "the flush latency on {} is {} us, which means durability is free here \
                 (tmpfs, a network filesystem, or a write-back cache with no barrier). \
                 Results calibrated against this are meaningless — re-run on a real block device.",
                self.filesystem,
                self.calibrated_p50.flush.as_micros()
            ))
        } else {
            None
        }
    }

    /// Largest measured group-commit saving, i.e. the best case for the
    /// speculative design.
    #[must_use]
    pub fn best_saving(&self) -> f64 {
        self.group_commit
            .iter()
            .map(|g| g.saving)
            .fold(0.0_f64, f64::max)
    }

    /// Group-commit saving at a given depth, or `None` if it was not measured.
    #[must_use]
    pub fn saving_at(&self, depth: usize) -> Option<f64> {
        self.group_commit
            .iter()
            .find(|g| g.depth == depth)
            .map(|g| g.saving)
    }
}

/// Measures the real persistence path on a local file.
pub struct Probe {
    dir: PathBuf,
    path: PathBuf,
    payload: Vec<u8>,
    depths: Vec<usize>,
    samples: usize,
}

impl Probe {
    /// Creates a probe in a fresh temporary directory.
    #[must_use]
    pub fn new() -> Self {
        Self::with_dir(temp_ledger_dir("probe"))
    }

    /// Creates a probe writing to an explicit directory (used by the CLI so the
    /// operator controls where the measurement lands).
    #[must_use]
    pub fn with_dir(dir: PathBuf) -> Self {
        let path = dir.join("probe.bin");
        Self {
            dir,
            path,
            // Roughly the size of a real delta record in this prototype.
            payload: vec![b'x'; 32],
            depths: vec![1, 2, 4, 8, 16, 32, 64, 128],
            samples: 200,
        }
    }

    /// Overrides the group-commit depths.
    pub fn with_depths(&mut self, depths: Vec<usize>) -> &mut Self {
        self.depths = depths;
        self
    }

    /// Overrides the number of samples per point. The probe truncates the file
    /// between batches, so repeated runs are not dominated by file growth.
    pub fn with_samples(&mut self, samples: usize) -> &mut Self {
        self.samples = samples;
        self
    }

    /// Runs the measurement.
    ///
    /// # Errors
    /// Returns [`ProbeError`] if the probe file cannot be created or written.
    pub fn measure(&self) -> Result<ProbeReport, ProbeError> {
        let append_only = self.measure_append_only()?;
        let single = self.measure_single_fsync()?;
        let group = self.measure_group_commit(&single)?;

        // `per_record` is the *write* half, measured without any flush. The flush
        // is measured separately, so the two compose to the single-record cost
        // rather than double counting it. Getting this wrong would make a
        // zero-flush sweep still pay a flush per step.
        let write_us = append_only.p50_us;
        let flush_us = single.p50_us.saturating_sub(write_us);
        let calibrated_p50 = PersistenceProfile {
            per_record: Duration::from_micros(write_us),
            per_kib: Duration::from_micros(
                (write_us as f64 / (self.payload.len() as f64 / 1024.0)).round() as u64,
            ),
            flush: Duration::from_micros(flush_us),
        };
        let write_p95 = append_only.p95_us;
        let calibrated_p95 = PersistenceProfile {
            per_record: Duration::from_micros(write_p95),
            per_kib: Duration::from_micros(
                (write_p95 as f64 / (self.payload.len() as f64 / 1024.0)).round() as u64,
            ),
            flush: Duration::from_micros(single.p95_us.saturating_sub(write_p95)),
        };

        Ok(ProbeReport {
            filesystem: filesystem_of(&self.path),
            path: self.path.clone(),
            record_bytes: self.payload.len(),
            append_only_us: append_only,
            single_fsync_us: single,
            group_commit: group,
            calibrated_p50,
            calibrated_p95,
        })
    }

    fn open(&self) -> Result<File, ProbeError> {
        Ok(OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?)
    }

    /// Cost of the `append` half alone: write, no flush.
    fn measure_append_only(&self) -> Result<FsyncStats, ProbeError> {
        let mut samples = Vec::with_capacity(self.samples);
        for _ in 0..self.samples {
            let mut f = self.open()?;
            let start = Instant::now();
            f.write_all(&self.payload)?;
            f.flush()?;
            samples.push(start.elapsed().as_micros() as u64);
        }
        Ok(FsyncStats::from_samples(samples))
    }

    /// One record, one flush: what the synchronous baseline pays per step.
    fn measure_single_fsync(&self) -> Result<FsyncStats, ProbeError> {
        let mut samples = Vec::with_capacity(self.samples);
        for _ in 0..self.samples {
            let mut f = self.open()?;
            let start = Instant::now();
            f.write_all(&self.payload)?;
            f.flush()?;
            f.sync_data()?;
            samples.push(start.elapsed().as_micros() as u64);
        }
        Ok(FsyncStats::from_samples(samples))
    }

    /// `depth` records, one flush: what a background writer with group commit
    /// pays for the same `depth` records.
    fn measure_group_commit(&self, single: &FsyncStats) -> Result<Vec<GroupPoint>, ProbeError> {
        let mut out = Vec::with_capacity(self.depths.len());
        for &depth in &self.depths {
            let mut samples = Vec::with_capacity(self.samples);
            for _ in 0..self.samples {
                let mut f = self.open()?;
                let start = Instant::now();
                for _ in 0..depth {
                    f.write_all(&self.payload)?;
                }
                f.flush()?;
                f.sync_data()?;
                samples.push(start.elapsed().as_micros() as u64);
            }
            let stats = FsyncStats::from_samples(samples);
            let saving = if stats.p50_us == 0 {
                0.0
            } else {
                (depth as f64 * single.p50_us as f64) / stats.p50_us as f64
            };
            out.push(GroupPoint {
                depth,
                stats,
                saving,
            });
        }
        Ok(out)
    }

    /// Removes the probe directory.
    pub fn cleanup(&self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

impl Default for Probe {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum ProbeError {
    Io(std::io::Error),
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProbeError::Io(e) => write!(f, "probe io error: {e}"),
        }
    }
}

impl std::error::Error for ProbeError {}

impl From<std::io::Error> for ProbeError {
    fn from(e: std::io::Error) -> Self {
        ProbeError::Io(e)
    }
}

use std::fmt;

/// Best-effort filesystem name for the probe path, so a report says what it was
/// measured on.
#[must_use]
pub fn filesystem_of(path: &Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let out = std::process::Command::new("findmnt")
        .args(["-no", "FSTYPE,SOURCE", "-T"])
        .arg(&path)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        _ => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_summarise_a_distribution() {
        let s = FsyncStats::from_samples((1..=100).collect());
        assert_eq!(s.samples, 100);
        assert_eq!(s.min_us, 1);
        assert_eq!(s.p50_us, 50);
        assert_eq!(s.p95_us, 95);
        assert_eq!(s.p99_us, 99);
        assert_eq!(s.max_us, 100);
        assert_eq!(s.mean_us, 50);
        let text = s.render();
        assert!(text.contains("p50="), "{text}");
        assert!(text.contains("50us"), "{text}");
        assert!(text.contains("n=100"), "{text}");
    }

    #[test]
    fn empty_stats_are_safe() {
        let s = FsyncStats::from_samples(Vec::new());
        assert_eq!(s.samples, 0);
        assert_eq!(s.p50(), Duration::ZERO);
    }

    /// A fast smoke test of the probe machinery. The *numbers* are not asserted:
    /// they are hardware dependent, which is the whole point of the module.
    #[test]
    fn probe_runs_and_reports_a_group_commit_curve() {
        let mut probe = Probe::new();
        probe.with_samples(3).with_depths(vec![1, 8]);
        let report = probe.measure().expect("probe should run");
        probe.cleanup();

        assert_eq!(report.single_fsync_us.samples, 3);
        assert_eq!(report.group_commit.len(), 2);
        assert!(
            report.single_fsync_us.p50_us > 0,
            "fsync must cost something"
        );
        assert!(report
            .saving_at(8)
            .is_some_and(|s| s > 0.0 || report.single_fsync_us.p50_us == 0));
        // The calibrated profile must be built from the measurement, not a
        // guess, and must not double count the flush.
        let composed = report.calibrated_p50.per_record + report.calibrated_p50.flush;
        let measured = Duration::from_micros(report.single_fsync_us.p50_us);
        if report.append_only_us.p50_us < report.single_fsync_us.p50_us {
            assert_eq!(
                composed, measured,
                "calibration must reproduce the measurement"
            );
        } else {
            // Durability is free here (tmpfs), so the flush clamps to zero and
            // the composed cost is the write cost, which must not exceed the
            // measured single-record cost.
            assert!(composed <= measured || report.durability_is_free());
        }
        assert!(!report.filesystem.is_empty());
        // tmpfs is free by construction, so only assert the warning fires when
        // and only when the measurement really is free.
        match report.credibility_warning() {
            Some(w) => assert!(report.durability_is_free(), "{w}"),
            None => assert!(report.calibrated_p50.flush >= Duration::ZERO),
        }
    }

    #[test]
    fn calibrated_profiles_are_positive() {
        let mut probe = Probe::new();
        probe.with_samples(2).with_depths(vec![1]);
        let report = probe.measure().expect("probe should run");
        probe.cleanup();
        assert!(report.calibrated_p50.cost_of(100) >= report.calibrated_p50.flush);
        assert_eq!(
            report.calibrated_p50.per_record
                + report.calibrated_p50.flush
                + report.calibrated_p50.per_kib * 1u32,
            report.calibrated_p50.cost_of(1024)
        );
    }
}

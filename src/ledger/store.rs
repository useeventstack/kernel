//! Store implementations: shared framing plus in-memory and filesystem sinks.
//!
//! `LogCore` owns the framing, the write buffer and the index. Both sinks use it,
//! so their truncation and torn-tail semantics cannot drift apart — which matters,
//! because those semantics *are* the durability guarantee the whole runtime rests
//! on.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::domain::version::LogPosition;
use crate::ports::{DurableStore, ScanReport, StoreError};

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Creates a fresh directory under the system temp dir, unique per process and
/// per call. Used by tests and by the CLI so runs never collide.
pub fn temp_ledger_dir(prefix: &str) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("ues-{prefix}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create temp ledger dir");
    path
}

const HEADER_LEN: usize = 8;

/// Shared record framing, write buffer and index.
#[derive(Debug, Default)]
pub struct LogCore {
    durable_bytes: Vec<u8>,
    /// `(offset, payload length)` of every durable record.
    index: Vec<(u64, u32)>,
    pending: Vec<u8>,
    /// Offset of each buffered record inside `pending`.
    pending_starts: Vec<u64>,
    pending_records: usize,
}

impl LogCore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Buffers a frame and returns its byte position.
    pub fn append(&mut self, payload: &[u8]) -> LogPosition {
        let offset = (self.durable_bytes.len() + self.pending.len()) as u64;
        self.pending_starts.push(self.pending.len() as u64);
        self.pending
            .extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.pending
            .extend_from_slice(&fnv1a32(payload).to_le_bytes());
        self.pending.extend_from_slice(payload);
        self.pending_records += 1;
        LogPosition::new(offset)
    }

    /// End of the write buffer after the last appended record.
    pub fn written_end(&self) -> LogPosition {
        LogPosition::new((self.durable_bytes.len() + self.pending.len()) as u64)
    }

    /// Number of bytes waiting to be made durable.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn pending_records(&self) -> usize {
        self.pending_records
    }

    /// Consumes the pending buffer so the sink can write it.
    pub fn take_pending(&mut self) -> Vec<u8> {
        self.pending_records = 0;
        self.pending_starts.clear();
        std::mem::take(&mut self.pending)
    }

    /// Removes the buffered bytes up to `position` and returns them, so the caller
    /// can write exactly that prefix. Records beyond `position` stay in the buffer
    /// and remain unrecoverable.
    ///
    /// This is what makes group commit honest: the writer commits a *prefix*, and
    /// everything handed to it after the batch started is still only in memory.
    pub fn take_prefix(&mut self, position: LogPosition) -> Result<Vec<u8>, StoreError> {
        let keep = self.prefix_len(position)?;
        let bytes: Vec<u8> = self.pending.drain(..keep).collect();
        let drop = self
            .pending_starts
            .iter()
            .position(|s| *s >= keep as u64)
            .unwrap_or(self.pending_starts.len());
        self.pending_starts.drain(..drop);
        for start in &mut self.pending_starts {
            *start -= keep as u64;
        }
        self.pending_records = self.pending_starts.len();
        Ok(bytes)
    }

    /// Number of buffered bytes covered by `position`.
    fn prefix_len(&self, position: LogPosition) -> Result<usize, StoreError> {
        let durable = self.durable_bytes.len() as u64;
        let total = durable + self.pending.len() as u64;
        if position.value() > total {
            return Err(StoreError::InvalidPosition(position));
        }
        Ok((position.value().saturating_sub(durable)) as usize)
    }

    /// Marks freshly written bytes as durable and reindexes.
    pub fn commit(&mut self, bytes: &[u8]) {
        self.durable_bytes.extend_from_slice(bytes);
        self.reindex();
    }

    /// Replaces the durable image, used when re-reading from a provider.
    pub fn load(&mut self, bytes: Vec<u8>) {
        self.durable_bytes = bytes;
        self.pending.clear();
        self.pending_starts.clear();
        self.pending_records = 0;
        self.reindex();
    }

    pub fn read_from(&self, position: LogPosition) -> Result<Vec<Vec<u8>>, StoreError> {
        let start = position.value();
        if start == self.durable_bytes.len() as u64 {
            return Ok(Vec::new());
        }
        let first = self
            .index
            .iter()
            .position(|(offset, _)| *offset == start)
            .ok_or(StoreError::InvalidPosition(position))?;
        let mut out = Vec::with_capacity(self.index.len() - first);
        for (offset, len) in &self.index[first..] {
            let begin = *offset as usize + HEADER_LEN;
            let end = begin + *len as usize;
            out.push(self.durable_bytes[begin..end].to_vec());
        }
        Ok(out)
    }

    pub fn all_records(&self) -> Result<Vec<Vec<u8>>, StoreError> {
        self.read_from(LogPosition::START)
    }

    pub fn sync_position(&self) -> LogPosition {
        LogPosition::new(self.durable_bytes.len() as u64)
    }

    pub fn written_position(&self) -> LogPosition {
        LogPosition::new((self.durable_bytes.len() + self.pending.len()) as u64)
    }

    pub fn durable_records(&self) -> usize {
        self.index.len()
    }

    pub fn record_positions(&self) -> Vec<LogPosition> {
        self.index
            .iter()
            .map(|(offset, _)| LogPosition::new(*offset))
            .collect()
    }

    pub fn durable_len(&self) -> usize {
        self.durable_bytes.len()
    }

    /// Rebuilds the record index, stopping at the first invalid or torn record.
    pub fn reindex(&mut self) {
        self.index = scan_prefix(&self.durable_bytes).0;
    }

    /// Drops a torn or corrupt tail from the durable image.
    pub fn repair(&mut self) -> ScanReport {
        let total = self.durable_bytes.len();
        let (index, valid) = scan_prefix(&self.durable_bytes);
        self.durable_bytes.truncate(valid);
        self.index = index;
        self.pending.clear();
        self.pending_starts.clear();
        self.pending_records = 0;
        ScanReport {
            valid_records: self.index.len(),
            valid_bytes: valid as u64,
            torn_bytes_dropped: (total - valid) as u64,
        }
    }

    /// Drops everything at or after `position`.
    pub fn truncate(&mut self, position: LogPosition) -> Result<(), StoreError> {
        let end = position.value() as usize;
        let at_boundary =
            end == self.durable_bytes.len() || self.index.iter().any(|(o, _)| *o as usize == end);
        if end > self.durable_bytes.len() || !at_boundary {
            return Err(StoreError::InvalidPosition(position));
        }
        self.durable_bytes.truncate(end);
        self.index.retain(|(offset, _)| (*offset as usize) < end);
        self.pending.clear();
        self.pending_starts.clear();
        self.pending_records = 0;
        Ok(())
    }

    /// Test hook: appends arbitrary bytes to simulate a torn write.
    pub fn append_raw(&mut self, bytes: &[u8]) {
        self.durable_bytes.extend_from_slice(bytes);
    }
}

/// Scans the longest valid prefix of the image.
///
/// Anything after the first invalid frame is a torn or corrupt tail and is not part
/// of the durable prefix. This single function is what makes "recovery yields a
/// valid prefix at every damage point" a structural property rather than a test.
fn scan_prefix(bytes: &[u8]) -> (Vec<(u64, u32)>, usize) {
    let mut index = Vec::new();
    let mut pos = 0usize;
    loop {
        if pos == bytes.len() {
            return (index, pos);
        }
        if pos + HEADER_LEN > bytes.len() {
            return (index, pos);
        }
        let len = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
        let checksum = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]);
        let body_start = pos + HEADER_LEN;
        let Some(body_end) = body_start.checked_add(len as usize) else {
            return (index, pos);
        };
        if body_end > bytes.len() {
            return (index, pos);
        }
        if fnv1a32(&bytes[body_start..body_end]) != checksum {
            return (index, pos);
        }
        index.push((pos as u64, len));
        pos = body_end;
    }
}

fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// In-memory store. Identical semantics to the filesystem store, no IO.
#[derive(Debug, Default)]
pub struct MemoryStore {
    core: LogCore,
}

impl MemoryStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            core: LogCore::new(),
        }
    }

    /// Total size in bytes, including the not yet durable buffer.
    pub fn size(&self) -> usize {
        self.core.durable_len() + self.core.pending_len()
    }

    /// Appends arbitrary bytes, simulating a write that was interrupted.
    pub fn append_raw(&mut self, bytes: &[u8]) {
        self.core.append_raw(bytes);
    }

    /// End of the write buffer, used by tests to build commit positions.
    pub fn written_end(&self) -> LogPosition {
        self.core.written_end()
    }

    /// Bytes waiting to be made durable.
    pub fn pending_len(&self) -> usize {
        self.core.pending_len()
    }

    /// Number of frames waiting to be made durable.
    pub fn pending_records(&self) -> usize {
        self.core.pending_records()
    }
}

impl DurableStore for MemoryStore {
    fn append(&mut self, payload: Vec<u8>) -> Result<LogPosition, StoreError> {
        Ok(self.core.append(&payload))
    }

    fn written_end(&self) -> LogPosition {
        self.core.written_end()
    }

    fn commit_all(&mut self) -> Result<(), StoreError> {
        let bytes = self.core.take_pending();
        if !bytes.is_empty() {
            self.core.commit(&bytes);
        }
        Ok(())
    }

    fn commit_prefix(&mut self, position: LogPosition) -> Result<(), StoreError> {
        let bytes = self.core.take_prefix(position)?;
        if !bytes.is_empty() {
            self.core.commit(&bytes);
        }
        Ok(())
    }

    fn read_from(&self, position: LogPosition) -> Result<Vec<Vec<u8>>, StoreError> {
        self.core.read_from(position)
    }

    fn sync_position(&self) -> LogPosition {
        self.core.sync_position()
    }

    fn written_position(&self) -> LogPosition {
        self.core.written_position()
    }

    fn truncate_to(&mut self, position: LogPosition) -> Result<(), StoreError> {
        self.core.truncate(position)
    }

    fn recover(&mut self) -> Result<ScanReport, StoreError> {
        Ok(self.core.repair())
    }

    fn durable_records(&self) -> usize {
        self.core.durable_records()
    }

    fn path(&self) -> Option<&Path> {
        None
    }
}

/// Append-only store backed by a local file.
#[derive(Debug)]
pub struct FileStore {
    core: LogCore,
    writer: File,
    reader: File,
    path: PathBuf,
}

impl FileStore {
    /// Opens (creating if needed) the store at `path`, rebuilding the in-memory
    /// index from whatever is already there. This is the recovery entry point: it
    /// never destroys data.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the file cannot be opened.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with(path, false)
    }

    /// Creates a **fresh** store at `path`, discarding anything already there.
    ///
    /// One execution owns one ledger. Truncating on create is what stops two runs
    /// from silently sharing a durable prefix: a replay would otherwise see the
    /// previous run's records and produce a graph that no single run produced.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the file cannot be created.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with(path, true)
    }

    fn open_with(path: impl AsRef<Path>, truncate: bool) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        if truncate {
            File::create(&path)?.sync_all()?;
        }
        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let mut reader = File::open(&path)?;
        let mut image = Vec::new();
        reader.read_to_end(&mut image)?;
        let mut core = LogCore::new();
        core.load(image);
        Ok(Self {
            core,
            writer,
            reader,
            path,
        })
    }

    /// Total size of the file in bytes.
    pub fn file_size(&self) -> usize {
        self.core.durable_len()
    }
}

impl DurableStore for FileStore {
    fn append(&mut self, payload: Vec<u8>) -> Result<LogPosition, StoreError> {
        Ok(self.core.append(&payload))
    }

    fn written_end(&self) -> LogPosition {
        self.core.written_end()
    }

    fn commit_all(&mut self) -> Result<(), StoreError> {
        let bytes = self.core.take_pending();
        if bytes.is_empty() {
            return Ok(());
        }
        self.writer.write_all(&bytes)?;
        self.writer.flush()?;
        self.writer.sync_data()?;
        self.core.commit(&bytes);
        Ok(())
    }

    fn commit_prefix(&mut self, position: LogPosition) -> Result<(), StoreError> {
        let bytes = self.core.take_prefix(position)?;
        if bytes.is_empty() {
            return Ok(());
        }
        self.writer.write_all(&bytes)?;
        self.writer.flush()?;
        self.writer.sync_data()?;
        self.core.commit(&bytes);
        Ok(())
    }

    fn read_from(&self, position: LogPosition) -> Result<Vec<Vec<u8>>, StoreError> {
        self.core.read_from(position)
    }

    fn sync_position(&self) -> LogPosition {
        self.core.sync_position()
    }

    fn written_position(&self) -> LogPosition {
        self.core.written_position()
    }

    fn truncate_to(&mut self, position: LogPosition) -> Result<(), StoreError> {
        self.core.truncate(position)?;
        self.writer.set_len(self.core.durable_len() as u64)?;
        self.writer.sync_all()?;
        Ok(())
    }

    fn recover(&mut self) -> Result<ScanReport, StoreError> {
        // Re-read from the provider: the in-memory image can be ahead of what
        // actually reached the file if a previous commit was interrupted.
        let mut image = Vec::new();
        self.reader.seek(SeekFrom::Start(0))?;
        self.reader.read_to_end(&mut image)?;
        self.core.load(image);
        let report = self.core.repair();
        if report.torn_bytes_dropped > 0 {
            // Physically remove the torn tail so a second scan is clean.
            self.writer.set_len(self.core.durable_len() as u64)?;
            self.writer.sync_all()?;
            let mut trimmed = Vec::new();
            self.reader.seek(SeekFrom::Start(0))?;
            self.reader.read_to_end(&mut trimmed)?;
            self.core.load(trimmed);
        }
        Ok(report)
    }

    fn durable_records(&self) -> usize {
        self.core.durable_records()
    }

    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(n: u64) -> Vec<u8> {
        format!("record-{n}-with-some-payload").into_bytes()
    }

    #[test]
    fn records_round_trip() {
        let mut store = MemoryStore::new();
        store.append(payload(0)).unwrap();
        store.append(payload(1)).unwrap();
        store.commit_all().unwrap();
        let records = store.read_from(LogPosition::START).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1], payload(1));
    }

    #[test]
    fn buffered_records_are_not_durable() {
        let mut store = MemoryStore::new();
        store.append(payload(0)).unwrap();
        assert_eq!(store.durable_records(), 0);
        assert!(store.read_from(LogPosition::START).unwrap().is_empty());
        assert!(store.sync_position() < store.written_position());
        store.commit_all().unwrap();
        assert_eq!(store.durable_records(), 1);
    }

    #[test]
    fn a_prefix_commit_leaves_the_rest_buffered() {
        let mut store = MemoryStore::new();
        store.append(payload(0)).unwrap();
        let second = store.append(payload(1)).unwrap();
        store.append(payload(2)).unwrap();
        store.commit_prefix(second).unwrap();
        assert_eq!(store.durable_records(), 1, "only the prefix became durable");
        assert_eq!(store.pending_records(), 2);
        store.commit_all().unwrap();
        assert_eq!(store.durable_records(), 3);
    }

    #[test]
    fn torn_tail_is_detected_and_dropped() {
        let mut store = MemoryStore::new();
        store.append(payload(0)).unwrap();
        store.append(payload(1)).unwrap();
        store.commit_all().unwrap();
        let good_len = store.core.durable_len();
        store.append_raw(&[9, 0, 0, 0, 1, 2, 3, 4, 7]);
        let report = store.recover().unwrap();
        assert_eq!(report.valid_records, 2);
        assert!(report.torn_bytes_dropped > 0);
        assert_eq!(store.sync_position().value() as usize, good_len);
    }

    #[test]
    fn corrupt_payload_is_detected_by_checksum() {
        let mut store = MemoryStore::new();
        store.append(payload(0)).unwrap();
        store.commit_all().unwrap();
        store.append_raw(&[2, 0, 0, 0, 0, 0, 0, 0, 9, 9]);
        let report = store.recover().unwrap();
        assert_eq!(report.valid_records, 1);
        assert_eq!(report.torn_bytes_dropped, 10);
    }

    #[test]
    fn truncate_drops_the_tail_and_refuses_a_non_boundary() {
        let mut store = MemoryStore::new();
        store.append(payload(0)).unwrap();
        let p1 = store.append(payload(1)).unwrap();
        store.commit_all().unwrap();
        assert!(store.truncate_to(LogPosition::new(3)).is_err());
        store.truncate_to(p1).unwrap();
        assert_eq!(store.durable_records(), 1);
    }

    #[test]
    fn read_from_a_non_boundary_errors() {
        let mut store = MemoryStore::new();
        store.append(payload(0)).unwrap();
        store.commit_all().unwrap();
        assert!(matches!(
            store.read_from(LogPosition::new(2)),
            Err(StoreError::InvalidPosition(_))
        ));
    }

    #[test]
    fn records_survive_reopening_the_file() {
        let dir = temp_ledger_dir("fs-roundtrip");
        let path = dir.join("a.log");
        {
            let mut store = FileStore::open(&path).unwrap();
            store.append(payload(0)).unwrap();
            store.append(payload(1)).unwrap();
            store.commit_all().unwrap();
            assert_eq!(store.durable_records(), 2);
        }
        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.read_from(LogPosition::START).unwrap().len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn torn_tail_is_repaired_on_reopen() {
        let dir = temp_ledger_dir("fs-torn");
        let path = dir.join("a.log");
        {
            let mut store = FileStore::open(&path).unwrap();
            store.append(payload(0)).unwrap();
            store.commit_all().unwrap();
        }
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[4, 0, 0, 0, 9, 9, 9, 9, 1, 2]).unwrap();
        }
        let mut store = FileStore::open(&path).unwrap();
        let report = store.recover().unwrap();
        assert_eq!(report.valid_records, 1);
        assert_eq!(report.torn_bytes_dropped, 10);
        assert_eq!(store.recover().unwrap().torn_bytes_dropped, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncate_shrinks_the_file() {
        let dir = temp_ledger_dir("fs-truncate");
        let path = dir.join("a.log");
        let mut store = FileStore::open(&path).unwrap();
        store.append(payload(0)).unwrap();
        let p1 = store.append(payload(1)).unwrap();
        store.commit_all().unwrap();
        let full = std::fs::metadata(&path).unwrap().len();
        store.truncate_to(p1).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < full);
        assert_eq!(store.durable_records(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn buffered_records_are_not_visible_after_reopen() {
        let dir = temp_ledger_dir("fs-buffered");
        let path = dir.join("a.log");
        {
            let mut store = FileStore::open(&path).unwrap();
            store.append(payload(0)).unwrap();
            // No commit: the record never reaches the file.
        }
        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.durable_records(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}

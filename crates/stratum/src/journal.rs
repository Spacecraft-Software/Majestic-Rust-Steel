// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The append-only edit [`Journal`] and crash recovery.
//!
//! The journal is a write-ahead log of [`EditOp`]s applied since the last save. After an
//! abnormal exit, [`Journal::recover`] reads back the intact records and [`replay`] applies
//! them onto the last saved content, reconstructing the in-memory document — Emacs-style
//! recovery, the contract's ≤ 1 s crash-loss guarantee.
//!
//! # Durability model
//! Each record is `write`-n to the file immediately, so a `SIGKILL` (which leaves the
//! kernel's already-written bytes intact) loses nothing. `sync_data` is called per
//! [`FlushPolicy`] (default: every 64 ops or 1 second) to additionally survive power loss —
//! that interval is the bound on what a hard crash can cost.
//!
//! # On-disk format
//! A header (`MJJRNL01` magic + base length, little-endian `u64`) followed by records:
//! `[len: u32-le][payload: len bytes][crc32: u32-le]`, where the payload is
//! `[start: u64-le][old_len: u64-le][inserted UTF-8 bytes]`. On read, a record whose bytes
//! are truncated (a crash mid-write) or whose CRC32 fails is treated as end-of-log and
//! dropped, so a partially written trailing record never corrupts recovery.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::rope::Rope;

/// Magic identifying a Majestic journal, version 1.
const MAGIC: &[u8; 8] = b"MJJRNL01";

/// Largest record payload accepted on read, to bound allocation against a corrupt length.
const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

/// A recorded edit: the bytes in `start..start + old_len` became `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditOp {
    /// Byte offset where the replaced range began (in the content at apply time).
    pub start: usize,
    /// Number of bytes removed.
    pub old_len: usize,
    /// The inserted text.
    pub text: String,
}

impl EditOp {
    /// Creates an edit op replacing `old_len` bytes at `start` with `text`.
    #[must_use]
    pub fn new(start: usize, old_len: usize, text: impl Into<String>) -> Self {
        Self {
            start,
            old_len,
            text: text.into(),
        }
    }
}

/// When the journal calls `sync_data` to make records durable against power loss.
#[derive(Clone, Copy, Debug)]
pub struct FlushPolicy {
    /// Sync after at most this many appended ops.
    pub max_ops: usize,
    /// Sync at least this often.
    pub max_interval: Duration,
}

impl Default for FlushPolicy {
    fn default() -> Self {
        Self {
            max_ops: 64,
            max_interval: Duration::from_secs(1),
        }
    }
}

/// What [`Journal::recover`] read back: the base length and the intact ops.
#[derive(Clone, Debug)]
pub struct Recovered {
    /// Byte length of the content the journal was written against.
    pub base_len: u64,
    /// The intact edit ops, in order.
    pub ops: Vec<EditOp>,
}

/// An append-only write-ahead log of edits, with a durability [`FlushPolicy`].
#[derive(Debug)]
pub struct Journal {
    file: File,
    policy: FlushPolicy,
    ops_since_sync: usize,
    last_sync: Instant,
}

impl Journal {
    /// Creates (truncating) a journal at `path` for content of `base_len` bytes.
    ///
    /// # Errors
    /// Returns any I/O error from opening the file or writing and syncing the header.
    pub fn create(path: &Path, base_len: u64) -> io::Result<Self> {
        Self::create_with_policy(path, base_len, FlushPolicy::default())
    }

    /// Like [`Journal::create`] with an explicit [`FlushPolicy`].
    ///
    /// # Errors
    /// Returns any I/O error from opening the file or writing and syncing the header.
    pub fn create_with_policy(path: &Path, base_len: u64, policy: FlushPolicy) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        write_header(&mut file, base_len)?;
        file.sync_data()?;
        Ok(Self {
            file,
            policy,
            ops_since_sync: 0,
            last_sync: Instant::now(),
        })
    }

    /// Appends `op`, writing it to the file immediately and syncing per the policy.
    ///
    /// # Errors
    /// Returns any I/O error from writing the record or syncing.
    pub fn append(&mut self, op: &EditOp) -> io::Result<()> {
        let mut buf = Vec::with_capacity(24 + op.text.len());
        write_record(&mut buf, op)?;
        self.file.write_all(&buf)?;
        self.ops_since_sync += 1;
        if self.ops_since_sync >= self.policy.max_ops
            || self.last_sync.elapsed() >= self.policy.max_interval
        {
            self.sync()?;
        }
        Ok(())
    }

    /// Forces a `sync_data`, resetting the flush counters.
    ///
    /// # Errors
    /// Returns any I/O error from syncing the file.
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()?;
        self.ops_since_sync = 0;
        self.last_sync = Instant::now();
        Ok(())
    }

    /// Reads back the base length and intact ops from the journal at `path`.
    ///
    /// A truncated or CRC-failed trailing record is treated as end-of-log and dropped.
    ///
    /// # Errors
    /// Returns an error if the file cannot be opened or its header is missing/invalid.
    pub fn recover(path: &Path) -> io::Result<Recovered> {
        let mut file = File::open(path)?;
        let (base_len, ops) = read_all(&mut file)?;
        Ok(Recovered { base_len, ops })
    }

    /// Opens an existing journal to append further records (e.g. after recovery), keeping
    /// its base content unchanged so the records already on disk stay valid.
    ///
    /// # Errors
    /// Returns an error if the file cannot be opened or does not begin with a valid header.
    pub fn open_append(path: &Path) -> io::Result<Self> {
        let mut header = [0u8; 8];
        File::open(path)?.read_exact(&mut header)?;
        if &header != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a Majestic journal",
            ));
        }
        let file = OpenOptions::new().append(true).open(path)?;
        Ok(Self {
            file,
            policy: FlushPolicy::default(),
            ops_since_sync: 0,
            last_sync: Instant::now(),
        })
    }
}

/// Replays `ops` onto `base`, returning the reconstructed rope.
///
/// Replay stops gracefully (rather than panicking) at the first op that does not apply
/// cleanly to the evolving content — out of bounds or off a `char` boundary — so a corrupt
/// journal degrades instead of crashing recovery.
#[must_use]
pub fn replay(base: &Rope, ops: &[EditOp]) -> Rope {
    let mut rope = base.clone();
    for op in ops {
        let Some(end) = op.start.checked_add(op.old_len) else {
            break;
        };
        if end > rope.len_bytes() || !rope.is_char_boundary(op.start) || !rope.is_char_boundary(end)
        {
            break;
        }
        rope = rope.replace(op.start..end, &op.text);
    }
    rope
}

// --- Serialization (private; tested directly over in-memory buffers) ---------------

fn write_header<W: Write>(writer: &mut W, base_len: u64) -> io::Result<()> {
    writer.write_all(MAGIC)?;
    writer.write_all(&base_len.to_le_bytes())
}

fn write_record<W: Write>(writer: &mut W, op: &EditOp) -> io::Result<()> {
    let mut payload = Vec::with_capacity(16 + op.text.len());
    payload.extend_from_slice(&(op.start as u64).to_le_bytes());
    payload.extend_from_slice(&(op.old_len as u64).to_le_bytes());
    payload.extend_from_slice(op.text.as_bytes());

    let Ok(len) = u32::try_from(payload.len()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "journal record too large",
        ));
    };
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.write_all(&crc32(&payload).to_le_bytes())
}

fn read_all<R: Read>(reader: &mut R) -> io::Result<(u64, Vec<EditOp>)> {
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a Majestic journal",
        ));
    }
    let mut base_buf = [0u8; 8];
    reader.read_exact(&mut base_buf)?;
    let base_len = u64::from_le_bytes(base_buf);

    let mut ops = Vec::new();
    loop {
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let len = u64::from(u32::from_le_bytes(len_buf));
        if len > MAX_RECORD_BYTES {
            break;
        }
        let Ok(len) = usize::try_from(len) else {
            break;
        };

        let mut payload = vec![0u8; len];
        if reader.read_exact(&mut payload).is_err() {
            break; // torn trailing record
        }
        let mut crc_buf = [0u8; 4];
        if reader.read_exact(&mut crc_buf).is_err() {
            break; // torn trailing record
        }
        if crc32(&payload) != u32::from_le_bytes(crc_buf) {
            break; // corrupt / partially written
        }
        if payload.len() < 16 {
            break;
        }

        let start = u64_le(&payload[0..8]);
        let old_len = u64_le(&payload[8..16]);
        let (Ok(start), Ok(old_len)) = (usize::try_from(start), usize::try_from(old_len)) else {
            break;
        };
        let Ok(text) = String::from_utf8(payload[16..].to_vec()) else {
            break;
        };
        ops.push(EditOp {
            start,
            old_len,
            text,
        });
    }
    Ok((base_len, ops))
}

fn u64_le(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(buf)
}

/// Reflected CRC-32 (IEEE), table-free.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{read_all, replay, write_header, write_record, EditOp, Journal};
    use crate::Rope;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "majestic-stratum-{tag}-{}-{n}.journal",
            std::process::id()
        ))
    }

    #[test]
    fn end_to_end_recover_matches_live_edits() {
        let base = Rope::from("hello world");
        let mut live = base.clone();
        let mut buf = Vec::new();
        write_header(&mut buf, base.len_bytes() as u64).unwrap();

        let ops = [
            EditOp::new(5, 0, ","),     // "hello, world"
            EditOp::new(0, 0, ">> "),   // ">> hello, world"
            EditOp::new(3, 5, "HELLO"), // ">> HELLO, world"
        ];
        for op in &ops {
            live = live.replace(op.start..op.start + op.old_len, &op.text);
            write_record(&mut buf, op).unwrap();
        }

        let (base_len, recovered) = read_all(&mut Cursor::new(buf)).unwrap();
        assert_eq!(base_len, base.len_bytes() as u64);
        assert_eq!(recovered, ops.to_vec());
        assert_eq!(replay(&base, &recovered).to_string(), live.to_string());
        assert_eq!(live.to_string(), ">> HELLO, world");
    }

    #[test]
    fn torn_trailing_record_is_dropped() {
        let mut buf = Vec::new();
        write_header(&mut buf, 0).unwrap();
        write_record(&mut buf, &EditOp::new(0, 0, "abc")).unwrap();
        let intact = buf.len();
        write_record(&mut buf, &EditOp::new(3, 0, "XYZ")).unwrap();
        buf.truncate(intact + 3); // crash mid second record

        let (_, ops) = read_all(&mut Cursor::new(buf)).unwrap();
        assert_eq!(ops, vec![EditOp::new(0, 0, "abc")]);
    }

    #[test]
    fn corrupt_crc_stops_reading() {
        let mut buf = Vec::new();
        write_header(&mut buf, 0).unwrap();
        write_record(&mut buf, &EditOp::new(0, 0, "abc")).unwrap();
        *buf.last_mut().unwrap() ^= 0xFF; // flip a CRC byte
        let (_, ops) = read_all(&mut Cursor::new(buf)).unwrap();
        assert!(ops.is_empty());
    }

    #[test]
    fn file_journal_survives_unsynced_drop() {
        // Records are write_all'd immediately, so they outlive a process death even
        // without an explicit sync (the kernel keeps the bytes). The CI SIGKILL kill-test
        // exercises a real signal; this asserts the same durability property in-process.
        let path = temp_path("survive");
        let mut journal = Journal::create(&path, 0).unwrap();
        for _ in 0..100 {
            journal.append(&EditOp::new(0, 0, "x")).unwrap();
        }
        drop(journal); // close the file without a final sync, as a kill would leave it
        let recovered = Journal::recover(&path).unwrap();
        assert_eq!(recovered.ops.len(), 100);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_journal_recovers_and_replays() {
        let path = temp_path("replay");
        let base = Rope::from("base");
        let mut journal = Journal::create(&path, base.len_bytes() as u64).unwrap();
        journal.append(&EditOp::new(4, 0, "!")).unwrap(); // "base!"
        journal.append(&EditOp::new(0, 0, "<")).unwrap(); // "<base!"
        journal.sync().unwrap();
        drop(journal);
        let recovered = Journal::recover(&path).unwrap();
        assert_eq!(recovered.base_len, base.len_bytes() as u64);
        assert_eq!(replay(&base, &recovered.ops).to_string(), "<base!");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_stops_on_out_of_bounds_op() {
        let base = Rope::from("abc");
        let ops = [EditOp::new(0, 0, "Z"), EditOp::new(99, 0, "bad")];
        // First op applies ("Zabc"); the second is out of bounds and is skipped.
        assert_eq!(replay(&base, &ops).to_string(), "Zabc");
    }
}

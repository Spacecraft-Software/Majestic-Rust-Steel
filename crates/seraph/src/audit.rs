// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The Seraph audit log — an append-only, hash-chained record of every agent side effect
//! (PRD #1 §5.2.4).
//!
//! Each [`AuditEntry`] carries a UTC timestamp, a description of the action, and a BLAKE3 hash that
//! covers the *previous* entry's hash. Because every hash depends on its predecessor, altering,
//! inserting, or removing any past entry breaks the chain — which [`AuditLog::verify`] detects. The
//! log persists as JSONL (one entry per line), so an M3 deployment can reconstruct every agent side
//! effect from disk (an M3 exit criterion). Timestamps are ISO-8601 UTC `Z` (Steelbore §12.5) by
//! virtue of `jiff::Timestamp`.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// One audited agent side effect: when it happened, what it was, and the chain hash linking it to the
/// entry before it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// When the action occurred (UTC; serializes as ISO-8601 `Z`).
    pub timestamp: Timestamp,
    /// A description of the agent side effect (e.g. `"edit auth.rs L44 approved"`).
    pub action: String,
    /// The chain hash: BLAKE3 over the previous entry's hash, this timestamp, and this action,
    /// rendered as lowercase hex.
    pub hash: String,
}

/// An append-only, hash-chained audit log of agent side effects. Forge-resistant by construction: any
/// edit to a past entry invalidates every hash after it, which [`Self::verify`] reports.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuditLog {
    entries: Vec<AuditEntry>,
}

impl AuditLog {
    /// Creates an empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `action` at `timestamp`, chaining it to the current last entry. The caller supplies the
    /// timestamp (from `Timestamp::now()` in production), which keeps the log deterministic to test.
    pub fn append(&mut self, timestamp: Timestamp, action: impl Into<String>) {
        let action = action.into();
        let previous = self.entries.last().map_or("", |entry| entry.hash.as_str());
        let hash = chain_hash(previous, timestamp, &action);
        self.entries.push(AuditEntry {
            timestamp,
            action,
            hash,
        });
    }

    /// The entries, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[AuditEntry] {
        &self.entries
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the hash chain is intact — every entry's hash recomputes from its predecessor. `false`
    /// means an entry was altered, inserted, or removed (the log was tampered with).
    #[must_use]
    pub fn verify(&self) -> bool {
        let mut previous = "";
        for entry in &self.entries {
            if entry.hash != chain_hash(previous, entry.timestamp, &entry.action) {
                return false;
            }
            previous = &entry.hash;
        }
        true
    }

    /// Serializes the log as JSONL — one JSON entry per line — for append-only persistence.
    ///
    /// # Errors
    /// Returns a [`serde_json::Error`] if an entry cannot be serialized (not expected for these types).
    pub fn to_jsonl(&self) -> Result<String, serde_json::Error> {
        let mut out = String::new();
        for entry in &self.entries {
            out.push_str(&serde_json::to_string(entry)?);
            out.push('\n');
        }
        Ok(out)
    }

    /// Parses a JSONL log (the inverse of [`Self::to_jsonl`]); blank lines are skipped. The hash chain
    /// is *not* checked here — call [`Self::verify`] on the result.
    ///
    /// # Errors
    /// Returns a [`serde_json::Error`] if any non-blank line is not a valid [`AuditEntry`].
    pub fn from_jsonl(text: &str) -> Result<Self, serde_json::Error> {
        let entries = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { entries })
    }
}

/// The chain hash for one entry: BLAKE3 over the previous entry's hash, the timestamp (ISO-8601 `Z`),
/// and the action, as lowercase hex. Length-prefix-free fields are safe here because each is appended
/// with a fixed-format separator implicitly: the timestamp is a fixed-shape `Z` string.
fn chain_hash(previous: &str, timestamp: Timestamp, action: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(previous.as_bytes());
    hasher.update(b"\0"); // separator so `prev || ts` can't be confused with a shifted boundary
    hasher.update(timestamp.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(action.as_bytes());
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::AuditLog;
    use jiff::Timestamp;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("valid timestamp")
    }

    #[test]
    fn append_chains_and_verifies() {
        let mut log = AuditLog::new();
        assert!(log.is_empty());
        log.append(ts("2026-06-24T10:00:00Z"), "edit a.rs L1 approved");
        log.append(ts("2026-06-24T10:01:00Z"), "shell `cargo test` approved");
        assert_eq!(log.len(), 2);
        assert!(log.verify());
        // The second entry's hash depends on the first (the chain links).
        assert_ne!(log.entries()[0].hash, log.entries()[1].hash);
    }

    #[test]
    fn tampering_breaks_the_chain() {
        let mut log = AuditLog::new();
        log.append(ts("2026-06-24T10:00:00Z"), "edit a.rs approved");
        log.append(ts("2026-06-24T10:01:00Z"), "edit b.rs approved");
        assert!(log.verify());

        // Forge the first entry's action without recomputing the chain.
        let mut forged = log.clone();
        forged.entries[0].action = "edit a.rs DENIED-becomes-approved".to_owned();
        assert!(
            !forged.verify(),
            "altering a past entry must break verification"
        );
    }

    #[test]
    fn jsonl_round_trips_and_uses_utc_z() {
        let mut log = AuditLog::new();
        log.append(ts("2026-06-24T10:00:00Z"), "edit a.rs approved");
        log.append(
            ts("2026-06-24T10:05:30Z"),
            "delete b.rs rejected (stale tag)",
        );

        let jsonl = log.to_jsonl().expect("serialize");
        assert_eq!(jsonl.lines().count(), 2);
        assert!(
            jsonl.contains("2026-06-24T10:00:00Z"),
            "timestamps are ISO-8601 Z: {jsonl}"
        );

        let parsed = AuditLog::from_jsonl(&jsonl).expect("parse");
        assert_eq!(parsed.entries(), log.entries());
        assert!(parsed.verify());
    }

    #[test]
    fn a_removed_entry_breaks_the_chain() {
        let mut log = AuditLog::new();
        log.append(ts("2026-06-24T10:00:00Z"), "one");
        log.append(ts("2026-06-24T10:01:00Z"), "two");
        log.append(ts("2026-06-24T10:02:00Z"), "three");
        // Drop the middle entry: the third's hash no longer chains from the first.
        let mut tampered = log.clone();
        tampered.entries.remove(1);
        assert!(!tampered.verify());
    }
}

//! Participant-local durable journal: one small JSON file per txid, replaced
//! atomically (tmp + fsync + rename + fsync dir). The entry for a txid is the
//! participant's "prepared promise": it is durable only when this file and
//! the staged data are both fsynced.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use txp_core::TxId;

/// A participant's local view of one transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalState {
    /// Work is staged but nothing has been promised.
    Staged,
    /// Staged work is durable; a `Prepared` vote was (or can be) given.
    Prepared,
    /// Local commit record written (one-phase path or mid-commit).
    Committed,
    /// Locally aborted.
    Aborted,
}

/// The JSON document stored per transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalEntry<T> {
    /// Lifecycle state.
    pub state: LocalState,
    /// Adapter-specific payload (e.g. the redo list).
    pub data: T,
}

/// A directory of per-transaction JSON entries with atomic replace.
#[derive(Clone, Debug)]
pub struct Journal {
    dir: PathBuf,
}

impl Journal {
    /// Open (creating if needed) the journal directory.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Journal> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Journal { dir })
    }

    /// The journal directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, txid: TxId) -> PathBuf {
        self.dir.join(format!("{txid}.json"))
    }

    /// Read the entry for `txid`, or `None` if there is none.
    pub fn get<T: DeserializeOwned>(&self, txid: TxId) -> io::Result<Option<JournalEntry<T>>> {
        match std::fs::read(self.path(txid)) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b).map_err(io::Error::other)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Read only the state of `txid`'s entry.
    pub fn state(&self, txid: TxId) -> io::Result<Option<LocalState>> {
        Ok(self.get::<serde_json::Value>(txid)?.map(|e| e.state))
    }

    /// Durably set the entry.
    pub fn set<T: Serialize>(&self, txid: TxId, state: LocalState, data: &T) -> io::Result<()> {
        use std::io::Write;
        let entry = JournalEntry { state, data };
        let bytes = serde_json::to_vec(&entry).map_err(io::Error::other)?;
        let final_path = self.path(txid);
        let tmp = self.dir.join(format!(".{txid}.tmp"));
        {
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &final_path)?;
        std::fs::File::open(&self.dir)?.sync_all()
    }

    /// Durably delete the entry. Idempotent: a missing entry is not an error.
    pub fn remove(&self, txid: TxId) -> io::Result<()> {
        match std::fs::remove_file(self.path(txid)) {
            Ok(()) => std::fs::File::open(&self.dir)?.sync_all(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// All entries, sorted by transaction id.
    pub fn list(&self) -> io::Result<Vec<(TxId, LocalState)>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(&self.dir)? {
            let e = e?;
            let name = e.file_name();
            let name = name.to_string_lossy();
            if let Some(hex) = name.strip_suffix(".json")
                && let Some(txid) = TxId::parse(hex)
                    && let Some(st) = self.state(txid)? {
                        out.push((txid, st));
                    }
        }
        out.sort_by_key(|(t, _)| *t);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_list_remove() {
        let d = tempfile::tempdir().unwrap();
        let j = Journal::open(d.path()).unwrap();
        j.set(TxId(5), LocalState::Prepared, &vec!["a", "b"]).unwrap();
        let e: JournalEntry<Vec<String>> = j.get(TxId(5)).unwrap().unwrap();
        assert_eq!(e.state, LocalState::Prepared);
        assert_eq!(e.data, vec!["a", "b"]);
        assert_eq!(j.list().unwrap(), vec![(TxId(5), LocalState::Prepared)]);
        j.remove(TxId(5)).unwrap();
        j.remove(TxId(5)).unwrap();
        assert!(j.list().unwrap().is_empty());
    }
}

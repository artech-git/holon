//! The replayable transaction table and its recovery rules.
//!
//! `TxnTable::apply` is a pure function of (table, lsn, record). It is what
//! recovery replays, what checkpoints snapshot, and what the live engine keeps
//! in sync as it writes records.

use crate::{Decision, LogRecord, Lsn, ParticipantId, ParticipantSpec, TxId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Persisted phase of a transaction as derived from the log alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TxPhase {
    /// `Begin` seen, no decision. Under presumed abort this *is* aborted if we
    /// crash here.
    Started,
    /// `Commit` seen, `Done` not yet seen: phase two must be (re-)driven.
    Committing,
    /// `Abort` seen, `Done` not yet seen.
    Aborting,
    /// `Done` seen; nothing left to do. Entries in this phase are dropped from
    /// the table and only kept transiently for status queries.
    Done,
}

/// One transaction as reconstructed from the log alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxnEntry {
    /// Transaction id.
    pub txid: TxId,
    /// Name from the `Begin` record.
    pub name: String,
    /// Phase implied by the last record applied.
    pub phase: TxPhase,
    /// Participant specs from the `Begin` record.
    pub participants: Vec<ParticipantSpec>,
    /// Participants named in the Commit record (may be a subset: read-only
    /// voters drop out of phase two).
    pub commit_set: Vec<ParticipantId>,
    /// LSN of the `Begin` record; the oldest position this entry pins.
    pub begin_lsn: Lsn,
    /// LSN of the most recent record applied to this entry.
    pub last_lsn: Lsn,
    /// Reason given in a `ForceResolve` record, if an operator intervened.
    pub forced: Option<String>,
}

/// What recovery must do for one transaction (design §1.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    /// No record / only Begin: tell every participant `abort` (idempotent),
    /// then write Abort+Done lazily.
    AbortStarted {
        /// Transaction id.
        txid: TxId,
        /// Everyone from the `Begin` record.
        participants: Vec<ParticipantSpec>,
    },
    /// Commit without Done: re-drive commit to the commit set until all ack.
    RedriveCommit {
        /// Transaction id.
        txid: TxId,
        /// Everyone from the `Begin` record (needed to rebuild adapters).
        participants: Vec<ParticipantSpec>,
        /// Subset that must be told to commit.
        commit_set: Vec<ParticipantId>,
    },
    /// Abort without Done: re-drive abort.
    RedriveAbort {
        /// Transaction id.
        txid: TxId,
        /// Everyone from the `Begin` record.
        participants: Vec<ParticipantSpec>,
    },
}

impl RecoveryAction {
    /// The transaction the action applies to.
    pub fn txid(&self) -> TxId {
        match self {
            RecoveryAction::AbortStarted { txid, .. }
            | RecoveryAction::RedriveCommit { txid, .. }
            | RecoveryAction::RedriveAbort { txid, .. } => *txid,
        }
    }
}

/// Serializable checkpoint of a [`TxnTable`]: the applied LSN and every
/// entry that is not yet `Done`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TxnTableSnapshot {
    /// LSN through which the entries are current.
    pub lsn: Lsn,
    /// In-flight entries at `lsn`, in `TxId` order.
    pub entries: Vec<TxnEntry>,
}

/// In-memory transaction table. Deterministic: `BTreeMap` so iteration order
/// is stable across replays (important for trace validation).
#[derive(Clone, Debug, Default)]
pub struct TxnTable {
    entries: BTreeMap<TxId, TxnEntry>,
    applied_lsn: Lsn,
}

/// Why [`TxnTable::apply`] refused a record.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApplyError {
    /// A non-`Begin` record arrived for a transaction with no `Begin`.
    #[error("record for unknown transaction {0} (no Begin seen)")]
    UnknownTxn(TxId),
    /// The record is not allowed in the entry's current phase.
    #[error("illegal transition for {txid}: {from:?} on {record}")]
    IllegalTransition {
        /// Transaction id.
        txid: TxId,
        /// Phase the entry was in.
        from: TxPhase,
        /// Kind of the offending record.
        record: &'static str,
    },
    /// `lsn` is not greater than the last applied LSN.
    #[error("non-monotonic lsn {lsn} after {applied}")]
    NonMonotonicLsn {
        /// LSN of the rejected record.
        lsn: Lsn,
        /// LSN already applied.
        applied: Lsn,
    },
    /// An operator tried to force-abort a transaction whose `Commit` is
    /// already durable (invariant I2).
    #[error("force_resolve(abort) on {0} contradicts a durable Commit")]
    ContradictsCommit(TxId),
}

impl TxnTable {
    /// An empty table at LSN zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a table from a checkpoint; records after `s.lsn` must then
    /// be replayed with [`TxnTable::apply`].
    pub fn from_snapshot(s: TxnTableSnapshot) -> Self {
        let mut t = TxnTable { entries: BTreeMap::new(), applied_lsn: s.lsn };
        for e in s.entries {
            t.entries.insert(e.txid, e);
        }
        t
    }

    /// Checkpoint the table. `Done` entries are omitted.
    pub fn snapshot(&self) -> TxnTableSnapshot {
        TxnTableSnapshot {
            lsn: self.applied_lsn,
            entries: self.entries.values().filter(|e| e.phase != TxPhase::Done).cloned().collect(),
        }
    }

    /// LSN of the last record applied.
    pub fn applied_lsn(&self) -> Lsn {
        self.applied_lsn
    }

    /// Look up one transaction.
    pub fn get(&self, txid: TxId) -> Option<&TxnEntry> {
        self.entries.get(&txid)
    }

    /// All entries, including transiently kept `Done` ones, in `TxId` order.
    pub fn entries(&self) -> impl Iterator<Item = &TxnEntry> {
        self.entries.values()
    }

    /// Lowest LSN still referenced by a not-Done transaction. Segments whose
    /// max LSN is below `min(checkpoint_lsn, this)` can be deleted.
    pub fn oldest_in_flight_lsn(&self) -> Option<Lsn> {
        self.entries.values().filter(|e| e.phase != TxPhase::Done).map(|e| e.begin_lsn).min()
    }

    /// Drop Done entries (used after status readers are satisfied).
    pub fn gc_done(&mut self) {
        self.entries.retain(|_, e| e.phase != TxPhase::Done);
    }

    /// Apply one log record. Tolerant of replays of records that recovery may
    /// legitimately see twice (e.g. a duplicated `Done`), strict on records
    /// that would violate the protocol (e.g. `Abort` after `Commit`, I2).
    pub fn apply(&mut self, lsn: Lsn, rec: &LogRecord) -> Result<(), ApplyError> {
        if lsn <= self.applied_lsn && self.applied_lsn != Lsn::ZERO {
            return Err(ApplyError::NonMonotonicLsn { lsn, applied: self.applied_lsn });
        }
        let txid = rec.txid();
        match rec {
            LogRecord::Begin { name, participants, .. } => {
                self.entries.insert(
                    txid,
                    TxnEntry {
                        txid,
                        name: name.clone(),
                        phase: TxPhase::Started,
                        participants: participants.clone(),
                        commit_set: Vec::new(),
                        begin_lsn: lsn,
                        last_lsn: lsn,
                        forced: None,
                    },
                );
            }
            LogRecord::Prepared { .. } => {
                let e = self.entries.get_mut(&txid).ok_or(ApplyError::UnknownTxn(txid))?;
                e.last_lsn = lsn;
            }
            LogRecord::Commit { participants, .. } => {
                let e = self.entries.get_mut(&txid).ok_or(ApplyError::UnknownTxn(txid))?;
                match e.phase {
                    TxPhase::Started => {
                        e.phase = TxPhase::Committing;
                        e.commit_set = participants.clone();
                        e.last_lsn = lsn;
                    }
                    TxPhase::Committing => {
                        // Idempotent replay of the same decision.
                        e.last_lsn = lsn;
                    }
                    from => {
                        return Err(ApplyError::IllegalTransition { txid, from, record: "commit" });
                    }
                }
            }
            LogRecord::Abort { .. } => {
                let e = self.entries.get_mut(&txid).ok_or(ApplyError::UnknownTxn(txid))?;
                match e.phase {
                    TxPhase::Started | TxPhase::Aborting => {
                        e.phase = TxPhase::Aborting;
                        e.last_lsn = lsn;
                    }
                    from => {
                        return Err(ApplyError::IllegalTransition { txid, from, record: "abort" });
                    }
                }
            }
            LogRecord::Done { .. } => {
                let e = self.entries.get_mut(&txid).ok_or(ApplyError::UnknownTxn(txid))?;
                match e.phase {
                    TxPhase::Committing | TxPhase::Aborting | TxPhase::Done => {
                        e.phase = TxPhase::Done;
                        e.last_lsn = lsn;
                    }
                    // Done after a bare Begin is legal: a transaction that
                    // aborted before any participant staged anything.
                    TxPhase::Started => {
                        e.phase = TxPhase::Done;
                        e.last_lsn = lsn;
                    }
                }
            }
            LogRecord::ForceResolve { decision, reason, .. } => {
                let e = self.entries.get_mut(&txid).ok_or(ApplyError::UnknownTxn(txid))?;
                match (e.phase, decision) {
                    (TxPhase::Committing, Decision::Abort) => {
                        return Err(ApplyError::ContradictsCommit(txid));
                    }
                    (TxPhase::Started | TxPhase::Aborting, Decision::Abort) => e.phase = TxPhase::Aborting,
                    (TxPhase::Started, Decision::Commit) => {
                        e.phase = TxPhase::Committing;
                        e.commit_set = e.participants.iter().map(|p| p.id.clone()).collect();
                    }
                    (TxPhase::Committing, Decision::Commit) => {}
                    (TxPhase::Aborting, Decision::Commit) => {
                        return Err(ApplyError::IllegalTransition { txid, from: e.phase, record: "force_resolve" });
                    }
                    (TxPhase::Done, _) => {}
                }
                e.forced = Some(reason.clone());
                e.last_lsn = lsn;
            }
        }
        self.applied_lsn = lsn;
        Ok(())
    }

    /// Recovery rules keyed by the last record seen per transaction (§1.4).
    pub fn recovery_actions(&self) -> Vec<RecoveryAction> {
        self.entries
            .values()
            .filter_map(|e| match e.phase {
                TxPhase::Started => Some(RecoveryAction::AbortStarted {
                    txid: e.txid,
                    participants: e.participants.clone(),
                }),
                TxPhase::Committing => Some(RecoveryAction::RedriveCommit {
                    txid: e.txid,
                    participants: e.participants.clone(),
                    commit_set: e.commit_set.clone(),
                }),
                TxPhase::Aborting => Some(RecoveryAction::RedriveAbort {
                    txid: e.txid,
                    participants: e.participants.clone(),
                }),
                TxPhase::Done => None,
            })
            .collect()
    }

    /// Answer a participant's `resolve(txid)` query: committed iff a Commit
    /// record is durable; otherwise presumed aborted (I4).
    pub fn resolve(&self, txid: TxId) -> Decision {
        match self.entries.get(&txid).map(|e| e.phase) {
            Some(TxPhase::Committing) => Decision::Commit,
            // Done after Commit is also committed; we can't distinguish from
            // the phase alone, so Done entries keep commit_set non-empty iff
            // they committed.
            Some(TxPhase::Done) => {
                if self.entries[&txid].commit_set.is_empty() { Decision::Abort } else { Decision::Commit }
            }
            _ => Decision::Abort,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str) -> ParticipantSpec {
        ParticipantSpec { id: ParticipantId::new(id), kind: "test".into(), config: serde_json::Value::Null }
    }
    fn begin(t: u128) -> LogRecord {
        LogRecord::Begin { txid: TxId(t), name: "n".into(), manifest_digest: "d".into(), submitter: None, participants: vec![spec("a"), spec("b")] }
    }

    #[test]
    fn presumed_abort_for_begin_only() {
        let mut t = TxnTable::new();
        t.apply(Lsn(1), &begin(1)).unwrap();
        assert_eq!(t.recovery_actions().len(), 1);
        assert!(matches!(t.recovery_actions()[0], RecoveryAction::AbortStarted { .. }));
        assert_eq!(t.resolve(TxId(1)), Decision::Abort);
        assert_eq!(t.resolve(TxId(99)), Decision::Abort);
    }

    #[test]
    fn commit_then_done() {
        let mut t = TxnTable::new();
        t.apply(Lsn(1), &begin(1)).unwrap();
        t.apply(Lsn(2), &LogRecord::Commit { txid: TxId(1), participants: vec![ParticipantId::new("a")] }).unwrap();
        assert!(matches!(t.recovery_actions()[0], RecoveryAction::RedriveCommit { .. }));
        assert_eq!(t.resolve(TxId(1)), Decision::Commit);
        t.apply(Lsn(3), &LogRecord::Done { txid: TxId(1) }).unwrap();
        assert!(t.recovery_actions().is_empty());
        assert_eq!(t.resolve(TxId(1)), Decision::Commit);
        assert!(t.snapshot().entries.is_empty());
    }

    #[test]
    fn abort_after_commit_is_rejected() {
        let mut t = TxnTable::new();
        t.apply(Lsn(1), &begin(1)).unwrap();
        t.apply(Lsn(2), &LogRecord::Commit { txid: TxId(1), participants: vec![] }).unwrap();
        let e = t.apply(Lsn(3), &LogRecord::Abort { txid: TxId(1) }).unwrap_err();
        assert!(matches!(e, ApplyError::IllegalTransition { .. }));
        let e = t
            .apply(Lsn(3), &LogRecord::ForceResolve { txid: TxId(1), decision: Decision::Abort, reason: "x".into() })
            .unwrap_err();
        assert_eq!(e, ApplyError::ContradictsCommit(TxId(1)));
    }

    fn force(t: u128, decision: Decision) -> LogRecord {
        LogRecord::ForceResolve { txid: TxId(t), decision, reason: "op".into() }
    }

    #[test]
    fn lsn_must_increase_after_the_first_record() {
        let mut t = TxnTable::new();
        assert_eq!(t.applied_lsn(), Lsn::ZERO);
        t.apply(Lsn(5), &begin(1)).unwrap();
        assert_eq!(t.applied_lsn(), Lsn(5));
        let e = t.apply(Lsn(5), &begin(2)).unwrap_err();
        assert_eq!(e, ApplyError::NonMonotonicLsn { lsn: Lsn(5), applied: Lsn(5) });
        assert_eq!(e.to_string(), "non-monotonic lsn 5 after 5");
    }

    #[test]
    fn records_for_unknown_transactions_are_rejected() {
        let recs = [
            LogRecord::Prepared { txid: TxId(9), votes: vec![] },
            LogRecord::Commit { txid: TxId(9), participants: vec![] },
            LogRecord::Abort { txid: TxId(9) },
            LogRecord::Done { txid: TxId(9) },
            force(9, Decision::Commit),
        ];
        for (i, r) in recs.iter().enumerate() {
            let mut t = TxnTable::new();
            assert_eq!(t.apply(Lsn(i as u64 + 1), r), Err(ApplyError::UnknownTxn(TxId(9))), "{r:?}");
        }
    }

    #[test]
    fn replays_are_idempotent_and_commit_after_abort_is_illegal() {
        let mut t = TxnTable::new();
        t.apply(Lsn(1), &begin(1)).unwrap();
        t.apply(Lsn(2), &LogRecord::Prepared { txid: TxId(1), votes: vec![] }).unwrap();
        assert_eq!(t.get(TxId(1)).unwrap().last_lsn, Lsn(2));
        let commit = LogRecord::Commit { txid: TxId(1), participants: vec![ParticipantId::new("a")] };
        t.apply(Lsn(3), &commit).unwrap();
        t.apply(Lsn(4), &commit).unwrap();
        let e = t.get(TxId(1)).unwrap();
        assert_eq!((e.phase, e.last_lsn, e.commit_set.len()), (TxPhase::Committing, Lsn(4), 1));

        t.apply(Lsn(5), &begin(2)).unwrap();
        t.apply(Lsn(6), &LogRecord::Abort { txid: TxId(2) }).unwrap();
        t.apply(Lsn(7), &LogRecord::Abort { txid: TxId(2) }).unwrap();
        assert_eq!(t.get(TxId(2)).unwrap().phase, TxPhase::Aborting);
        let e = t.apply(Lsn(8), &LogRecord::Commit { txid: TxId(2), participants: vec![] }).unwrap_err();
        assert_eq!(e, ApplyError::IllegalTransition { txid: TxId(2), from: TxPhase::Aborting, record: "commit" });
        assert!(e.to_string().starts_with("illegal transition for"), "{e}");
        assert!(matches!(t.recovery_actions()[1], RecoveryAction::RedriveAbort { txid: TxId(2), .. }));
        assert_eq!(t.recovery_actions().iter().map(|a| a.txid()).collect::<Vec<_>>(), vec![TxId(1), TxId(2)]);

        // Done twice is fine; it leaves an aborted transaction resolvable.
        t.apply(Lsn(9), &LogRecord::Done { txid: TxId(2) }).unwrap();
        t.apply(Lsn(10), &LogRecord::Done { txid: TxId(2) }).unwrap();
        assert_eq!(t.resolve(TxId(2)), Decision::Abort);
    }

    #[test]
    fn force_resolve_follows_the_decision_rules() {
        let mut t = TxnTable::new();
        for i in 1..=4 {
            t.apply(Lsn(i as u64), &begin(i)).unwrap();
        }
        // Started + abort, and again once Aborting.
        t.apply(Lsn(5), &force(1, Decision::Abort)).unwrap();
        t.apply(Lsn(6), &force(1, Decision::Abort)).unwrap();
        let e = t.get(TxId(1)).unwrap();
        assert_eq!((e.phase, e.forced.as_deref()), (TxPhase::Aborting, Some("op")));
        // Aborting + commit contradicts the earlier decision.
        let err = t.apply(Lsn(7), &force(1, Decision::Commit)).unwrap_err();
        assert_eq!(err, ApplyError::IllegalTransition { txid: TxId(1), from: TxPhase::Aborting, record: "force_resolve" });
        // Started + commit commits everyone from Begin; a second commit is a no-op.
        t.apply(Lsn(8), &force(2, Decision::Commit)).unwrap();
        t.apply(Lsn(9), &force(2, Decision::Commit)).unwrap();
        let e = t.get(TxId(2)).unwrap();
        assert_eq!(e.phase, TxPhase::Committing);
        assert_eq!(e.commit_set, vec![ParticipantId::new("a"), ParticipantId::new("b")]);
        assert_eq!(t.resolve(TxId(2)), Decision::Commit);
        // Done stays Done whatever the operator says, but the reason is kept.
        t.apply(Lsn(10), &LogRecord::Done { txid: TxId(3) }).unwrap();
        t.apply(Lsn(11), &force(3, Decision::Abort)).unwrap();
        t.apply(Lsn(12), &force(3, Decision::Commit)).unwrap();
        let e = t.get(TxId(3)).unwrap();
        assert_eq!((e.phase, e.forced.as_deref(), e.last_lsn), (TxPhase::Done, Some("op"), Lsn(12)));
        assert_eq!(ApplyError::ContradictsCommit(TxId(2)).to_string(), format!("force_resolve(abort) on {} contradicts a durable Commit", TxId(2)));
        assert_eq!(ApplyError::UnknownTxn(TxId(2)).to_string(), format!("record for unknown transaction {} (no Begin seen)", TxId(2)));
    }

    #[test]
    fn oldest_in_flight_and_gc_ignore_done_entries() {
        let mut t = TxnTable::new();
        assert_eq!(t.oldest_in_flight_lsn(), None);
        t.apply(Lsn(1), &begin(1)).unwrap();
        t.apply(Lsn(2), &begin(2)).unwrap();
        assert_eq!(t.oldest_in_flight_lsn(), Some(Lsn(1)));
        t.apply(Lsn(3), &LogRecord::Done { txid: TxId(1) }).unwrap();
        assert_eq!(t.oldest_in_flight_lsn(), Some(Lsn(2)));
        assert_eq!(t.entries().count(), 2);
        t.gc_done();
        assert_eq!(t.entries().map(|e| e.txid).collect::<Vec<_>>(), vec![TxId(2)]);
        assert!(t.get(TxId(1)).is_none());
    }

    #[test]
    fn snapshot_roundtrip_equals_replay() {
        let mut t = TxnTable::new();
        t.apply(Lsn(1), &begin(1)).unwrap();
        t.apply(Lsn(2), &begin(2)).unwrap();
        t.apply(Lsn(3), &LogRecord::Abort { txid: TxId(2) }).unwrap();
        let snap = t.snapshot();
        let t2 = TxnTable::from_snapshot(snap.clone());
        assert_eq!(t2.snapshot(), snap);
        assert_eq!(t2.recovery_actions(), t.recovery_actions());
    }
}

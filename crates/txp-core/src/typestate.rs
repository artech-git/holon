//! Typestate wrappers for the live driver (design §1.4).
//!
//! The persisted representation is [`crate::TxPhase`]; these wrappers exist
//! so that illegal transitions in the engine do not compile, and so that the
//! only way into phase two is a [`DurableCommit`] token minted by the log
//! writer after `fdatasync` returned `Ok`.

use crate::{Lsn, TxId};
use std::marker::PhantomData;

/// Unforgeable proof that `Commit{txid}` is durable at `lsn`.
///
/// Constructed only by a log implementation via [`DurableCommit::mint`], which
/// is `#[doc(hidden)]` and must only be called after the forced write
/// succeeded. In Phase 4 "durable" means Raft-committed at index `lsn`.
#[derive(Debug)]
pub struct DurableCommit {
    txid: TxId,
    lsn: Lsn,
    _private: (),
}

impl DurableCommit {
    #[doc(hidden)]
    pub fn mint(txid: TxId, lsn: Lsn) -> DurableCommit {
        DurableCommit { txid, lsn, _private: () }
    }
    /// The transaction the proof is for.
    pub fn txid(&self) -> TxId {
        self.txid
    }
    /// Log position at which the `Commit` record became durable.
    pub fn lsn(&self) -> Lsn {
        self.lsn
    }
}

/// Phase marker: locks held and `Begin` logged; steps are being staged.
pub struct Started;
/// Phase marker: votes are being collected.
pub struct Preparing;
/// Phase marker: `Commit` is durable; phase two is in progress.
pub struct Committing;
/// Phase marker: abort decided; participants are being told to discard.
pub struct Aborting;

/// Phase-indexed handle. `P` is a generic payload (the engine's per-txn
/// context) that rides along unchanged through transitions.
pub struct Txn<S, P> {
    id: TxId,
    /// Caller-owned context carried unchanged through every transition.
    pub payload: P,
    _s: PhantomData<S>,
}

impl<S, P> Txn<S, P> {
    /// Transaction id.
    pub fn id(&self) -> TxId {
        self.id
    }
    fn cast<T>(self) -> Txn<T, P> {
        Txn { id: self.id, payload: self.payload, _s: PhantomData }
    }
}

impl<P> Txn<Started, P> {
    /// Start a transaction in the `Started` phase.
    pub fn new(id: TxId, payload: P) -> Self {
        Txn { id, payload, _s: PhantomData }
    }
    /// Staging finished; move on to collecting votes.
    pub fn begin_prepare(self) -> Txn<Preparing, P> {
        self.cast()
    }
    /// Abort before any vote was requested.
    pub fn abort(self) -> Txn<Aborting, P> {
        self.cast()
    }
}

impl<P> Txn<Preparing, P> {
    /// Only way to obtain `Txn<Committing>`: proof that the Commit record is durable.
    pub fn decide_commit(self, proof: &DurableCommit) -> Txn<Committing, P> {
        assert_eq!(proof.txid(), self.id, "DurableCommit token for a different transaction");
        self.cast()
    }
    /// A participant voted no, failed, or timed out: abort.
    pub fn decide_abort(self) -> Txn<Aborting, P> {
        self.cast()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_compile_and_check_token() {
        let t = Txn::<Started, ()>::new(TxId(7), ());
        let t = t.begin_prepare();
        let proof = DurableCommit::mint(TxId(7), Lsn(3));
        let c = t.decide_commit(&proof);
        assert_eq!(c.id(), TxId(7));
    }

    #[test]
    fn token_carries_its_lsn_and_both_abort_paths_keep_the_payload() {
        let proof = DurableCommit::mint(TxId(7), Lsn(3));
        assert_eq!((proof.txid(), proof.lsn()), (TxId(7), Lsn(3)));
        let a = Txn::<Started, &str>::new(TxId(1), "ctx").abort();
        assert_eq!((a.id(), a.payload), (TxId(1), "ctx"));
        let b = Txn::<Started, &str>::new(TxId(2), "ctx").begin_prepare().decide_abort();
        assert_eq!((b.id(), b.payload), (TxId(2), "ctx"));
    }

    #[test]
    #[should_panic]
    fn wrong_token_panics() {
        let t = Txn::<Started, ()>::new(TxId(7), ()).begin_prepare();
        let proof = DurableCommit::mint(TxId(8), Lsn(3));
        let _ = t.decide_commit(&proof);
    }
}

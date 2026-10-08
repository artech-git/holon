//! txp-lock: strict two-phase locking over declared logical resources
//! (design §2.1, MVP subset).
//!
//! - Keys are hierarchical (`fs:/srv/site/img/a.png` conflicts with
//!   `fs:/srv/site`), compared component-wise.
//! - Batch transactions acquire all locks up front in canonical key order
//!   ([`LockManager::acquire_all`]), which makes deadlock impossible by
//!   construction. Wound-wait for interactive transactions is deferred.
//! - Waiters are oneshot channels; there is no polling.
//! - Lock state is not persisted. On restart the engine re-acquires locks for
//!   prepared/in-doubt transactions before admission reopens.

#![warn(missing_docs)]

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::oneshot;
use txp_core::TxId;

/// Lock mode. Shared locks are compatible with each other; an exclusive
/// lock conflicts with everything on a related key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Mode {
    /// Read lock.
    Shared,
    /// Write lock.
    Exclusive,
}

/// Hierarchical resource name such as `fs:/srv/site`. Trailing slashes
/// are stripped on construction.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResourceKey(pub String);

impl ResourceKey {
    /// Normalize `s` (strip trailing `/`) into a key.
    pub fn new(s: impl Into<String>) -> Self {
        let mut s: String = s.into();
        while s.ends_with('/') && s.len() > 1 {
            s.pop();
        }
        ResourceKey(s)
    }
    fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }
    /// True if `self` is `other`, an ancestor of it, or a descendant of it.
    pub fn related(&self, other: &ResourceKey) -> bool {
        let a: Vec<&str> = self.components().collect();
        let b: Vec<&str> = other.components().collect();
        let n = a.len().min(b.len());
        a[..n] == b[..n]
    }
}

fn conflicts(a_mode: Mode, a_key: &ResourceKey, b_mode: Mode, b_key: &ResourceKey) -> bool {
    (a_mode == Mode::Exclusive || b_mode == Mode::Exclusive) && a_key.related(b_key)
}

/// A granted lock.
#[derive(Clone, Debug, Serialize)]
pub struct Held {
    /// Holder.
    pub txid: TxId,
    /// Locked key.
    pub key: ResourceKey,
    /// Granted mode.
    pub mode: Mode,
}

struct Waiter {
    txid: TxId,
    key: ResourceKey,
    mode: Mode,
    tx: oneshot::Sender<()>,
}

#[derive(Default)]
struct State {
    held: Vec<Held>,
    waiters: VecDeque<Waiter>,
}

impl State {
    fn grantable(&self, txid: TxId, key: &ResourceKey, mode: Mode, before_waiter_idx: usize) -> bool {
        if self.held.iter().any(|h| h.txid != txid && conflicts(h.mode, &h.key, mode, key)) {
            return false;
        }
        // FIFO fairness: do not overtake an earlier conflicting waiter.
        self.waiters
            .iter()
            .take(before_waiter_idx)
            .all(|w| w.txid == txid || !conflicts(w.mode, &w.key, mode, key))
    }
}

/// Cheaply cloneable handle to one shared lock table.
#[derive(Clone, Default)]
pub struct LockManager {
    st: Arc<Mutex<State>>,
}

/// One edge of the wait-for graph.
#[derive(Clone, Debug, Serialize)]
pub struct WaitEdge {
    /// Transaction that is blocked.
    pub waiter: TxId,
    /// Transaction holding a conflicting lock.
    pub holder: TxId,
    /// Key the waiter asked for.
    pub key: ResourceKey,
}

impl LockManager {
    /// An empty lock table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire one lock; resolves when granted. Idempotent for re-acquiring a
    /// key already held by `txid` (upgrades S→X are treated as a new request).
    pub async fn acquire(&self, txid: TxId, key: ResourceKey, mode: Mode) {
        let rx = {
            let mut s = self.st.lock();
            if let Some(h) = s.held.iter_mut().find(|h| h.txid == txid && h.key == key) {
                if h.mode == mode || h.mode == Mode::Exclusive {
                    return;
                }
                // Upgrade: drop the S hold and request X below.
                s.held.retain(|h| !(h.txid == txid && h.key == key));
            }
            let idx = s.waiters.len();
            if s.grantable(txid, &key, mode, idx) {
                s.held.push(Held { txid, key, mode });
                return;
            }
            let (tx, rx) = oneshot::channel();
            s.waiters.push_back(Waiter { txid, key, mode, tx });
            rx
        };
        // If the manager is dropped, treat as granted-by-shutdown; the engine
        // is going away anyway.
        let _ = rx.await;
    }

    /// Conservative acquisition in canonical order (deadlock-free by
    /// construction when every batch transaction uses it).
    pub async fn acquire_all(&self, txid: TxId, mut keys: Vec<(ResourceKey, Mode)>) {
        keys.sort();
        keys.dedup();
        for (k, m) in keys {
            self.acquire(txid, k, m).await;
        }
    }

    /// Release everything held by `txid` and grant whatever became possible.
    pub fn release_all(&self, txid: TxId) {
        let mut s = self.st.lock();
        s.held.retain(|h| h.txid != txid);
        s.waiters.retain(|w| w.txid != txid);
        self.grant_waiters(&mut s);
    }

    fn grant_waiters(&self, s: &mut State) {
        let mut i = 0;
        while i < s.waiters.len() {
            let w = &s.waiters[i];
            if s.grantable(w.txid, &w.key, w.mode, i) {
                let w = s.waiters.remove(i).unwrap();
                s.held.push(Held { txid: w.txid, key: w.key, mode: w.mode });
                let _ = w.tx.send(());
            } else {
                i += 1;
            }
        }
    }

    /// Snapshot of every granted lock.
    pub fn held(&self) -> Vec<Held> {
        self.st.lock().held.clone()
    }

    /// Observability: who waits on whom.
    pub fn wait_for_graph(&self) -> Vec<WaitEdge> {
        let s = self.st.lock();
        let mut out = Vec::new();
        for w in &s.waiters {
            for h in &s.held {
                if h.txid != w.txid && conflicts(h.mode, &h.key, w.mode, &w.key) {
                    out.push(WaitEdge { waiter: w.txid, holder: h.txid, key: w.key.clone() });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn hierarchy_conflicts() {
        let a = ResourceKey::new("fs:/srv/site");
        let b = ResourceKey::new("fs:/srv/site/img/a.png");
        let c = ResourceKey::new("fs:/srv/other");
        assert!(a.related(&b) && b.related(&a));
        assert!(!a.related(&c));
        assert!(conflicts(Mode::Exclusive, &a, Mode::Shared, &b));
        assert!(!conflicts(Mode::Shared, &a, Mode::Shared, &b));
    }

    #[tokio::test]
    async fn exclusive_waits_and_is_granted_on_release() {
        let lm = LockManager::new();
        let k = ResourceKey::new("r");
        lm.acquire(TxId(1), k.clone(), Mode::Exclusive).await;
        let lm2 = lm.clone();
        let k2 = k.clone();
        let h = tokio::spawn(async move { lm2.acquire(TxId(2), k2, Mode::Exclusive).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!h.is_finished());
        assert_eq!(lm.wait_for_graph().len(), 1);
        lm.release_all(TxId(1));
        tokio::time::timeout(Duration::from_secs(1), h).await.unwrap().unwrap();
        assert_eq!(lm.held().len(), 1);
    }

    #[tokio::test]
    async fn no_deadlock_with_canonical_order() {
        let lm = LockManager::new();
        let mut hs = Vec::new();
        for t in 0..50u128 {
            let lm = lm.clone();
            hs.push(tokio::spawn(async move {
                let keys: Vec<_> = (0..5)
                    .map(|i| (ResourceKey::new(format!("k{}", (t as usize * 7 + i * 3) % 9)), Mode::Exclusive))
                    .collect();
                lm.acquire_all(TxId(t), keys).await;
                tokio::task::yield_now().await;
                lm.release_all(TxId(t));
            }));
        }
        for h in hs {
            tokio::time::timeout(Duration::from_secs(5), h).await.unwrap().unwrap();
        }
        assert!(lm.held().is_empty());
    }
}

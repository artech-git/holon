//! Manifest → execution plan: participant specs, lock keys, step routing.

use std::collections::HashMap;
use std::path::PathBuf;
use txp_core::{ParticipantId, ParticipantSpec, TxId};
use txp_lock::{Mode, ResourceKey};
use txp_manifest::{parse_duration, Manifest};
use txp_participant::StepSpec;
use txp_proc::{MountSpec, ProcConfig, ProcParticipant, RunAs};

/// A step routed to the participant that executes it.
#[derive(Clone, Debug)]
pub struct PlanStep {
    /// Target participant (`fs:<resource>` or `proc:<step>`).
    pub participant: ParticipantId,
    /// What to stage.
    pub spec: StepSpec,
}

/// Everything the engine needs to run one manifest, derived once at submit.
#[derive(Clone, Debug)]
pub struct Plan {
    /// Manifest name.
    pub name: String,
    /// SHA-256 of the manifest text.
    pub digest: String,
    /// Whole-transaction timeout.
    pub timeout: std::time::Duration,
    /// One `fs` participant per writable resource plus one `proc` participant
    /// per process step; recorded in `Begin`.
    pub participants: Vec<ParticipantSpec>,
    /// Lock keys to acquire up front (exclusive for `write`, shared for `read`).
    pub locks: Vec<(ResourceKey, Mode)>,
    /// Steps in topological order.
    pub steps: Vec<PlanStep>,
}

/// Constrains the identity a submitter's process steps may run as.
#[derive(Clone, Copy, Debug)]
pub struct RunAsPolicy {
    /// Identity used when a step omits `user`.
    pub default: RunAs,
    /// When `Some`, every step must run as exactly this identity; a `user`
    /// requesting anything else is rejected. `None` lets the (root) submitter
    /// choose any uid/gid.
    pub pin: Option<RunAs>,
}

fn fs_pid(rid: &str) -> ParticipantId {
    ParticipantId::new(format!("fs:{rid}"))
}
fn proc_pid(sid: &str) -> ParticipantId {
    ParticipantId::new(format!("proc:{sid}"))
}

impl Plan {
    /// Derive the plan. Process steps that share a resource see the uppers of
    /// earlier steps as extra overlay layers, so later steps build on earlier
    /// ones within the same transaction.
    pub fn build(m: &Manifest, text: &str, txid: TxId, runas: &RunAsPolicy) -> Result<Plan, txp_manifest::ManifestError> {
        let mut participants: Vec<ParticipantSpec> = Vec::new();
        let mut locks = Vec::new();
        let mut root_of: HashMap<&str, PathBuf> = HashMap::new();
        for r in &m.resources {
            root_of.insert(&r.id, r.path.clone());
            let mode = if r.mode == "write" { Mode::Exclusive } else { Mode::Shared };
            locks.push((ResourceKey::new(format!("fs:{}", r.path.display())), mode));
            if r.mode == "write" {
                participants.push(ParticipantSpec {
                    id: fs_pid(&r.id),
                    kind: "fs".into(),
                    config: serde_json::json!({ "root": r.path }),
                });
            }
        }
        // Earlier process steps' uppers per resource, most recent first.
        let mut uppers: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let mut steps = Vec::new();
        for s in m.ordered_steps()? {
            match s.kind.as_str() {
                "process" => {
                    let mut mounts = Vec::new();
                    for mt in &s.mounts {
                        let root = root_of[mt.resource.as_str()].clone();
                        mounts.push(MountSpec {
                            resource: mt.resource.clone(),
                            root: root.clone(),
                            at: mt.at.clone(),
                            extra_lowers: uppers.get(&mt.resource).cloned().unwrap_or_default(),
                        });
                    }
                    for mt in &s.mounts {
                        let root = &root_of[mt.resource.as_str()];
                        uppers.entry(mt.resource.clone()).or_default().insert(0, ProcParticipant::upper_dir(root, txid, &s.id));
                    }
                    let timeout_secs = match &s.timeout {
                        Some(t) => parse_duration(t)?.as_secs().max(1),
                        None => 600,
                    };
                    let run_as = match s.user.as_deref() {
                        None => runas.default,
                        Some(u) => {
                            let requested = parse_user(u)?;
                            if let Some(pin) = runas.pin
                                && (requested.uid != pin.uid || requested.gid != pin.gid)
                            {
                                return Err(txp_manifest::ManifestError::Invalid(format!(
                                    "step {:?}: not permitted to run as {}:{}; you may only run steps as {}:{}",
                                    s.id, requested.uid, requested.gid, pin.uid, pin.gid
                                )));
                            }
                            requested
                        }
                    };
                    let cfg = ProcConfig {
                        step: s.id.clone(),
                        argv: s.argv.clone(),
                        cwd: s.cwd.clone(),
                        env: s.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                        mounts,
                        run_as,
                        timeout_secs,
                        landlock: true,
                        seccomp: true,
                        private_tmp: true,
                    };
                    let pid = proc_pid(&s.id);
                    participants.push(ParticipantSpec {
                        id: pid.clone(),
                        kind: "proc".into(),
                        config: serde_json::to_value(&cfg).unwrap(),
                    });
                    steps.push(PlanStep {
                        participant: pid,
                        spec: StepSpec { id: s.id.clone(), kind: "process".into(), config: serde_json::Value::Null },
                    });
                }
                _ => {
                    let rid = s.resource.clone().unwrap();
                    steps.push(PlanStep {
                        participant: fs_pid(&rid),
                        spec: StepSpec {
                            id: s.id.clone(),
                            kind: s.kind.clone(),
                            config: serde_json::json!({ "path": s.path, "content": s.content, "source": s.source }),
                        },
                    });
                }
            }
        }
        Ok(Plan { name: m.txn.name.clone(), digest: Manifest::digest(text), timeout: m.timeout()?, participants, locks, steps })
    }
}

fn parse_user(u: &str) -> Result<RunAs, txp_manifest::ManifestError> {
    let (a, b) = u.split_once(':').unwrap_or((u, u));
    let p = |s: &str| s.parse::<u32>().map_err(|_| txp_manifest::ManifestError::Invalid(format!("user must be numeric uid[:gid], got {u:?}")));
    Ok(RunAs { uid: p(a)?, gid: p(b)? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use txp_manifest::Manifest;

    const PROC: &str = "[txn]\nname = \"t\"\n[[resource]]\nid = \"r\"\nkind = \"fs.tree\"\npath = \"/srv/x\"\n[[step]]\nid = \"s\"\nkind = \"process\"\nargv = [\"true\"]\nmounts = [{ resource = \"r\" }]\n";

    fn manifest(user: Option<&str>) -> Manifest {
        let text = match user {
            Some(u) => format!("{PROC}user = \"{u}\"\n"),
            None => PROC.to_string(),
        };
        Manifest::parse(&text).unwrap()
    }

    fn proc_run_as(plan: &Plan) -> RunAs {
        let spec = plan.participants.iter().find(|p| p.kind == "proc").unwrap();
        serde_json::from_value::<ProcConfig>(spec.config.clone()).unwrap().run_as
    }

    #[test]
    fn non_root_submitter_defaults_to_its_own_identity() {
        let policy = RunAsPolicy { default: RunAs { uid: 1000, gid: 1000 }, pin: Some(RunAs { uid: 1000, gid: 1000 }) };
        let plan = Plan::build(&manifest(None), "txt", TxId(1), &policy).unwrap();
        assert_eq!(proc_run_as(&plan).uid, 1000);
    }

    #[test]
    fn non_root_submitter_cannot_request_another_uid() {
        let policy = RunAsPolicy { default: RunAs { uid: 1000, gid: 1000 }, pin: Some(RunAs { uid: 1000, gid: 1000 }) };
        let e = Plan::build(&manifest(Some("0:0")), "txt", TxId(1), &policy).unwrap_err();
        assert!(e.to_string().contains("not permitted to run as"), "{e}");
    }

    #[test]
    fn root_submitter_may_choose_any_uid() {
        let policy = RunAsPolicy { default: RunAs { uid: 65534, gid: 65534 }, pin: None };
        let plan = Plan::build(&manifest(Some("0:0")), "txt", TxId(1), &policy).unwrap();
        assert_eq!(proc_run_as(&plan).uid, 0);
    }
}

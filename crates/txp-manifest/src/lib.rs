//! txp-manifest (design §1.9, MVP subset).
//!
//! ```toml
//! [txn]
//! name = "rebuild-and-publish"
//! timeout = "10m"
//!
//! [[resource]]
//! id = "site"; kind = "fs.tree"; path = "/srv/site"; mode = "write"
//!
//! [[step]]
//! id = "build"; kind = "process"; argv = ["make", "site"]
//! mounts = [{ resource = "site" }]        # staged view at the real path
//! cwd = "/srv/site"; timeout = "5m"
//!
//! [[step]]
//! id = "stamp"; kind = "fs.put"; resource = "site"; after = ["build"]
//! path = "DEPLOY"; content = "$txid\n"
//! ```
//! Step kinds: `process`, `fs.put`, `fs.delete`, `fs.replace_tree`.
//! Effects (`http.deferred`) and Postgres are post-MVP and rejected here.

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

/// Why a manifest was rejected.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// Not valid TOML, or the shape does not match [`Manifest`].
    #[error("parse: {0}")]
    Parse(String),
    /// Well-formed but semantically wrong (unknown resource, cycle, ...).
    #[error("invalid manifest: {0}")]
    Invalid(String),
}

/// The `[txn]` table.
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct TxnSection {
    /// Required name, recorded in the log.
    pub name: String,
    /// Whole-transaction timeout such as `"10m"` (default 10 minutes).
    #[serde(default)]
    pub timeout: Option<String>,
    /// Reserved; accepted but unused in the MVP.
    #[serde(default)]
    pub isolation: Option<String>,
}

/// A `[[resource]]` entry: something the transaction locks and may modify.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Resource {
    /// Unique id referenced by steps.
    pub id: String,
    /// Resource kind; only `fs.tree` is supported.
    pub kind: String,
    /// Absolute path of the managed directory.
    pub path: PathBuf,
    /// `write` (default) takes an exclusive lock and gets a participant;
    /// `read` takes a shared lock only.
    #[serde(default = "default_mode")]
    pub mode: String,
}
fn default_mode() -> String {
    "write".into()
}

/// A resource exposed to a `process` step.
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct StepMount {
    /// Resource id.
    pub resource: String,
    /// Mount point inside the sandbox; defaults to the resource's real path.
    #[serde(default)]
    pub at: Option<PathBuf>,
}

/// A `[[step]]` entry. Which fields apply depends on `kind`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Step {
    /// Unique id; must not contain `/` or `..`.
    pub id: String,
    /// `process`, `fs.put`, `fs.delete` or `fs.replace_tree`.
    pub kind: String,
    /// Ids of steps that must stage before this one.
    #[serde(default)]
    pub after: Vec<String>,
    // process
    /// `process`: program and arguments (required).
    #[serde(default)]
    pub argv: Vec<String>,
    /// `process`: working directory inside the staged view.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// `process`: environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `process`: resources to stage (at least one, all `write` mode).
    #[serde(default)]
    pub mounts: Vec<StepMount>,
    /// `process`: per-step timeout.
    #[serde(default)]
    pub timeout: Option<String>,
    /// `process`: must be `"deny"` if given; egress is always denied in the MVP.
    #[serde(default)]
    pub network: Option<String>,
    /// `process`: numeric `uid[:gid]` to run as; defaults to the daemon's setting.
    #[serde(default)]
    pub user: Option<String>,
    // fs.*
    /// `fs.*`: resource id the path is relative to (required).
    #[serde(default)]
    pub resource: Option<String>,
    /// `fs.*`: path relative to the resource root (required).
    #[serde(default)]
    pub path: Option<String>,
    /// `fs.put`: literal file content; `$txid` is substituted.
    #[serde(default)]
    pub content: Option<String>,
    /// `fs.put` / `fs.replace_tree`: host path to copy from.
    #[serde(default)]
    pub source: Option<String>,
}

/// A parsed and validated manifest.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    /// The `[txn]` table.
    pub txn: TxnSection,
    /// All `[[resource]]` entries.
    #[serde(default, rename = "resource")]
    pub resources: Vec<Resource>,
    /// All `[[step]]` entries in manifest order.
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
    /// `[[effect]]` entries; any present cause validation to fail.
    #[serde(default, rename = "effect")]
    pub effects: Vec<toml::Value>,
}

/// Parse `"90"`, `"1.5s"`, `"250ms"`, `"10m"` or `"2h"` into a duration.
pub fn parse_duration(s: &str) -> Result<Duration, ManifestError> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len()));
    let n: f64 = num.parse().map_err(|_| ManifestError::Invalid(format!("bad duration {s:?}")))?;
    let mult = match unit.trim() {
        "" | "s" | "sec" => 1.0,
        "ms" => 0.001,
        "m" | "min" => 60.0,
        "h" => 3600.0,
        u => return Err(ManifestError::Invalid(format!("bad duration unit {u:?}"))),
    };
    Ok(Duration::from_secs_f64(n * mult))
}

impl Manifest {
    /// Parse TOML and run [`Manifest::validate`].
    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        let m: Manifest = toml::from_str(text).map_err(|e| ManifestError::Parse(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    /// SHA-256 hex digest of the manifest text, stored in the `Begin` record.
    pub fn digest(text: &str) -> String {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(text.as_bytes()))
    }

    /// The transaction timeout (default 600 seconds).
    pub fn timeout(&self) -> Result<Duration, ManifestError> {
        match &self.txn.timeout {
            Some(t) => parse_duration(t),
            None => Ok(Duration::from_secs(600)),
        }
    }

    /// Look up a resource by id.
    pub fn resource(&self, id: &str) -> Option<&Resource> {
        self.resources.iter().find(|r| r.id == id)
    }

    /// Check every rule the MVP enforces: unique ids, supported kinds, absolute
    /// paths, read/write modes, required per-kind fields, known `after` targets,
    /// no cycles, parsable durations, no `[[effect]]`.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let inv = |s: String| ManifestError::Invalid(s);
        if self.txn.name.trim().is_empty() {
            return Err(inv("txn.name is required".into()));
        }
        if !self.effects.is_empty() {
            return Err(inv("[[effect]] (deferred effects / outbox) is not supported in this version".into()));
        }
        let mut ids = HashSet::new();
        for r in &self.resources {
            if !ids.insert(r.id.clone()) {
                return Err(inv(format!("duplicate resource id {:?}", r.id)));
            }
            if r.kind != "fs.tree" {
                return Err(inv(format!("resource {:?}: unsupported kind {:?} (supported: fs.tree)", r.id, r.kind)));
            }
            if !r.path.is_absolute() {
                return Err(inv(format!("resource {:?}: path must be absolute", r.id)));
            }
            if !matches!(r.mode.as_str(), "read" | "write") {
                return Err(inv(format!("resource {:?}: mode must be read or write", r.id)));
            }
        }
        let mut sids = HashSet::new();
        for s in &self.steps {
            if !sids.insert(s.id.clone()) {
                return Err(inv(format!("duplicate step id {:?}", s.id)));
            }
            if s.id.contains('/') || s.id.contains("..") {
                return Err(inv(format!("step id {:?} must not contain '/' or '..'", s.id)));
            }
            match s.kind.as_str() {
                "process" => {
                    if s.argv.is_empty() {
                        return Err(inv(format!("step {:?}: argv is required", s.id)));
                    }
                    if s.mounts.is_empty() {
                        return Err(inv(format!("step {:?}: a process step must mount at least one resource", s.id)));
                    }
                    for m in &s.mounts {
                        let r = self.resource(&m.resource).ok_or_else(|| inv(format!("step {:?}: unknown resource {:?}", s.id, m.resource)))?;
                        if r.mode != "write" {
                            return Err(inv(format!("step {:?}: resource {:?} is read-only", s.id, m.resource)));
                        }
                    }
                    if let Some(n) = &s.network
                        && n != "deny" {
                            return Err(inv(format!("step {:?}: network must be \"deny\" (deferred/allowlisted egress is post-MVP)", s.id)));
                        }
                    if let Some(t) = &s.timeout {
                        parse_duration(t)?;
                    }
                }
                "fs.put" | "fs.delete" | "fs.replace_tree" => {
                    let rid = s.resource.as_ref().ok_or_else(|| inv(format!("step {:?}: resource is required", s.id)))?;
                    let r = self.resource(rid).ok_or_else(|| inv(format!("step {:?}: unknown resource {:?}", s.id, rid)))?;
                    if r.mode != "write" {
                        return Err(inv(format!("step {:?}: resource {:?} is read-only", s.id, rid)));
                    }
                    if s.path.is_none() {
                        return Err(inv(format!("step {:?}: path is required", s.id)));
                    }
                    if s.kind == "fs.put" && s.content.is_none() && s.source.is_none() {
                        return Err(inv(format!("step {:?}: content or source is required", s.id)));
                    }
                    if s.kind == "fs.replace_tree" && s.source.is_none() {
                        return Err(inv(format!("step {:?}: source is required", s.id)));
                    }
                }
                k => return Err(inv(format!("step {:?}: unsupported kind {:?}", s.id, k))),
            }
            for a in &s.after {
                if !self.steps.iter().any(|x| &x.id == a) {
                    return Err(inv(format!("step {:?}: after refers to unknown step {:?}", s.id, a)));
                }
            }
        }
        self.ordered_steps()?;
        if let Some(t) = &self.txn.timeout {
            parse_duration(t)?;
        }
        Ok(())
    }

    /// Steps in a deterministic topological order (manifest order among
    /// independent steps). Errors on cycles.
    pub fn ordered_steps(&self) -> Result<Vec<&Step>, ManifestError> {
        let idx: HashMap<&str, usize> = self.steps.iter().enumerate().map(|(i, s)| (s.id.as_str(), i)).collect();
        let n = self.steps.len();
        let mut indeg = vec![0usize; n];
        let mut succ: Vec<Vec<usize>> = vec![vec![]; n];
        for (i, s) in self.steps.iter().enumerate() {
            for a in &s.after {
                let j = idx[a.as_str()];
                succ[j].push(i);
                indeg[i] += 1;
            }
        }
        let mut out = Vec::new();
        let mut ready: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
        while !ready.is_empty() {
            ready.sort_unstable_by(|a, b| b.cmp(a));
            let i = ready.pop().unwrap();
            out.push(&self.steps[i]);
            for &j in &succ[i] {
                indeg[j] -= 1;
                if indeg[j] == 0 {
                    ready.push(j);
                }
            }
        }
        if out.len() != n {
            return Err(ManifestError::Invalid("step dependency cycle".into()));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
[txn]
name = "demo"
timeout = "2m"
[[resource]]
id = "site"
kind = "fs.tree"
path = "/srv/site"
[[step]]
id = "build"
kind = "process"
argv = ["sh", "-c", "echo hi > x"]
mounts = [{ resource = "site" }]
[[step]]
id = "stamp"
kind = "fs.put"
resource = "site"
after = ["build"]
path = "DEPLOY"
content = "$txid"
"#;

    #[test]
    fn parses_and_orders() {
        let m = Manifest::parse(OK).unwrap();
        assert_eq!(m.timeout().unwrap(), Duration::from_secs(120));
        let o: Vec<_> = m.ordered_steps().unwrap().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(o, vec!["build", "stamp"]);
    }

    #[test]
    fn rejects_cycle_and_unknown() {
        let bad = OK.replace("after = [\"build\"]", "after = [\"nope\"]");
        assert!(Manifest::parse(&bad).is_err());
        let cyc = OK.replace("mounts = [{ resource = \"site\" }]", "mounts = [{ resource = \"site\" }]\nafter = [\"stamp\"]");
        assert!(Manifest::parse(&cyc).unwrap_err().to_string().contains("cycle"));
    }
}

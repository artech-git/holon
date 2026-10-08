//! Atomicity checker and scenario builders shared by the harness binary and
//! the integration test.

#![warn(missing_docs)]

use std::path::{Path, PathBuf};

/// A two-root scenario: each root gets `out.txt` written by the transaction.
pub struct Scenario {
    /// Directory holding the roots, the source tree and the data dir.
    pub base: PathBuf,
    /// Managed roots `root0`, `root1`, ...
    pub roots: Vec<PathBuf>,
    /// Engine data directory.
    pub data_dir: PathBuf,
}

impl Scenario {
    /// Lay out `n_roots` roots with known "before" contents and a source
    /// tree for `fs.replace_tree`.
    pub fn new(base: &Path, n_roots: usize) -> Scenario {
        let roots: Vec<PathBuf> = (0..n_roots).map(|i| base.join(format!("root{i}"))).collect();
        for r in &roots {
            std::fs::create_dir_all(r).unwrap();
            // Process steps run unprivileged; let them write into the root.
            let _ = std::fs::set_permissions(r, std::os::unix::fs::PermissionsExt::from_mode(0o777));
            std::fs::write(r.join("out.txt"), "before").unwrap();
            std::fs::create_dir_all(r.join("tree")).unwrap();
            std::fs::write(r.join("tree/old"), "old").unwrap();
        }
        let src = base.join("newtree");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("new"), "new").unwrap();
        Scenario { base: base.to_path_buf(), roots, data_dir: base.join("data") }
    }

    /// A manifest that writes `out.txt` and replaces `tree/` in every root,
    /// plus one process step on `root0` when `with_process` is set.
    pub fn manifest(&self, with_process: bool) -> String {
        let mut m = String::from("[txn]\nname = \"crash\"\ntimeout = \"60s\"\n");
        for (i, r) in self.roots.iter().enumerate() {
            m.push_str(&format!("[[resource]]\nid = \"r{i}\"\nkind = \"fs.tree\"\npath = \"{}\"\n", r.display()));
        }
        for i in 0..self.roots.len() {
            m.push_str(&format!("[[step]]\nid = \"put{i}\"\nkind = \"fs.put\"\nresource = \"r{i}\"\npath = \"out.txt\"\ncontent = \"after\"\n"));
            m.push_str(&format!("[[step]]\nid = \"tree{i}\"\nkind = \"fs.replace_tree\"\nresource = \"r{i}\"\npath = \"tree\"\nsource = \"{}\"\n", self.base.join("newtree").display()));
        }
        if with_process {
            m.push_str("[[step]]\nid = \"proc\"\nkind = \"process\"\nargv = [\"/bin/sh\", \"-c\", \"echo made > made.txt\"]\nmounts = [{ resource = \"r0\" }]\ncwd = \"");
            m.push_str(&self.roots[0].display().to_string());
            m.push_str("\"\n");
        }
        m
    }

    /// `Some(true)` = all effects visible, `Some(false)` = none visible,
    /// `None` = partial (atomicity violation).
    pub fn check(&self, with_process: bool) -> Option<bool> {
        let mut seen: Vec<bool> = Vec::new();
        for r in &self.roots {
            let out = std::fs::read_to_string(r.join("out.txt")).unwrap_or_default();
            seen.push(out == "after");
            seen.push(r.join("tree/new").exists());
            seen.push(!r.join("tree/old").exists());
        }
        if with_process {
            seen.push(self.roots[0].join("made.txt").exists());
        }
        if seen.iter().all(|b| *b) {
            Some(true)
        } else if seen.iter().all(|b| !*b) {
            Some(false)
        } else {
            None
        }
    }

    /// Per-transaction staging directories that still exist (should be none
    /// after recovery).
    pub fn leftovers(&self) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.base.join(".txp-stage")) {
            for e in rd.flatten() {
                if let Ok(rd2) = std::fs::read_dir(e.path()) {
                    for e2 in rd2.flatten() {
                        v.push(e2.path());
                    }
                }
            }
        }
        v
    }
}

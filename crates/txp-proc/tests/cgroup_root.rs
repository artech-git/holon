//! Root-only tests of real cgroup v2 leaves (run via `scripts/test-root.sh`).

use std::time::Duration;
use txp_proc::cgroup::Cgroup;

#[tokio::test]
async fn cgroups_are_created_once_and_killed_as_a_whole() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let name = format!("txp-test-cg-{}", std::process::id());
    let a = Cgroup::create(&name).unwrap();
    assert_eq!(Cgroup::create(&name).unwrap().path, a.path, "creating twice is fine");
    assert!(Cgroup::create("no-such-parent/child").is_err());
    // A directory that is not a cgroup v2 leaf has no cgroup.kill.
    assert!(Cgroup::create("../..").unwrap_err().to_string().contains("cgroup.kill not available"));

    let mut child = std::process::Command::new("sleep").arg("100").spawn().unwrap();
    a.add(child.id()).unwrap();
    assert!(a.populated().unwrap());
    assert!(!a.wait_empty(Duration::from_millis(50)).await.unwrap(), "sleep is still in there");
    a.destroy().await.unwrap();
    assert!(child.wait().unwrap().code().is_none(), "killed by a signal");
    assert!(!a.path.exists());
}

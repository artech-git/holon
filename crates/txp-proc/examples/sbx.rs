//! Debug helper: `sudo sbx <root> [--no-landlock] [--no-seccomp] -- cmd args...`
use std::path::PathBuf;
use std::time::Duration;
use txp_proc::cgroup::Cgroup;
use txp_proc::sandbox::{run, OverlayMount, SandboxSpec};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(&args[0]);
    let landlock = !args.contains(&"--no-landlock".to_string());
    let seccomp = !args.contains(&"--no-seccomp".to_string());
    let idx = args.iter().position(|a| a == "--").unwrap();
    let argv = args[idx + 1..].to_vec();
    let stage = txp_fs::stage_root_for(&root).join("dbg");
    let cg = Cgroup::create("dbg").unwrap();
    let spec = SandboxSpec {
        argv,
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        cwd: Some(root.clone()),
        mounts: vec![OverlayMount { lowers: vec![root.clone()], upper: stage.join("upper"), work: stage.join("work"), at: root.clone() }],
        uid: 65534,
        gid: 65534,
        timeout: Duration::from_secs(5),
        cgroup: cg.clone(),
        landlock,
        seccomp,
        private_tmp: true,
        output_limit: 65536,
    };
    let t = std::time::Instant::now();
    let r = run(spec).await;
    let _ = cg.destroy().await;
    println!("{:?} in {:?}", r, t.elapsed());
    println!("abi={}", txp_proc::landlock::abi_version());
}

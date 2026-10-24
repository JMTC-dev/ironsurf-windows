//! Windows job containment (`spawn_contained`): the child runs once resumed,
//! a restricted job refuses grandchildren, and dropping the containment kills
//! the child — which is what happens to every tab when the shell dies.
#![cfg(windows)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ironsurf_sandbox::spawn_contained;

fn cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new("cmd.exe");
    cmd.arg("/d").arg("/c").args(args);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    cmd
}

#[test]
fn a_contained_child_resumes_and_exits_normally() {
    let (mut child, containment) =
        spawn_contained(&mut cmd(&["exit", "7"]), true).expect("spawn contained");
    assert!(containment.is_contained(), "notes: {:?}", containment.notes);
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(7), "the suspended child must have been resumed");
}

#[test]
fn a_restricted_job_refuses_grandchildren() {
    // cmd /c cmd /c exit 0 — the inner cmd is a second process in the job.
    let (mut free, _job) = spawn_contained(&mut cmd(&["cmd.exe", "/d", "/c", "exit", "0"]), false)
        .expect("spawn unrestricted");
    assert_eq!(free.wait().unwrap().code(), Some(0), "unrestricted: grandchild runs");

    let (mut boxed, containment) =
        spawn_contained(&mut cmd(&["cmd.exe", "/d", "/c", "exit", "0"]), true)
            .expect("spawn restricted");
    assert!(containment.is_contained(), "notes: {:?}", containment.notes);
    assert_ne!(
        boxed.wait().unwrap().code(),
        Some(0),
        "restricted: one process per job, the grandchild must not start"
    );
}

#[test]
fn dropping_the_containment_kills_the_child() {
    // ping is one process that runs ~30 s on its own.
    let mut ping = Command::new("ping.exe");
    ping.args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null());
    let (mut child, containment) = spawn_contained(&mut ping, true).expect("spawn ping");
    assert!(containment.is_contained(), "notes: {:?}", containment.notes);
    std::thread::sleep(Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "ping should still be running");

    drop(containment);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    panic!("closing the job did not kill the child");
}

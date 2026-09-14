// Regression demo: pre-fix, `RLIMIT_NPROC=128` (absolute) on a desktop uid
// already above 128 tasks made every pthread_create return EAGAIN, which is
// where reqwest's tokio runtime build panicked in ironsurf-tab. Thread spawns
// (the primitive tokio uses) must succeed after apply().
fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let policy = ironsurf_sandbox::SandboxPolicy {
        writable_rw: vec![dir.into()],
        ..Default::default()
    };
    let st = ironsurf_sandbox::apply(&policy).expect("apply");
    for n in &st.notes {
        println!("note: {n}");
    }
    let mut handles = Vec::new();
    for _ in 0..16 {
        handles.push(
            std::thread::Builder::new()
                .stack_size(8 * 1024 * 1024)
                .spawn(|| 7u32)
                .expect("thread spawn must succeed post-sandbox"),
        );
    }
    for h in handles {
        assert_eq!(h.join().unwrap(), 7);
    }
    println!("OK: 16 threads spawned after sandbox apply");
}

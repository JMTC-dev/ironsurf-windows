//! `ironsurf-sandbox` — per-tab OS sandboxing for IronSurf.
//!
//! Contract: `ARCHITECTURE.md` §ironsurf-sandbox.
//!
//! Layers applied by [`apply`], in order:
//! 1. `prctl(PR_SET_NO_NEW_PRIVS)` — no privilege escalation via setuid etc.
//! 2. rlimits — `RLIMIT_NOFILE`, `RLIMIT_NPROC`, optional `RLIMIT_AS`.
//! 3. Landlock (Linux, kernel 5.13+) — filesystem view reduced to
//!    `readable_ro` (read-only) + `exec_ro` (read+execute) + `writable_rw`
//!    (read-write-create); everything else is denied (`EACCES`).
//!
//! Sandboxing **must never panic** and must degrade gracefully: kernels
//! without Landlock (or containers that block it) fall back to
//! rlimits + no_new_privs, and the reasons are recorded in
//! [`SandboxStatus::notes`].
//!
//! Windows has its own stack (see `src/windows.rs`): the tab drops itself to
//! Low integrity behind process mitigation policies, and the shell starts
//! every tab inside a job object ([`spawn_contained`]).
//!
//! The `sbox-probe` bin target (see `src/bin/sbox_probe.rs`) applies a real
//! sandbox in-process and verifies the expected allow/deny outcomes; the
//! integration test in `tests/probe.rs` spawns it.

use std::path::PathBuf;
use std::process::{Child, Command};
use thiserror::Error;

#[cfg(windows)]
mod windows;

/// Errors from the parts of sandbox setup that cannot degrade to "nothing".
///
/// Landlock and rlimit failures never produce this error — they are recorded
/// in [`SandboxStatus::notes`] instead.
#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("prctl({op}) failed: {source}")]
    Prctl { op: &'static str, source: std::io::Error },
    #[error("landlock setup failed hard: {message}")]
    Landlock { message: String },
}

/// What a tab process is allowed to see/do once sandboxed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Paths readable (read-only) inside the sandbox.
    pub readable_ro: Vec<PathBuf>,
    /// Read-write-create paths — exactly ONE per tab: its container profile dir.
    pub writable_rw: Vec<PathBuf>,
    /// Executable paths (read + execute), e.g. the `ironsurf-tab` binary itself.
    pub exec_ro: Vec<PathBuf>,
    /// `RLIMIT_NOFILE` (soft+hard).
    pub max_open_files: u64,
    /// `RLIMIT_NPROC` headroom. On Linux RLIMIT_NPROC is enforced per *real
    /// UID* (every process+thread of that user system-wide), so this is applied
    /// as `current tasks owned by our uid + max_processes`, never below the
    /// uid's live task count (doing so makes every `pthread_create` fail with
    /// `EAGAIN` and breaks e.g. reqwest's tokio runtime build).
    pub max_processes: u64,
    /// Optional `RLIMIT_AS` in megabytes. Off by default: breaks most allocators
    /// unless chosen carefully.
    pub address_space_mb: Option<u64>,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        SandboxPolicy {
            readable_ro: default_readable_ro()
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            writable_rw: Vec::new(),
            exec_ro: Vec::new(),
            max_open_files: 1024,
            max_processes: 128,
            address_space_mb: None,
        }
    }
}

impl SandboxPolicy {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Canonical read-only paths a IronSurf tab needs (fonts, TLS trust roots).
/// Missing paths are skipped at [`apply`] time with a note.
pub fn default_readable_ro() -> Vec<&'static str> {
    vec![
        "/usr/share/fonts",
        "/usr/lib", // also where the dynamic loader usually lives
        "/lib",
        "/lib64",
        "/etc/ssl",
        "/etc/resolv.conf",
        "/etc/hosts",
        "/dev/urandom",
        "/dev/null",
    ]
}

/// What actually got enforced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxStatus {
    /// Landlock ruleset was enforced (fully or partially) on this process/thread.
    pub landlock_active: bool,
    /// At least one `RLIMIT_*` was applied.
    pub rlimits_applied: bool,
    /// Windows: the process dropped to Low integrity — it can no longer write
    /// anywhere but the Low-labelled `writable_rw` dirs.
    pub low_integrity: bool,
    /// Windows: the process mitigation policies that took (short names).
    pub mitigations: Vec<&'static str>,
    /// Human-readable log of what was applied and what degraded (and why).
    pub notes: Vec<String>,
}

impl SandboxStatus {
    /// True if any enforcement layer is active.
    pub fn is_restricted(&self) -> bool {
        self.landlock_active
            || self.rlimits_applied
            || self.low_integrity
            || !self.mitigations.is_empty()
    }
}

/// What [`spawn_contained`] put around a tab child. Keep it alive exactly as
/// long as the child: on Windows dropping it closes the job, which kills the
/// tab (and the OS does the same if the shell itself dies).
#[derive(Debug, Default)]
pub struct Containment {
    #[cfg(windows)]
    job: Option<std::os::windows::io::OwnedHandle>,
    /// What degraded, and why (empty when everything applied).
    pub notes: Vec<String>,
}

impl Containment {
    /// Is the child inside a job object (Windows)? Elsewhere tabs confine
    /// themselves in [`apply`] and there is nothing to hold.
    pub fn is_contained(&self) -> bool {
        #[cfg(windows)]
        {
            self.job.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

/// Spawn a tab child inside the platform's process container.
///
/// On Windows the child is created suspended, joins its own job object —
/// killed with the shell, and with `restrict`: one process, a memory cap, no
/// desktop or clipboard — and only then resumes, so not one instruction of it
/// runs outside the job. A job that cannot be set up degrades to a note; the
/// child still runs. Elsewhere this is a plain `spawn`.
pub fn spawn_contained(cmd: &mut Command, restrict: bool) -> std::io::Result<(Child, Containment)> {
    #[cfg(windows)]
    {
        windows::spawn_contained(cmd, restrict)
    }
    #[cfg(not(windows))]
    {
        let _ = restrict;
        Ok((cmd.spawn()?, Containment::default()))
    }
}

// ---------------------------------------------------------------------------
// Landlock availability probe
// ---------------------------------------------------------------------------

/// True if the running kernel supports Landlock and it is enabled.
///
/// Equivalent to the old `landlock::KernelVersion::is_available()`: performs
/// `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` and
/// checks the result. Never panics; cheap; safe from any thread.
#[cfg(target_os = "linux")]
pub fn is_available() -> bool {
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
    // SAFETY: passing a NULL attrs pointer with the VERSION flag only queries
    // the supported ABI; the kernel does not dereference the pointer.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    ret >= 0
}

#[cfg(not(target_os = "linux"))]
pub fn is_available() -> bool {
    false
}

// ---------------------------------------------------------------------------
// Linux: full stack (prctl + rlimits + Landlock)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub fn apply(policy: &SandboxPolicy) -> Result<SandboxStatus, SandboxError> {
    let mut notes: Vec<String> = Vec::new();

    // 1) PR_SET_NO_NEW_PRIVS — required for landlock_restrict_self and a
    //    baseline hardening layer on its own. Failure here is unrecoverable
    //    for sandboxing purposes (the kernel refused the basics).
    // SAFETY: prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) takes no pointer args.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(SandboxError::Prctl {
            op: "PR_SET_NO_NEW_PRIVS",
            source: std::io::Error::last_os_error(),
        });
    }
    // PR_SET_PDEATHSIG: if the shell (our parent) dies, the tab gets SIGTERM.
    // Best-effort: it only takes effect for the calling thread's parent, and
    // may be reset across execve — note it either way, never fail.
    // SAFETY: prctl(PR_SET_PDEATHSIG, SIGTERM, 0, 0, 0) takes no pointer args.
    let rc = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) };
    if rc == 0 {
        notes.push("prctl(PR_SET_PDEATHSIG, SIGTERM) applied".to_string());
    } else {
        notes.push(format!(
            "prctl(PR_SET_PDEATHSIG) failed (degraded): {}",
            std::io::Error::last_os_error()
        ));
    }
    notes.push("PR_SET_NO_NEW_PRIVS applied".to_string());

    // 2) rlimits (process-wide). Failures degrade to notes.
    let rlimits_applied = apply_rlimits(policy, &mut notes);

    // 3) Landlock (per-thread; must be applied before spawning worker
    //    threads — children inherit the domain). Best-effort: on kernels
    //    without Landlock this becomes a no-op and we note the fallback.
    let landlock_active = apply_landlock(policy, &mut notes);

    if !landlock_active && !notes.iter().any(|n| n.to_lowercase().contains("landlock")) {
        notes.push("Landlock: unknown state, treating as inactive".to_string());
    }

    Ok(SandboxStatus {
        landlock_active,
        rlimits_applied,
        notes,
        ..SandboxStatus::default()
    })
}

// ---------------------------------------------------------------------------
// Windows: Low integrity + mitigation policies (src/windows.rs)
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub fn apply(policy: &SandboxPolicy) -> Result<SandboxStatus, SandboxError> {
    Ok(windows::apply(policy))
}

// ---------------------------------------------------------------------------
// Fallback (macOS and anything else)
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "linux", windows)))]
pub fn apply(policy: &SandboxPolicy) -> Result<SandboxStatus, SandboxError> {
    let mut notes: Vec<String> = Vec::new();
    // prctl/Landlock are Linux-only; apply rlimits where libc has them.
    let rlimits_applied = apply_rlimits(policy, &mut notes);
    notes.push(format!(
        "unsupported platform {}: no prctl/Landlock; degraded to {}rlimits-only",
        std::env::consts::OS,
        if rlimits_applied { "" } else { "no-" }
    ));
    Ok(SandboxStatus {
        rlimits_applied,
        notes,
        ..SandboxStatus::default()
    })
}

// ---------------------------------------------------------------------------
// rlimits (any unix)
// ---------------------------------------------------------------------------

/// Number of tasks (processes + threads) currently owned by real uid `uid`,
/// counted from the `Threads:` field of every `/proc/<pid>/status`.
/// Returns `None` if /proc is unreadable (degrades to the old absolute-cap
/// behaviour).
#[cfg(target_os = "linux")]
fn current_task_count(uid: u32) -> Option<u64> {
    let mut total: u64 = 0;
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.bytes().all(|b| b.is_ascii_digit()) || name.is_empty() {
            continue; // non-pid entries like "self", "thread-self"
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{name}/status")) else {
            continue; // raced exit or EPERM
        };
        let mut uid_match = false;
        let mut threads: u64 = 1; // a process always has >= 1 task
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:\t") {
                uid_match = rest.split('\t').next() == Some(&uid.to_string()[..]);
                if !uid_match {
                    break;
                }
            } else if let Some(rest) = line.strip_prefix("Threads:\t") {
                threads = rest.trim().parse().unwrap_or(1);
            }
        }
        if uid_match {
            total = total.saturating_add(threads);
        }
    }
    Some(total)
}

/// `setrlimit`'s resource argument type differs per libc (`__rlimit_resource_t`
/// on Linux-gnu, `c_int` elsewhere).
#[cfg(all(unix, target_os = "linux"))]
type RlimitResource = libc::__rlimit_resource_t;
#[cfg(all(unix, not(target_os = "linux")))]
type RlimitResource = libc::c_int;

#[cfg(unix)]
fn apply_rlimits(policy: &SandboxPolicy, notes: &mut Vec<String>) -> bool {
    fn set(resource: RlimitResource, name: &str, value: u64, notes: &mut Vec<String>) -> bool {
        // Respect the existing hard limit; raising it requires privileges we
        // deliberately do not have. Clamp soft+hard down to `value`.
        let mut cur = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `cur` is a valid rlimit out-param.
        if unsafe { libc::getrlimit(resource, &mut cur) } != 0 {
            notes.push(format!(
                "getrlimit({name}) failed (degraded): {}",
                std::io::Error::last_os_error()
            ));
            return false;
        }
        let unlimited = cur.rlim_cur == libc::RLIM_INFINITY;
        let hard = if cur.rlim_max == libc::RLIM_INFINITY {
            u64::MAX
        } else {
            cur.rlim_max as u64
        };
        let target = value.min(hard);
        if !unlimited && target >= cur.rlim_cur as u64 {
            notes.push(format!(
                "rlimit({name}) left at {} (already <= requested {value})",
                cur.rlim_cur
            ));
            return true;
        }
        let lim = libc::rlimit {
            rlim_cur: target as libc::rlim_t,
            rlim_max: target as libc::rlim_t,
        };
        // SAFETY: `lim` is a valid rlimit in-param.
        if unsafe { libc::setrlimit(resource, &lim) } == 0 {
            notes.push(format!("rlimit({name}) set to {target}"));
            true
        } else {
            notes.push(format!(
                "setrlimit({name}, {target}) failed (degraded): {}",
                std::io::Error::last_os_error()
            ));
            false
        }
    }

    let mut applied = false;
    applied |= set(
        libc::RLIMIT_NOFILE,
        "NOFILE",
        policy.max_open_files,
        notes,
    );
    // RLIMIT_NPROC is per-UID on Linux: an absolute cap below the uid's current
    // task count instantly breaks every thread spawn in *all* the user's
    // processes (EAGAIN). Treat `max_processes` as headroom above current usage
    // and refuse to lower the limit under what the uid is already using.
    #[cfg(target_os = "linux")]
    let nproc_value = {
        // SAFETY: geteuid() always succeeds.
        let uid = unsafe { libc::geteuid() };
        match current_task_count(uid) {
            Some(current) => {
                let value = current.saturating_add(policy.max_processes);
                notes.push(format!(
                    "RLIMIT_NPROC is per-UID on Linux: uid {uid} has {current} tasks; setting to current+requested {value} (headroom {}) rather than absolute cap",
                    policy.max_processes
                ));
                value
            }
            None => policy.max_processes,
        }
    };
    #[cfg(not(target_os = "linux"))]
    let nproc_value = policy.max_processes;
    applied |= set(libc::RLIMIT_NPROC, "NPROC", nproc_value, notes);
    if let Some(mb) = policy.address_space_mb {
        // RLIMIT_AS is off by default: a too-low value reliably kills the
        // process instead of sandboxing it.
        applied |= set(
            libc::RLIMIT_AS,
            "AS",
            mb.saturating_mul(1024 * 1024),
            notes,
        );
    }
    applied
}

#[cfg(not(any(unix, windows)))]
fn apply_rlimits(_policy: &SandboxPolicy, _notes: &mut Vec<String>) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Landlock via the `landlock` crate (best-effort)
// ---------------------------------------------------------------------------

/// Enforce a Landlock ruleset on the calling thread (which must happen before
/// any worker threads exist; they inherit the domain).
///
/// Uses ABI-v1-handled access so that rights introduced later (Truncate,
/// MakeSym, …) stay unrestricted rather than accidentally denied, and
/// `CompatLevel::BestEffort` so kernels without (or with older) Landlock
/// degrade to no-op instead of failing. Returns whether anything was enforced.
#[cfg(target_os = "linux")]
fn apply_landlock(policy: &SandboxPolicy, notes: &mut Vec<String>) -> bool {
    use landlock::{
        Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, ABI,
    };

    if !is_available() {
        notes.push(
            "Landlock unavailable on this kernel — degraded to rlimits + no_new_privs"
                .to_string(),
        );
        return false;
    }

    // Access rights requested per class (all exist in ABI v1).
    let ro = AccessFs::ReadFile | AccessFs::ReadDir;
    let exec = AccessFs::ReadFile | AccessFs::Execute;
    let rw = AccessFs::ReadFile
        | AccessFs::WriteFile
        | AccessFs::ReadDir
        | AccessFs::RemoveDir
        | AccessFs::RemoveFile
        | AccessFs::MakeChar
        | AccessFs::MakeDir
        | AccessFs::MakeReg
        | AccessFs::MakeSock
        | AccessFs::MakeFifo;
    // NOTE: `Refer` (cross-directory link/rename) is an ABI-v2 right; handled is
    // the v1 set, so it stays unhandled (= permitted) rather than denied.

    let mut rules: Vec<PathBeneath<PathFd>> = Vec::new();
    let mut add = |paths: &[PathBuf], access: landlock::BitFlags<AccessFs>, notes: &mut Vec<String>| {
        for p in paths {
            match PathFd::new(p) {
                Ok(fd) => rules.push(PathBeneath::new(fd, access)),
                Err(e) => notes.push(format!(
                    "landlock: skipping path {} (not openable now): {e}",
                    p.display()
                )),
            }
        }
    };
    add(&policy.readable_ro, ro, notes);
    add(&policy.exec_ro, exec, notes);
    add(&policy.writable_rw, rw, notes);

    if rules.is_empty() {
        // An empty ruleset would restrict everything to nothing except what
        // stays unhandled; that is not what "no paths given" should mean.
        notes.push("landlock: no rules requested — left unenforced".to_string());
        return false;
    }

    let rule_count = rules.len();
    let enforced = (|| -> Result<RulesetStatus, landlock::RulesetError> {
        let ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            // Handle the full ABI-v1 FS right set: anything we never allow is
            // denied; rights newer than v1 stay unhandled (= allowed).
            .handle_access(AccessFs::from_all(ABI::V1))?
            .create()?
            .add_rules(rules.into_iter().map(Ok::<_, landlock::RulesetError>))?;
        Ok(ruleset.restrict_self()?.ruleset)
    })();

    match enforced {
        Ok(RulesetStatus::FullyEnforced) => {
            notes.push(format!(
                "landlock: fully enforced ({rule_count} rules)"
            ));
            true
        }
        Ok(RulesetStatus::PartiallyEnforced) => {
            notes.push(format!(
                "landlock: partially enforced ({rule_count} rules) — kernel lacks some requested features"
            ));
            true
        }
        Ok(RulesetStatus::NotEnforced) => {
            notes
                .push("landlock: kernel does not enforce (NotEnforced) — rlimits-only".to_string());
            false
        }
        Err(e) => {
            notes.push(format!(
                "landlock: setup failed, degraded to rlimits-only: {e}"
            ));
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests (sandbox-free)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_contract() {
        let p = SandboxPolicy::default();
        assert_eq!(p.max_open_files, 1024);
        assert_eq!(p.max_processes, 128);
        assert_eq!(p.address_space_mb, None);
        assert!(p.writable_rw.is_empty());
        assert!(!p.readable_ro.is_empty());
    }

    /// RLIMIT_NPROC must never be lowered below the uid's live task count
    /// (per-UID semantics: doing so breaks all thread creation with EAGAIN).
    #[test]
    #[cfg(target_os = "linux")]
    fn nproc_limit_stays_above_current_task_count() {
        let extra: Vec<_> = (0..3)
            .map(|_| {
                std::thread::spawn(|| std::thread::sleep(std::time::Duration::from_millis(100)))
            })
            .collect();
        // SAFETY: geteuid() always succeeds.
        let uid = unsafe { libc::geteuid() };
        let before = current_task_count(uid).expect("/proc readable");
        assert!(before >= 1);

        let mut notes = Vec::new();
        let policy = SandboxPolicy {
            max_processes: 128,
            ..SandboxPolicy::new()
        };
        let applied = super::apply_rlimits(&policy, &mut notes);
        assert!(applied);

        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: valid out-param.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut lim) }, 0);
        if lim.rlim_cur != libc::RLIM_INFINITY {
            assert!(
                lim.rlim_cur as u64 > before,
                "NPROC soft {} must exceed pre-apply task count {before}",
                lim.rlim_cur
            );
        }
        for h in extra {
            let _ = h.join();
        }
    }

    #[test]
    fn is_available_never_panics() {
        // Value depends on the kernel; must simply not blow up.
        let _ = is_available();
    }

    #[test]
    fn status_helpers() {
        let mut s = SandboxStatus::default();
        assert!(!s.is_restricted());
        s.rlimits_applied = true;
        assert!(s.is_restricted());
    }

    #[test]
    fn abi_v1_handled_rights_are_nonempty() {
        #[cfg(target_os = "linux")]
        {
            use landlock::{Access, AccessFs, ABI};
            let bits = AccessFs::from_all(ABI::V1);
            assert!(bits.contains(AccessFs::ReadFile));
            assert!(bits.contains(AccessFs::WriteFile));
            assert!(bits.contains(AccessFs::MakeReg));
            assert!(bits.contains(AccessFs::ReadDir));
            // Refer arrived in ABI v2 — must NOT be in the v1 handled set we use.
            assert!(!bits.contains(AccessFs::Refer));
        }
    }
}

//! Windows tab sandbox (stage 1): the Windows half of [`crate::apply`] plus
//! the shell-side job object behind [`crate::spawn_contained`].
//!
//! The tab locks *itself* down before it touches any web content — the same
//! shape as Landlock on Linux (self-applied, best-effort, every step noted):
//!
//! 1. every `writable_rw` dir gets a Low mandatory label that everything
//!    inside inherits, so it stays writable after step 3;
//! 2. process mitigation policies: no dynamic code (ACG — possible because
//!    Boa is an interpreter: nothing in the tab ever JITs), no extension
//!    points (AppInit DLLs, legacy hooks, Winsock LSPs), strict handle
//!    checks, system fonts only for GDI, no remote or Low-labelled DLLs, and
//!    last, no child processes;
//! 3. the process token drops to Low integrity: no-write-up then denies
//!    writes to everything else the user owns — files, registry keys, and
//!    other processes.
//!
//! Not covered yet, and why: Low integrity is no-write-up only, so the tab
//! can still *read* the user's files, and every tab shares the Low label, so
//! one tab could write into another container's profile. Both need an
//! AppContainer (per-container SID, read confinement), which can only be
//! chosen when the process is created — stage 2.
//!
//! The shell's half: every tab starts suspended, joins its own job object
//! (killed with the shell, one process, a memory cap, no desktop or
//! clipboard) and only then runs.

use std::ffi::{c_void, OsStr};
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
    GetNamedSecurityInfoW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    GetAce, GetLengthSid, GetSecurityDescriptorSacl, GetSidSubAuthority, SetTokenInformation,
    TokenIntegrityLevel, ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, LABEL_SECURITY_INFORMATION,
    OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SID_AND_ATTRIBUTES,
    SYSTEM_MANDATORY_LABEL_ACE, TOKEN_ADJUST_DEFAULT, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Console::GetConsoleWindow;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECTINFOCLASS,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};
use windows_sys::Win32::System::SystemServices::{
    SECURITY_MANDATORY_LOW_RID, SE_GROUP_INTEGRITY, SYSTEM_MANDATORY_LABEL_ACE_TYPE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcessToken, OpenThread, ProcessChildProcessPolicy,
    ProcessDynamicCodePolicy, ProcessExtensionPointDisablePolicy, ProcessFontDisablePolicy,
    ProcessImageLoadPolicy, ProcessStrictHandleCheckPolicy, ResumeThread,
    SetProcessMitigationPolicy, CREATE_NO_WINDOW, CREATE_SUSPENDED, PROCESS_MITIGATION_POLICY,
    THREAD_SUSPEND_RESUME,
};

use crate::{Containment, SandboxPolicy, SandboxStatus};

/// A Low mandatory label that every file and folder inside inherits (OI|CI),
/// with the default no-write-up policy.
const LOW_LABEL_SDDL: &str = "S:(ML;OICI;NW;;;LW)";
/// The Low integrity level's SID.
const LOW_INTEGRITY_SID: &str = "S-1-16-4096";

/// The mitigation policies [`apply`] sets, in order: the policy class, the
/// flag bits (every `PROCESS_MITIGATION_*_POLICY` is a single bit-field
/// DWORD) and the name reported in [`SandboxStatus::mitigations`].
const MITIGATIONS: &[(PROCESS_MITIGATION_POLICY, u32, &str)] = &[
    // ProhibitDynamicCode: no new executable memory, no RW→X flips (ACG).
    (ProcessDynamicCodePolicy, 1 << 0, "no-dynamic-code"),
    // DisableExtensionPoints: AppInit DLLs, legacy IMEs, global window hooks
    // and Winsock LSPs can no longer inject into the tab.
    (ProcessExtensionPointDisablePolicy, 1 << 0, "no-extension-points"),
    // RaiseExceptionOnInvalidHandleReference | HandleExceptionsPermanentlyEnabled:
    // a bad handle is a crash, not a handle-reuse primitive.
    (ProcessStrictHandleCheckPolicy, (1 << 0) | (1 << 1), "strict-handles"),
    // DisableNonSystemFonts: GDI (the kernel's font parser) only takes fonts
    // from the system font folder. Web fonts are parsed in user mode by the
    // engine and DirectWrite, which this does not touch.
    (ProcessFontDisablePolicy, 1 << 0, "system-fonts-only"),
    // NoRemoteImages | NoLowMandatoryLabelImages | PreferSystem32Images: no
    // DLLs from network shares or from anywhere a Low process could write.
    (ProcessImageLoadPolicy, (1 << 0) | (1 << 1) | (1 << 2), "no-remote-or-low-dlls"),
    // NoChildProcessCreation — last: it is irreversible, and nothing above
    // spawns anything.
    (ProcessChildProcessPolicy, 1 << 0, "no-child-processes"),
];

/// Committed-memory cap per tab job. A tab idles at ~1 GiB of commit (an
/// untouched up-front reservation in the engine graph; its working set is
/// ~50 MiB), so this leaves ~3 GiB for the page before the job kills a
/// runaway one.
const TAB_MEMORY_LIMIT: usize = 4 << 30;

/// Diagnostics escape hatch: a comma-separated list of layers to leave off —
/// any [`MITIGATIONS`] name and/or `low-integrity` (e.g.
/// `IRONSURF_SANDBOX_SKIP=no-dynamic-code,low-integrity`). Each skip is noted.
const SKIP_ENV: &str = "IRONSURF_SANDBOX_SKIP";

/// The tab half: lock this process down (see the module docs). Never fails —
/// every step that does not take becomes a note.
pub(crate) fn apply(policy: &SandboxPolicy) -> SandboxStatus {
    let mut status = SandboxStatus::default();
    let skip_list = std::env::var(SKIP_ENV).unwrap_or_default();
    let skipped = |name: &str| skip_list.split(',').any(|s| s.trim() == name);

    // 1) Keep the profile writable once this process is at Low integrity.
    for dir in &policy.writable_rw {
        let _ = std::fs::create_dir_all(dir);
        let labelled = match has_low_label(dir) {
            Ok(true) => Ok("already Low-labelled"),
            _ => set_low_label(dir).map(|()| "Low-labelled (inherited by its contents)"),
        };
        status.notes.push(match labelled {
            Ok(what) => format!("writable {}: {what}", dir.display()),
            Err(err) => format!(
                "writable {}: cannot label it Low ({err}) — read-only for this tab (degraded)",
                dir.display()
            ),
        });
    }

    // 2) Mitigation policies, each on its own.
    for &(class, flags, name) in MITIGATIONS {
        if skipped(name) {
            status.notes.push(format!("mitigation {name} skipped ({SKIP_ENV})"));
            continue;
        }
        // SAFETY: every policy class here takes exactly one DWORD of flags.
        let ok = unsafe {
            SetProcessMitigationPolicy(class, &flags as *const u32 as *const c_void, size_of::<u32>())
        } != 0;
        if ok {
            status.mitigations.push(name);
        } else {
            status.notes.push(format!(
                "mitigation {name} not applied (degraded): {}",
                io::Error::last_os_error()
            ));
        }
    }

    // 3) Low integrity.
    if skipped("low-integrity") {
        status.notes.push(format!("integrity: unchanged, low-integrity skipped ({SKIP_ENV})"));
        return status;
    }
    match drop_to_low_integrity() {
        Ok(()) => {
            status.low_integrity = true;
            status.notes.push(
                "integrity: Low — writes outside the writable dirs are denied; reads are not \
                 confined until an AppContainer (stage 2)"
                    .to_string(),
            );
        }
        Err(err) => status
            .notes
            .push(format!("integrity: cannot drop to Low (degraded): {err}")),
    }
    status
}

/// The shell half: spawn `cmd` suspended, put it in a fresh job, resume it.
///
/// A tab is a console program; when the shell has no console of its own (it
/// was double-clicked, and detached the one Windows opened for it), the tab
/// must not get one either — every tab would open its own black window.
pub(crate) fn spawn_contained(cmd: &mut Command, restrict: bool) -> io::Result<(Child, Containment)> {
    // SAFETY: no arguments; null when this process has no console.
    let no_console = unsafe { GetConsoleWindow() }.is_null();
    cmd.creation_flags(CREATE_SUSPENDED | if no_console { CREATE_NO_WINDOW } else { 0 });
    let mut child = cmd.spawn()?;
    let mut notes = Vec::new();
    let job = match make_job(&child, restrict, &mut notes) {
        Ok(job) => Some(job),
        Err(err) => {
            notes.push(format!("job object unavailable — tab not contained (degraded): {err}"));
            None
        }
    };
    if let Err(err) = resume(&child) {
        // A child that never resumes would hang its tab forever.
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::new(err.kind(), format!("cannot resume the suspended tab: {err}")));
    }
    Ok((child, Containment { job, notes }))
}

fn make_job(child: &Child, restrict: bool, notes: &mut Vec<String>) -> io::Result<OwnedHandle> {
    // SAFETY: default security, unnamed.
    let raw = unsafe { CreateJobObjectW(null(), null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle we own.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };

    // SAFETY: plain-data struct, all-zero is "no limits".
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    // Dies with the shell (the last job handle closing kills it), and a crash
    // is just an exit code — never a WER dialog on the user's desktop.
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    if restrict {
        limits.BasicLimitInformation.LimitFlags |=
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        limits.ProcessMemoryLimit = TAB_MEMORY_LIMIT;
    }
    set_job_info(&job, JobObjectExtendedLimitInformation, &limits)?;

    // SAFETY: both handles are live; the child is still suspended.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }

    if restrict {
        // Set after assignment: when the shell itself runs inside a job (a
        // terminal or IDE may put it in one), the tab's job is nested, and a
        // job with UI limits cannot be assigned as a nested job.
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_EXITWINDOWS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
        };
        if let Err(err) = set_job_info(&job, JobObjectBasicUIRestrictions, &ui) {
            notes.push(format!("job UI restrictions not applied (degraded): {err}"));
        }
    }
    Ok(job)
}

fn set_job_info<T>(job: &OwnedHandle, class: JOBOBJECTINFOCLASS, info: &T) -> io::Result<()> {
    // SAFETY: `info` is the struct `class` names, passed with its exact size.
    let ok = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            class,
            info as *const T as *const c_void,
            size_of::<T>() as u32,
        )
    } != 0;
    if ok {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    /// Resume every thread of a process. Undocumented, but part of ntdll's
    /// stable surface since Windows XP (Process Explorer's "Resume").
    fn NtResumeProcess(process: HANDLE) -> i32;
}

/// Resume a process created suspended. std's `Child` does not hand out the
/// primary thread's handle (stable Rust), so the process is resumed whole —
/// one syscall — falling back to finding its thread by id, which takes a
/// system-wide thread snapshot (~30 ms on a busy desktop).
fn resume(child: &Child) -> io::Result<()> {
    // SAFETY: a live process handle with PROCESS_ALL_ACCESS (CreateProcess's).
    if unsafe { NtResumeProcess(child.as_raw_handle()) } >= 0 {
        return Ok(());
    }
    resume_by_snapshot(child.id())
}

fn resume_by_snapshot(pid: u32) -> io::Result<()> {
    // SAFETY: a system-wide thread snapshot; no pointers in.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle we own.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    // SAFETY: plain-data struct; `dwSize` is set before use as the API asks.
    let mut entry: THREADENTRY32 = unsafe { zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;

    let mut resumed = 0;
    // SAFETY: `entry` is a valid THREADENTRY32 with its size set.
    let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: opening a thread by id; checked for null below.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !thread.is_null() {
                // SAFETY: a fresh handle we own.
                let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
                // SAFETY: a live thread handle with THREAD_SUSPEND_RESUME.
                if unsafe { ResumeThread(thread.as_raw_handle()) } != u32::MAX {
                    resumed += 1;
                }
            }
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    if resumed == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no thread of process {pid} could be resumed"),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Integrity labels and the token

/// NUL-terminated UTF-16 for the W APIs.
fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// `LocalFree` on drop, for what the SDDL/SID converters and
/// `GetNamedSecurityInfoW` allocate.
struct LocalBox(*mut c_void);

impl Drop for LocalBox {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by the API that returned it, freed once.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// Does `dir` already carry an inheritable Low label? Relabelling walks the
/// whole tree to propagate (a profile's cache can hold thousands of files),
/// so it is done once, not on every tab spawn.
fn has_low_label(dir: &Path) -> io::Result<bool> {
    let path = wide(dir.as_os_str());
    let mut sacl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: valid NUL-terminated path; out-params are ours.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sacl,
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    let _sd = LocalBox(sd);
    if sacl.is_null() {
        return Ok(false);
    }
    let inherit = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
    // SAFETY: `sacl` points into `sd`, alive for this scope.
    let count = unsafe { (*sacl).AceCount };
    for index in 0..u32::from(count) {
        let mut ace: *mut c_void = null_mut();
        // SAFETY: index < AceCount.
        if unsafe { GetAce(sacl, index, &mut ace) } == 0 {
            continue;
        }
        // SAFETY: every ACE starts with an ACE_HEADER.
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        if u32::from(header.AceType) != SYSTEM_MANDATORY_LABEL_ACE_TYPE
            || header.AceFlags & inherit != inherit
        {
            continue;
        }
        // SAFETY: a mandatory-label ACE's SID starts at `SidStart`.
        let sid = unsafe { std::ptr::addr_of!((*(ace as *const SYSTEM_MANDATORY_LABEL_ACE)).SidStart) }
            as PSID;
        // SAFETY: an integrity SID has exactly one sub-authority, the level.
        let level = unsafe { *GetSidSubAuthority(sid, 0) };
        if level <= SECURITY_MANDATORY_LOW_RID as u32 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Give `dir` an inheritable Low label. `SetNamedSecurityInfoW` propagates it
/// to every file and folder already below `dir`, too.
fn set_low_label(dir: &Path) -> io::Result<()> {
    let sddl = wide(OsStr::new(LOW_LABEL_SDDL));
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: valid NUL-terminated SDDL; `sd` is freed by `LocalBox`.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let _sd = LocalBox(sd);
    let (mut present, mut defaulted) = (0, 0);
    let mut sacl: *mut ACL = null_mut();
    // SAFETY: `sd` is a valid descriptor; out-params are ours.
    if unsafe { GetSecurityDescriptorSacl(sd, &mut present, &mut sacl, &mut defaulted) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let path = wide(dir.as_os_str());
    // SAFETY: valid path; `sacl` points into `sd`, alive for this call.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null(),
            sacl,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

/// Lower this process's token to Low integrity (a process may always lower
/// its own level; nothing can raise it again).
fn drop_to_low_integrity() -> io::Result<()> {
    let mut token: HANDLE = null_mut();
    // SAFETY: our own process pseudo-handle; `token` is an out-param.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_DEFAULT | TOKEN_QUERY, &mut token) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle we own.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };

    let text = wide(OsStr::new(LOW_INTEGRITY_SID));
    let mut sid: PSID = null_mut();
    // SAFETY: valid NUL-terminated SID string; `sid` is freed by `LocalBox`.
    if unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _sid = LocalBox(sid);

    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: SE_GROUP_INTEGRITY as u32,
        },
    };
    // SAFETY: `sid` is a valid SID.
    let size = size_of::<TOKEN_MANDATORY_LABEL>() as u32 + unsafe { GetLengthSid(sid) };
    // SAFETY: `label` (and the SID it points at) outlive the call.
    if unsafe {
        SetTokenInformation(
            token.as_raw_handle(),
            TokenIntegrityLevel,
            &label as *const TOKEN_MANDATORY_LABEL as *const c_void,
            size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

//! Tab process management: spawn `ironsurf-tab` children, pump their
//! `FromTab` stream, send `ToTab` commands.
//!
//! One reader thread per tab decodes length-prefixed frames straight into
//! an unbounded channel tagged with the [`TabId`]; the UI drains the channel
//! with a Slint timer (see `app.rs`). No Slint types are used here so the
//! pump can be (and is) tested headless against a scripted fake tab.

use std::io::{BufReader, IsTerminal};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI8, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::thread;

use ironsurf_common::logging::{self, Level};
use ironsurf_common::{debug, error, info, trace, warn};
use ironsurf_common::{read_frame, write_frame, ContainerId, FromTab, TabId, ToTab};

/// How the shell wires a tab child's **stderr**.
///
/// Nobody reads that stream in-process (the tab protocol is stdout-only), so a
/// chatty child can fill the 64 KiB pipe buffer and block the tab. `Auto`
/// therefore only lets the child inherit our stderr when the level reaches
/// `debug` **and** our own stderr is a terminal (drained by the emulator).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabStderr {
    /// Level + terminal heuristic (see above).
    Auto,
    /// Always inherit (`--tab-stderr`, `IRONSURF_TAB_STDERR=1`).
    Inherit,
    /// Always drop (`--no-tab-stderr`, `IRONSURF_TAB_STDERR=0`).
    Null,
}

/// -1 unset (consult env, then [`TabStderr::Auto`]), 0 null, 1 inherit.
static TAB_STDERR: AtomicI8 = AtomicI8::new(-1);

/// Override the child-stderr policy (CLI flags win over the env).
pub fn set_tab_stderr(mode: TabStderr) {
    let value = match mode {
        TabStderr::Auto => -1,
        TabStderr::Inherit => 1,
        TabStderr::Null => 0,
    };
    TAB_STDERR.store(value, Ordering::Relaxed);
}

/// Should the next spawned tab inherit our stderr?
pub fn stderr_inherit() -> bool {
    match TAB_STDERR.load(Ordering::Relaxed) {
        1 => true,
        0 => false,
        _ => match std::env::var("IRONSURF_TAB_STDERR").ok().as_deref() {
            Some(v) if matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on") => true,
            Some(v) if !v.trim().is_empty() => false,
            _ => logging::level() >= Level::Debug && std::io::stderr().is_terminal(),
        },
    }
}

/// A running tab child process with its stdin (mutexed: writes come from
/// the UI thread only, but frames must never interleave).
/// The reader thread owns the child's stdout; dropping `TabProc` closes
/// stdin (EOF ⇒ the tab exits per its contract) and reaps the child.
pub struct TabProc {
    pub id: TabId,
    pub container: ContainerId,
    child: Child,
    stdin: Option<Mutex<ChildStdin>>,
}

/// Envelope tagging each message with the tab it came from.
pub type PumpedMessage = (TabId, FromTab);

impl TabProc {
    /// Spawn `bin --tab-id N --container-id N --profile-dir P [--url URL]
    /// [extra…]` with piped stdio. `sink` receives every decoded `FromTab`.
    pub fn spawn(
        bin: &Path,
        id: TabId,
        container: ContainerId,
        profile_dir: &Path,
        url: Option<&str>,
        extra_args: &[&str],
        sink: &Sender<PumpedMessage>,
    ) -> std::io::Result<Self> {
        let mut cmd = Command::new(bin);
        cmd.arg("--tab-id")
            .arg(id.0.to_string())
            .arg("--container-id")
            .arg(container.0.to_string())
            .arg("--profile-dir")
            .arg(profile_dir);
        if let Some(url) = url {
            cmd.arg("--url").arg(url);
        }
        let inherit = stderr_inherit();
        cmd.args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if inherit { Stdio::inherit() } else { Stdio::null() });
        debug!(
            "spawning {} (tab {id} container {container}, stderr {})",
            bin.display(),
            if inherit { "inherited" } else { "null" }
        );
        let mut child = cmd.spawn().map_err(|err| {
            error!("cannot spawn {} for tab {id}: {err}", bin.display());
            err
        })?;
        info!("tab {id} spawned in container {container} (stderr {})", if inherit { "inherited" } else { "null" });

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let tx = sink.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut messages = 0u64;
            while let Ok(msg) = read_frame::<_, FromTab>(&mut reader) {
                messages += 1;
                if tx.send((id, msg)).is_err() {
                    debug!("tab {id} pump stopped: the UI is gone (after {messages} messages)");
                    return;
                }
            }
            // EOF or corrupt stream: the reaper in app.rs notices the tab
            // never says more; Closed or child exit handling covers it.
            debug!("tab {id} message stream ended after {messages} messages");
        });

        Ok(TabProc { id, container, child, stdin: Some(Mutex::new(stdin)) })
    }

    /// Send one command to the tab.
    pub fn send(&self, msg: ToTab) -> std::io::Result<()> {
        trace!("to tab {}: {msg:?}", self.id);
        let stdin = self
            .stdin
            .as_ref()
            .ok_or_else(|| std::io::Error::other("tab stdin closed"))?;
        let mut guard = stdin.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        write_frame(&mut *guard, &msg)
    }

    /// Access to the raw child (for reaping in tests).
    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Ask the tab to shut down (persist cookies etc.), then reap.
    pub fn destroy(&mut self) {
        let started = std::time::Instant::now();
        info!("destroying tab {}", self.id);
        let _ = self.send(ToTab::Destroy);
        self.stdin = None; // close EOF too
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                debug!(
                    "tab {} exited {} after {} ms",
                    self.id,
                    status,
                    started.elapsed().as_millis()
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        warn!("tab {} did not exit within 2 s — killing it", self.id);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TabProc {
    fn drop(&mut self) {
        // If the caller forgot to destroy(): closing stdin sends EOF (the
        // tab exits per contract) and we never leave orphans behind.
        self.stdin = None;
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// The receiving end of every tab's message stream (std unbounded channel).
pub struct Pump {
    tx: Sender<PumpedMessage>,
    rx: Receiver<PumpedMessage>,
}

impl Pump {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Pump { tx, rx }
    }

    pub fn sender(&self) -> Sender<PumpedMessage> {
        self.tx.clone()
    }

    /// Drain every queued message (FIFO), oldest first.
    pub fn drain(&mut self) -> Vec<PumpedMessage> {
        let mut out = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            out.push(msg);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy is process-global (a static + env), so these tests serialize.
    static SERIALIZED: Mutex<()> = Mutex::new(());

    fn env(value: Option<&str>) {
        match value {
            Some(v) => std::env::set_var("IRONSURF_TAB_STDERR", v),
            None => std::env::remove_var("IRONSURF_TAB_STDERR"),
        }
    }

    #[test]
    fn explicit_flags_beat_the_environment() {
        let _guard = SERIALIZED.lock().unwrap_or_else(|p| p.into_inner());
        env(Some("0"));
        set_tab_stderr(TabStderr::Inherit);
        assert!(stderr_inherit(), "--tab-stderr must win over IRONSURF_TAB_STDERR=0");
        env(Some("1"));
        set_tab_stderr(TabStderr::Null);
        assert!(!stderr_inherit(), "--no-tab-stderr must win over IRONSURF_TAB_STDERR=1");
        set_tab_stderr(TabStderr::Auto);
    }

    #[test]
    fn auto_follows_the_env_then_level_and_terminal() {
        let _guard = SERIALIZED.lock().unwrap_or_else(|p| p.into_inner());
        set_tab_stderr(TabStderr::Auto);
        for truthy in ["1", "true", "yes", "on"] {
            env(Some(truthy));
            assert!(stderr_inherit(), "{truthy} must inherit the terminal");
        }
        for falsey in ["0", "false", "no", "off"] {
            env(Some(falsey));
            assert!(!stderr_inherit(), "{falsey} must null the child stderr");
        }
        // Unset: auto = level >= Debug AND our stderr is a terminal. Tests never
        // run against one, so with the logger silenced the answer is "null".
        env(Some(""));
        logging::set_level(Level::Off);
        assert!(!stderr_inherit());
        logging::set_level(logging::DEFAULT_LEVEL);
        set_tab_stderr(TabStderr::Auto);
    }
}

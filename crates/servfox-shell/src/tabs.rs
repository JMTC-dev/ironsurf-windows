//! Tab process management: spawn `servfox-tab` children, pump their
//! `FromTab` stream, send `ToTab` commands.
//!
//! One reader thread per tab decodes length-prefixed frames straight into
//! an unbounded channel tagged with the [`TabId`]; the UI drains the channel
//! with a Slint timer (see `app.rs`). No Slint types are used here so the
//! pump can be (and is) tested headless against a scripted fake tab.

use std::io::BufReader;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::thread;

use servfox_common::{read_frame, write_frame, ContainerId, FromTab, TabId, ToTab};

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
        cmd.args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn()?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let tx = sink.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(msg) = read_frame::<_, FromTab>(&mut reader) {
                if tx.send((id, msg)).is_err() {
                    break; // UI is gone
                }
            }
            // EOF or corrupt stream: the reaper in app.rs notices the tab
            // never says more; Closed or child exit handling covers it.
        });

        Ok(TabProc { id, container, child, stdin: Some(Mutex::new(stdin)) })
    }

    /// Send one command to the tab.
    pub fn send(&self, msg: ToTab) -> std::io::Result<()> {
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
        let _ = self.send(ToTab::Destroy);
        self.stdin = None; // close EOF too
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
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

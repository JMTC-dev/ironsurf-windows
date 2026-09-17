//! ironsurf-shell — the Slint GUI shell.
//!
//! Owns the container registry view, spawns one `ironsurf-tab` child process
//! per tab (stdio pipes, `ironsurf-common` framing), pumps `FromTab` messages
//! into a per-tab state machine, and composites the newest RGBA frame of the
//! active tab into the Slint window.
//!
//! The pure decision logic lives in [`classify`], [`keymap`] and [`state`]
//! (unit-tested headless); the process pump lives in [`tabs`];
//! [`app`] glues everything to the generated Slint component.

// Generated Slint component (`AppWindow`) + exported structs at crate root.
slint::include_modules!();

pub mod archive;
pub mod archive_server;
pub mod classify;
pub mod keymap;
pub mod state;
pub mod tabs;
pub mod user_agents;
pub mod userscripts;

pub mod app;

/// UI preferences persisted best-effort in `~/.local/share/ironsurf/ui.json`
/// (currently: which container new tabs open in).
pub mod uisettings {
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    pub struct UiSettings {
        /// Container id new tabs open in ("remember last chosen").
        pub new_tab_container: Option<u32>,
        /// Archive mode (the 💾 toolbar toggle): off by default, persisted
        /// so a session that left it running picks it back up.
        #[serde(default)]
        pub archive_enabled: bool,
        /// Archive root folder; `None` = [`crate::archive::default_archive_root`]
        /// (`$HOME/Downloads/ironsurf-tabs`).
        #[serde(default)]
        pub archive_dir: Option<String>,
        /// The **archive browsing server** (`ironsurf://archive`): folder, host
        /// and port, all applied from its own control page. `None`s mean "the
        /// default": folder = the archive root, host `127.0.0.1`, port random.
        #[serde(default)]
        pub archive_server: ArchiveServerSettings,
    }

    /// What the archive server serves and from where (see
    /// [`crate::archive_server`]). Kept in [`UiSettings`] so the choice
    /// survives restarts.
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    pub struct ArchiveServerSettings {
        /// Folder served (`None` = the effective archive root).
        #[serde(default)]
        pub root: Option<String>,
        /// Host bound (`None` = `127.0.0.1`).
        #[serde(default)]
        pub host: Option<String>,
        /// Port bound (`None`/`Some(0)` = a random free port).
        #[serde(default)]
        pub port: Option<u16>,
    }

    impl UiSettings {
        pub fn load(path: &std::path::Path) -> Self {
            std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default()
        }

        pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, serde_json::to_vec_pretty(self).unwrap())
        }
    }
}

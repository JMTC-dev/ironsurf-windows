//! servfox-shell — the Slint GUI shell.
//!
//! Owns the container registry view, spawns one `servfox-tab` child process
//! per tab (stdio pipes, `servfox-common` framing), pumps `FromTab` messages
//! into a per-tab state machine, and composites the newest RGBA frame of the
//! active tab into the Slint window.
//!
//! The pure decision logic lives in [`classify`], [`keymap`] and [`state`]
//! (unit-tested headless); the process pump lives in [`tabs`];
//! [`app`] glues everything to the generated Slint component.

// Generated Slint component (`AppWindow`) + exported structs at crate root.
slint::include_modules!();

pub mod classify;
pub mod keymap;
pub mod state;
pub mod tabs;

pub mod app;

/// UI preferences persisted best-effort in `~/.local/share/servfox/ui.json`
/// (currently: which container new tabs open in).
pub mod uisettings {
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    pub struct UiSettings {
        /// Container id new tabs open in ("remember last chosen").
        pub new_tab_container: Option<u32>,
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

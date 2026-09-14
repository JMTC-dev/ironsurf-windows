//! ironsurf-common — shared types and the IPC codec for IronSurf.
//!
//! Framing: `u32` LE length prefix + bincode payload, over stdin/stdout pipes.
//! See `/workspace/ARCHITECTURE.md` §ironsurf-common (single source of truth).

pub mod logging;

use serde::{Deserialize, Serialize};

/// Identifier of a container (isolated session).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContainerId(pub u32);

/// Identifier of a tab (one OS process per tab).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TabId(pub u32);

/// A named, colored, isolated browsing session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Container {
    pub id: ContainerId,
    pub name: String,
    /// `"#rrggbb"` hex, used for the tab stripe.
    pub color: String,
    pub is_default: bool,
}

/// One tab-to-shell message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FromTab {
    Ready {
        tab_id: TabId,
    },
    /// RGBA8, straight alpha, top-down; `rgba.len() == width * height * 4`.
    Frame {
        width: u32,
        height: u32,
        scale: f32,
        rgba: Vec<u8>,
    },
    TitleChanged {
        title: Option<String>,
    },
    UrlChanged {
        url: Option<String>,
    },
    LoadingChanged {
        loading: bool,
    },
    HistoryChanged {
        can_go_back: bool,
        can_go_forward: bool,
    },
    StatusText {
        text: Option<String>,
    },
    ConsoleLog {
        level: String,
        message: String,
    },
    Closed,
    Error {
        message: String,
    },
}

/// One shell-to-tab message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToTab {
    Navigate {
        url: String,
    },
    Reload,
    /// v1: no-op placeholder.
    Stop,
    Back,
    Forward,
    Resize {
        width: u32,
        height: u32,
        scale: f32,
    },
    /// Throttle background rendering.
    SetVisible {
        visible: bool,
    },
    Pointer(PointerEvent),
    Key(KeyEvent),
    Scroll(ScrollEvent),
    Destroy,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointerEvent {
    pub x: f32,
    pub y: f32,
    pub kind: PointerKind,
    pub button: MouseButton,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PointerKind {
    Move,
    Down,
    Up,
    Over,
    Out,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyEvent {
    /// DOM `key`.
    pub key: String,
    /// DOM `code`.
    pub code: String,
    pub text: Option<String>,
    pub repeat: bool,
    pub mods: Modifiers,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollEvent {
    pub x: f32,
    pub y: f32,
    pub delta_x: f32,
    pub delta_y: f32,
}

/// Keyboard modifier state as a bitflags-style `u8` newtype.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const CTRL_BIT: u8 = 1 << 0;
    pub const SHIFT_BIT: u8 = 1 << 1;
    pub const ALT_BIT: u8 = 1 << 2;
    pub const META_BIT: u8 = 1 << 3;

    #[inline]
    pub const fn new(bits: u8) -> Self {
        Modifiers(bits)
    }

    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[inline]
    pub const fn empty(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn ctrl(self) -> bool {
        self.0 & Self::CTRL_BIT != 0
    }

    #[inline]
    pub const fn shift(self) -> bool {
        self.0 & Self::SHIFT_BIT != 0
    }

    #[inline]
    pub const fn alt(self) -> bool {
        self.0 & Self::ALT_BIT != 0
    }

    #[inline]
    pub const fn meta(self) -> bool {
        self.0 & Self::META_BIT != 0
    }

    #[inline]
    pub const fn with_ctrl(mut self) -> Self {
        self.0 |= Self::CTRL_BIT;
        self
    }

    #[inline]
    pub const fn with_shift(mut self) -> Self {
        self.0 |= Self::SHIFT_BIT;
        self
    }

    #[inline]
    pub const fn with_alt(mut self) -> Self {
        self.0 |= Self::ALT_BIT;
        self
    }

    #[inline]
    pub const fn with_meta(mut self) -> Self {
        self.0 |= Self::META_BIT;
        self
    }

    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Modifiers(self.0 | other.0)
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Modifiers;
    #[inline]
    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

/// Guard against corrupt streams (frames larger than this are rejected).
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Default start page URL.
pub fn default_url() -> String {
    "https://example.com".to_string()
}

fn invalid_data<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}

/// Serialize `msg` with bincode and write it as
/// `u32 LE length prefix + payload`. Flushes the writer.
pub fn write_frame<W: std::io::Write, T: serde::Serialize>(
    w: &mut W,
    msg: &T,
) -> std::io::Result<()> {
    let payload = bincode::serialize(msg).map_err(invalid_data)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "frame too large: {} bytes (max {})",
                payload.len(),
                MAX_FRAME_BYTES
            ),
        ));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(&payload)?;
    w.flush()
}

/// Read one length-prefixed bincode frame.
pub fn read_frame<R: std::io::Read, T: serde::de::DeserializeOwned>(
    r: &mut R,
) -> std::io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes (max {MAX_FRAME_BYTES})"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    bincode::deserialize(&buf).map_err(invalid_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_event() -> KeyEvent {
        KeyEvent {
            key: "a".into(),
            code: "KeyA".into(),
            text: Some("a".into()),
            repeat: false,
            mods: Modifiers::NONE.with_ctrl(),
        }
    }

    fn pointer_event() -> PointerEvent {
        PointerEvent {
            x: 1.5,
            y: -2.25,
            kind: PointerKind::Down,
            button: MouseButton::Left,
        }
    }

    fn scroll_event() -> ScrollEvent {
        ScrollEvent {
            x: 10.0,
            y: 20.0,
            delta_x: 0.0,
            delta_y: -3.0,
        }
    }

    fn frame_round_trip<T>(msg: T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq,
    {
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).expect("write_frame");
        let got: T = read_frame(&mut buf.as_slice()).expect("read_frame");
        assert_eq!(got, msg);
        got
    }

    #[test]
    fn ids_and_container_round_trip() {
        frame_round_trip(ContainerId(7));
        frame_round_trip(TabId(42));
        frame_round_trip(Container {
            id: ContainerId(1),
            name: "personal".into(),
            color: "#ff5500".into(),
            is_default: true,
        });
        assert_eq!(ContainerId(1), ContainerId(1));
        assert_ne!(ContainerId(1), ContainerId(2));
    }

    #[test]
    fn from_tab_all_variants_round_trip() {
        frame_round_trip(FromTab::Ready { tab_id: TabId(3) });
        frame_round_trip(FromTab::Frame {
            width: 2,
            height: 2,
            scale: 1.0,
            rgba: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        });
        frame_round_trip(FromTab::TitleChanged { title: Some("t".into()) });
        frame_round_trip(FromTab::TitleChanged { title: None });
        frame_round_trip(FromTab::UrlChanged { url: Some("https://example.com".into()) });
        frame_round_trip(FromTab::UrlChanged { url: None });
        frame_round_trip(FromTab::LoadingChanged { loading: true });
        frame_round_trip(FromTab::LoadingChanged { loading: false });
        frame_round_trip(FromTab::HistoryChanged { can_go_back: true, can_go_forward: false });
        frame_round_trip(FromTab::StatusText { text: Some("s".into()) });
        frame_round_trip(FromTab::StatusText { text: None });
        frame_round_trip(FromTab::ConsoleLog { level: "warn".into(), message: "m".into() });
        frame_round_trip(FromTab::Closed);
        frame_round_trip(FromTab::Error { message: "boom".into() });
    }

    #[test]
    fn to_tab_all_variants_round_trip() {
        frame_round_trip(ToTab::Navigate { url: default_url() });
        frame_round_trip(ToTab::Reload);
        frame_round_trip(ToTab::Stop);
        frame_round_trip(ToTab::Back);
        frame_round_trip(ToTab::Forward);
        frame_round_trip(ToTab::Resize { width: 800, height: 600, scale: 2.0 });
        frame_round_trip(ToTab::SetVisible { visible: false });
        for kind in [
            PointerKind::Move,
            PointerKind::Down,
            PointerKind::Up,
            PointerKind::Over,
            PointerKind::Out,
        ] {
            for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
                frame_round_trip(ToTab::Pointer(PointerEvent {
                    kind,
                    button,
                    ..pointer_event()
                }));
            }
        }
        frame_round_trip(ToTab::Key(key_event()));
        frame_round_trip(ToTab::Scroll(scroll_event()));
        frame_round_trip(ToTab::Destroy);
    }

    #[test]
    fn key_event_all_modifier_combinations_round_trip() {
        let base = key_event();
        for bits in 0u8..=0b1111 {
            let mut ev = base.clone();
            ev.mods = Modifiers::new(bits);
            frame_round_trip(ev);
        }
    }

    #[test]
    fn modifiers_helpers() {
        assert!(Modifiers::NONE.empty());
        assert!(!Modifiers::NONE.with_ctrl().empty());
        let m = Modifiers::NONE.with_ctrl().with_shift().with_alt().with_meta();
        assert!(m.ctrl() && m.shift() && m.alt() && m.meta());
        assert_eq!(m.bits(), 0b1111);
        assert_eq!(
            Modifiers::NONE.with_ctrl() | Modifiers::NONE.with_alt(),
            Modifiers::NONE.with_ctrl().with_alt()
        );
        assert!(Modifiers::default().empty());
    }

    #[test]
    fn multiple_frames_stream_in_order() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ToTab::Reload).unwrap();
        write_frame(&mut buf, &ToTab::Navigate { url: "https://a.test".into() }).unwrap();
        write_frame(&mut buf, &FromTab::Closed).unwrap();

        let mut cur = std::io::Cursor::new(&buf);
        assert_eq!(read_frame::<_, ToTab>(&mut cur).unwrap(), ToTab::Reload);
        assert_eq!(
            read_frame::<_, ToTab>(&mut cur).unwrap(),
            ToTab::Navigate { url: "https://a.test".into() }
        );
        assert_eq!(read_frame::<_, FromTab>(&mut cur).unwrap(), FromTab::Closed);
        // Stream exhausted -> error, not a hang.
        assert!(read_frame::<_, FromTab>(&mut cur).is_err());
    }

    #[test]
    fn truncated_frame_is_error() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ToTab::Navigate { url: "https://example.com/x".into() }).unwrap();
        // Cut the payload in half (length prefix intact).
        let cut = buf.len() - 5;
        let mut truncated = &buf[..cut];
        let res = read_frame::<_, ToTab>(&mut truncated);
        assert!(res.is_err(), "truncated frame must error, got {res:?}");

        // Truncated length prefix (fewer than 4 bytes) is also an error.
        let res2 = read_frame::<_, ToTab>(&mut &[0u8, 1, 2][..]);
        assert!(res2.is_err());
    }

    #[test]
    fn empty_reader_is_error() {
        let res = read_frame::<_, FromTab>(&mut std::io::empty());
        assert!(res.is_err());
    }

    #[test]
    fn oversize_frame_is_rejected() {
        // Craft a header claiming more than MAX_FRAME_BYTES.
        let header = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes();
        let res = read_frame::<_, ToTab>(&mut &header[..]);
        assert!(res.is_err(), "oversize length prefix must error, got {res:?}");

        let u32_max = u32::MAX.to_le_bytes();
        let res2 = read_frame::<_, ToTab>(&mut &u32_max[..]);
        assert!(res2.is_err());
        assert_eq!(res2.unwrap_err().kind(), std::io::ErrorKind::InvalidData);

        // A frame exactly at MAX_FRAME_BYTES is not rejected by the guard
        // (it then fails on truncated payload, which is a different path).
        let exact = (MAX_FRAME_BYTES as u32).to_le_bytes();
        let res3 = read_frame::<_, ToTab>(&mut &exact[..]);
        assert!(res3.is_err());
        assert_ne!(res3.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn garbage_payload_is_invalid_data() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[9u8; 4]); // plausible tiny length
        buf.extend_from_slice(b"not bincode at all, definitely garbage bytes!!");
        let res = read_frame::<_, ToTab>(&mut &buf[..]);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn default_url_is_https_example() {
        assert_eq!(default_url(), "https://example.com");
    }
}

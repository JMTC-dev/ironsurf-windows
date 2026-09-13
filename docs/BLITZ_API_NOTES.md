# BLITZ_API_NOTES.md — authoritative Blitz 0.3.0-beta.2 API cheat-sheet (task 7a)

For the servfox-tab Blitz+Boa runtime rebuild (#7b+). Everything below was read from the
**vendored crates.io sources** (authoritative — do NOT trust docs.rs or GitHub main; the API
changed a lot between the older `blitz-dom 0.599`-era tutorials and `0.3.0-beta.2`):

```
REG=~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f
$REG/blitz-dom-0.3.0-beta.2        $REG/blitz-traits-0.3.0-beta.2
$REG/blitz-html-0.3.0-beta.2       $REG/blitz-net-0.3.0-beta.2
$REG/blitz-paint-0.3.0-beta.2      $REG/anyrender-0.13.0
$REG/anyrender_vello_cpu-0.17.0    (vello_cpu 0.1.0)
```

Workspace pins (`crates/servfox-tab/Cargo.toml`): `blitz-{dom,html,net,paint,traits} = 0.3.0-beta.2`,
`anyrender = 0.13`, `anyrender_vello_cpu = 0.17`, `kurbo 0.13`, `peniko 0.6`.
Working end-to-end headless example: `/workspace/crates/servfox-tab/src/spike.rs`.

### ⚠ Corrections vs the task-7a brief (and older Blitz lore)

- `NetHandler` has **ONE method** (`bytes`), not headers/status/complete/cancel. There is **no
  `Response` type and no `ResourceType`** anywhere in these crates.
- Link clicks do **not** surface a `DocumentEvent` — they call the **`NavigationProvider`** shell
  hook (`DocumentConfig::navigation_provider`). `DocumentEvent::NavigateIframe` exists only for
  iframe sub-documents. Good news: the hook is first-class, so the "resolve href ourselves"
  fallback (docs §5.3 below) is optional.
- `BaseDocument` has **no `resize()`** in this version — use `set_viewport(Viewport)`.
- `UiEvent` variants are `PointerMove/Up/Down/Cancel/Wheel/KeyUp/KeyDown/Ime/AppleStandardKeybinding`
  — there is no `UnicodeInput`; text input flows through `BlitzKeyEvent.text`.
- `DocumentEvent` is **not nameable from outside** (`blitz_dom::DocumentEvent` fails to compile —
  the `document` module is private and only `BaseDocument, DocGuard, DocGuardMut, Document,
  PlainDocument` are re-exported, `blitz-dom/src/lib.rs:83`). Drain with `handle_messages()`.
- `BaseDocument`/`HtmlDocument` are **neither `Send` nor `Sync`** (compile-probed, §8).

---

## 1. HtmlDocument::from_html + DocumentConfig

`$REG/blitz-html-0.3.0-beta.2/src/html_document.rs`

```rust
pub struct HtmlDocument { inner: BaseDocument }         // :7
impl Deref for HtmlDocument      { type Target = BaseDocument; … } // :11 (DerefMut :17)
impl Document for HtmlDocument { … }                   // :27
impl HtmlDocument {
    /// content sniffed between HTML and XHTML parsing
    pub fn from_html(html: &str, config: DocumentConfig) -> Self;  // :45
    pub fn from_xml(xml: &str, config: DocumentConfig) -> Self;    // :50 (XHTML)
    pub fn into_inner(self) -> BaseDocument;                       // :72
}
```

`parse_with` (private, :58) always appends `blitz_dom::DEFAULT_CSS` to `ua_stylesheets` if
provided, then `BaseDocument::new(config)` + parse into the mutator.

`DocumentConfig` — `$REG/blitz-dom-0.3.0-beta.2/src/config.rs:33-62` — `#[derive(Default)]`;
**every field except `style_threading` is an `Option`**:

```rust
pub struct DocumentConfig {
    pub viewport: Option<Viewport>,                    // None ⇒ Viewport::default() (0×0!, scale 1, Light)
    pub base_url: Option<String>,                      // None ⇒ "data:text/css;charset=utf-8;base64," (document.rs:570 url.rs Default)
    pub ua_stylesheets: Option<Vec<String>>,           // None ⇒ DEFAULT_CSS only
    pub net_provider: Option<Arc<dyn NetProvider>>,    // None ⇒ DummyNetProvider (never delivers!)
    pub navigation_provider: Option<Arc<dyn NavigationProvider>>, // None ⇒ DummyNavigationProvider (link clicks vanish)
    pub shell_provider: Option<Arc<dyn ShellProvider>>,// None ⇒ DummyShellProvider (no redraw/title/cursor)
    pub html_parser_provider: Option<Arc<dyn HtmlParserProvider>>, // None ⇒ Dummy… (only needed for setInnerHTML)
    pub font_ctx: Option<FontContext>,                 // None ⇒ fresh parley FontContext w/ system fonts (feature `system-fonts`)
    pub media_type: Option<MediaType>,                 // None ⇒ MediaType::screen
    pub style_threading: StyleThreading,               // NOT Option. #[default] = Sequential (doc-comment saying Parallel is stale)
    pub incremental: Option<bool>,                     // None ⇒ true (incremental layout)
    pub abort_signal: Option<AbortSignal>,             // Some ⇒ every sub-resource Request carries it
    pub subdocument_depth: usize,                      // 0 for root doc; iframes nest (max 10, iframe.rs:19)
}
```

`StyleThreading` (config.rs:18-31): `Parallel` = Stylo rayon pool (single-doc fastest; **panics
`already mutably borrowed` if two Parallel resolves overlap on different threads**); `Sequential`
(default) = safe across user threads. One-doc-per-process ⇒ either works; `Sequential` is the safe pick.

Fallback defaults verified in `BaseDocument::new` (`document.rs:385-545`).

Canonical construction (from `spike.rs`, verified working):

```rust
let config = DocumentConfig {
    base_url: Some(url.as_str().to_owned()),
    viewport: Some(Viewport::new((w as f32 * scale) as u32, (h as f32 * scale) as u32,
                                 scale, ColorScheme::Light)),
    net_provider: Some(blitz_net::Provider::shared(None)),      // replace w/ ours in #7b
    navigation_provider: Some(Arc::new(OurNavProvider { .. })), // ← add for link clicks
    shell_provider: Some(Arc::new(OurShell { .. })),            // ← add for redraw/title/cursor
    ..Default::default()
};
let mut document = HtmlDocument::from_html(&html, config);
document.resolve(0.);
document.handle_messages();          // settle inline/no-network doc messages
document.resolve(0.);
```

---

## 2. Viewport & ColorScheme

`$REG/blitz-traits-0.3.0-beta.2/src/shell.rs`

```rust
pub enum ColorScheme { #[default] Light, Dark }        // :70-74 (drives prefers-color-scheme; make_device document.rs:375-378)

#[derive(Debug, Clone, PartialEq)]
pub struct Viewport {                                  // :77
    pub color_scheme: ColorScheme,
    pub window_size: (u32, u32),                       // PHYSICAL px
    pub hidpi_scale: f32,
    pub zoom: f32,                                     // 1.0 unzoomed
}
impl Viewport {
    pub fn new(physical_width: u32, physical_height: u32,
               scale_factor: f32, color_scheme: ColorScheme) -> Self; // :96 — sets zoom = 1.0
    pub fn scale(&self) -> f32;                        // hidpi_scale * zoom
    pub fn scale_f64(&self) -> f64;
    pub fn set_hidpi_scale(&mut self, scale: f32);
    pub fn zoom(&self) -> f32;  pub fn set_zoom(&mut self, f32);
    pub fn zoom_by(&mut self, f32);  pub fn zoom_mut(&mut self) -> &mut f32;
}
// Viewport::default() = { window_size: (0,0), hidpi_scale: 1.0, zoom: 1.0, color_scheme: Light }
```

Pass **physical** px (`css_w * scale`); Stylo device + hit-testing convert back with
`scale()` in CSS px (`make_device`, `document.rs:355-380`).

---

## 3. Document / BaseDocument driving API

`$REG/blitz-dom-0.3.0-beta.2/src/document.rs` unless noted.

### resolve

```rust
// src/resolve.rs:39 (impl BaseDocument)
pub fn resolve(&mut self, current_time_for_animations: f64);
```

⚠ It **returns `()`** (not a "changed" flag) and **first calls `handle_messages()` internally**
(:51). While `has_pending_critical_resources()` (render-blocking head stylesheets) it returns
**early without restyling** (:61). Pass animation time in **ms** (0. when static).

### viewport / resize

```rust
pub fn set_viewport(&mut self, viewport: Viewport);    // :1944 — THE resize entry point (no `resize` fn exists)
pub fn viewport(&self) -> &Viewport;                   // :1979
pub fn get_viewport(&self) -> Viewport;                // :1997 (clone)
pub fn viewport_mut(&mut self) -> ViewportMut<'_>;     // :1983 — mutation guard; on Drop re-makes device if changed (mutator.rs:1266-1300)
pub fn zoom_by(&mut self, increment: f32);             // :1987
pub fn zoom_to(&mut self, zoom: f32);                  // :1992
pub fn viewport_scroll(&self) -> crate::Point<f64>;    // :2143
pub fn set_viewport_scroll(&mut self, scroll: crate::Point<f64>); // :2147
```

`set_viewport` rebuilds the Stylo device, clamps scroll, and — if the scale changed — invalidates
inline contexts and calls `shell_provider.request_redraw()` (:1957). For `ToTab::Resize` just call
`set_viewport(Viewport::new(nw*scale, nh*scale, scale, scheme))`, then `resolve`, then repaint.

### title / url / id / favicon

```rust
pub fn id(&self) -> usize;                             // :587 (this is the NetProvider `doc_id`)
pub fn url(&self) -> &url::Url;                        // :1149 (document base URL; deref of DocumentUrl)
pub fn set_base_url(&mut self, url: &str);             // :570
pub fn favicon_url(&self) -> Option<String>;           // :597 — scans <link rel~=icon href>
pub fn find_title_node(&self) -> Option<&Node>;        // :2360
```

**There is no `title()` accessor.** Title flows *out* via the shell hook:
`DocumentMutator::flush` does
`let title = self.doc.nodes[id].text_content(); self.doc.shell_provider.set_window_title(title);`
(`mutator.rs:842-845`). Implement `ShellProvider::set_window_title` (shell.rs:16) and cache the
last value → `FromTab::TitleChanged`. (Or poll `find_title_node().map(|n| n.text_content())`.)
`Node::text_content(&self) -> String` is at `node/node.rs:1090`.

### input

```rust
// on the `Document` trait (document.rs:130-152); HtmlDocument and BaseDocument impl it
fn handle_ui_event(&mut self, event: UiEvent);         // :146 — runs EventDriver::new(&mut *doc, NoopEventHandler).handle_ui_event(ev)
fn poll(&mut self, task_context: Option<std::task::Context>) -> bool; // :149 — default no-op
fn id(&self) -> usize;                                 // :157
```

Under the hood: `EventDriver::handle_ui_event` (`events/driver.rs:148`) updates hover/active/
focus state, builds the DOM event chain, dispatches to handlers, and runs the **default action**
via `BaseDocument::handle_dom_event(&mut self, event: &mut DomEvent, dispatch_event: F)` (:631).
You can drive raw DOM events yourself with `EventDriver::new(&mut dyn_document, handler)` — the
trait `EventHandler::handle_event(&mut self, chain: &[NodeId], event: &mut DomEvent,
doc: &mut dyn Document, event_state: &mut EventState)` (driver.rs:9-16) is your script hook-up.

### draining doc events / repaint & animation signals

```rust
pub fn handle_messages(&mut self);                     // :1240 — drains internal mpsc rx (resource loads)
pub fn handle_message(&mut self, msg: DocumentEvent);  // :1253 (enum NOT nameable externally)
pub fn has_pending_critical_resources(&self) -> bool;  // :1261
pub fn has_changes(&self) -> bool;                     // :1015 ⚠ INVERTED: returns `changed_nodes.is_empty()` — false ⇒ dirty!
pub fn is_animating(&self) -> bool;                    // :2031 canvas | CSS anims | subdoc | scroll-fling | scrollbar fade
pub fn poll_subdocuments(&mut self, waker: Option<&std::task::Waker>) -> bool; // :794 (iframe subdocs)
pub fn take_pending_resource_deallocations(&mut self) -> Vec<anyrender::ResourceId>; // :816
```

`DocumentEvent` variants (`document.rs:183-191`) — **complete list**:

```rust
pub enum DocumentEvent {
    ResourceLoad(ResourceLoadResponse),   // net.rs:126: { request_id, node_id: Option<NodeId>,
                                          //   resolved_url: Option<String>, result: Result<Resource, String> }
    NavigateIframe { node_id: NodeId, url: url::Url }, // only from iframe sub-documents
}
```

**No `Repaint`, no `AnimFinished`, no title/nav variants** in beta.2. Repaint signalling is done
through **your** `ShellProvider` callbacks (see §8).

Hit-testing / node queries (all on BaseDocument):

```rust
pub fn hit(&self, x: f32, y: f32) -> Option<HitResult>;            // :1549 (HitResult{node_id,is_text,x,y}, events.rs:452)
pub fn element_from_point(&self, x: f32, y: f32) -> Option<NodeId>;// :1560
pub fn elements_from_point(&self, x: f32, y: f32) -> Vec<NodeId>;  // :1595
pub fn get_node(&self, id: NodeId) -> Option<&Node>;               // :614 (versioned NodeId; stale ids → None)
pub fn node_chain(&self, id: NodeId) -> Vec<NodeId>;               // traversal.rs:129 (target..root)
pub fn get_hover_node_id(&self) -> Option<NodeId>;                 // :1936   (field hover_node_id is pub(crate))
pub fn get_mousedown_node_id(&self) -> Option<NodeId>;             // :1940
pub fn get_focussed_node_id(&self) -> Option<NodeId>;              // :622
pub fn set_hover_to(&mut self, x: f32, y: f32) -> bool;            // :1816  pub fn clear_hover(&mut self) -> bool  // :1895
pub fn set_focus_to(&mut self, id: NodeId) -> bool;                // :1669; focus_next_node/focus_prev_node/clear_focus :1640-1656
pub fn get_client_bounding_rect(&self, id: NodeId) -> Option<BoundingRect>; // :2169
pub fn query_selector(&self, selector: &str) -> Option<NodeId>;    // query_selector.rs:62 (also query_selector_all :124, *_in scoped variants)
pub fn get_cursor(&self) -> Option<cursor_icon::CursorIcon>;       // :2089
pub fn find_body_node(&self)/find_head_node()/find_element_by_tag_name(&LocalName); // :2350/:2355/:2330
```

`NodeId` = versioned `u64` (blitz-traits/src/node_id.rs): `as_u64()/from_u64()` for IPC; Debug =
`NodeId(idx+v)`.

---

## 4. UiEvent — every variant, exact shapes

All in `$REG/blitz-traits-0.3.0-beta.2/src/events.rs` (blitz-dom re-exports nothing here — depend
on `blitz-traits` directly).

```rust
#[derive(Debug, Clone)] #[repr(u8)]
pub enum UiEvent {                                      // :62
    PointerMove(BlitzPointerEvent),                     // :63
    PointerUp(BlitzPointerEvent),
    PointerDown(BlitzPointerEvent),
    PointerCancel(BlitzPointerEvent),
    Wheel(BlitzWheelEvent),                             // :67
    KeyUp(BlitzKeyEvent),
    KeyDown(BlitzKeyEvent),
    Ime(BlitzImeEvent),
    AppleStandardKeybinding(SmolStr),                   // macOS-only keybinding commands; ignore on Linux
}
```

There is **no separate over/down/up/move/cancel "over" UiEvent** — enter/leave/over/out are
*DomEvent*-level and generated by the driver from `PointerMove` (driver.rs `handle_pointer_move`).
No `UnicodeInput`, no `Scroll` UiEvent (scroll is a side effect of `Wheel`).

```rust
pub struct BlitzPointerEvent {                          // :498  (Clone, NOT Copy: Arc field)
    pub id: BlitzPointerId,                             // :464  enum { Mouse, Pen, Finger(u64) }
    pub is_primary: bool,
    pub coords: PointerCoords,                          // below
    pub button: MouseEventButton,                       // :688  enum Main=0,Auxiliary=1,Secondary=2,Fourth=3,Fifth=4 (Default=Main)
    pub buttons: MouseEventButtons,                     // :649  bitflags u8: None=0, Primary=1, Secondary=2, Auxiliary=4, Fourth=8, Fifth=16
    pub mods: keyboard_types::Modifiers,
    pub details: PointerDetails,                        // :481 (pressure/tangent/tilt/twist/altitude/azimuth — Default)
    pub element: Point<f32>,                            // driver overwrites via get_client_bounding_rect; pass {0.0,0.0}
    pub active_pointers: Arc<atomic_refcell::AtomicRefCell<Vec<BlitzPointerEvent>>>, // shared touch list; fresh empty Arc per event is fine for mouse
}
impl BlitzPointerEvent { pub fn is_mouse/is_pen/is_finger(&self); page_x()/page_y()/client_x()/client_y()/screen_x()/screen_y()/element_x()/element_y(&self) -> f32; }

pub struct PointerCoords {                              // :471  f32, ALL in CSS px
    pub page_x: f32, pub page_y: f32,                   // page_x/y = client + doc scroll; hit-test & hover use page coords
    pub screen_x: f32, pub screen_y: f32,
    pub client_x: f32, pub client_y: f32,               // viewport coords, unscrolled
}

pub struct BlitzWheelEvent {                            // :576
    pub delta: BlitzWheelDelta,                         // :570  enum Lines(f64,f64) | Pixels(f64,f64)
    pub coords: PointerCoords,
    pub buttons: MouseEventButtons,
    pub mods: keyboard_types::Modifiers,
    pub element: Point<f32>,
}
// default action (pointer.rs:828-848): Lines(x,y) scroll by (x*20, y*20) CSS px; Pixels used as-is;
// scrolled from doc.get_hover_node_id() up the scroll chain; positive delta = scroll down/right.

pub struct BlitzKeyEvent {                              // :715 — used for BOTH KeyDown and KeyUp ("KeyPress" is only a DomEventKind)
    pub key: keyboard_types::Key,                       //   DOM `key`
    pub code: keyboard_types::Code,                     //   DOM `code`
    pub modifiers: keyboard_types::Modifiers,
    pub location: keyboard_types::Location,             // Standard/Left/Right…
    pub is_auto_repeating: bool,
    pub is_composing: bool,
    pub state: KeyState,                                // :703  enum { Pressed, Released } — set to match the variant
    pub text: Option<SmolStr>,                          // printable char(s) (Unicode input rides here)
}

pub enum BlitzImeEvent { Enabled, Preedit(String, Option<(usize,usize)>), Commit(String),
                          DeleteSurrounding { before_bytes: usize, after_bytes: usize }, Disabled } // :736 (winit mirror)
```

Other event types: `BlitzInputEvent { value: String }` (:727), `BlitzFocusEvent` unit (:732),
`EventState` (:14 — `prevent_default/stop_propagation/request_redraw/is_cancelled`), `DomEvent`
(target/bubbles/cancelable/data/request_redraw) and the full `DomEventData` list (Click,
ContextMenu, DoubleClick, KeyPress, Input, Wheel, Focus/Blur/FocusIn/FocusOut, Touch*, …).
Keyboard default actions (`events/keyboard.rs`): Tab/Shift-Tab move focus (:20-27), Ctrl/Cmd+C
copies selection (:31-52), focused text inputs get edits via `apply_keypress_event` — **we feed
only `KeyDown`** (dom `KeyDown` data is what the default action consumes; `DomEventData::KeyPress`
has *no* default action, events/mod.rs:219). IME: `events/ime.rs` (only affects focused inputs).

### Modifiers — type & mapping from servfox-common

`keyboard_types::Modifiers` is a bitflags set with (keyboard-types 0.7.0,
`src/modifiers.rs:16-29`): `ALT=0x01, ALT_GRAPH=0x02, CAPS_LOCK=0x04, CONTROL=0x08, FN=0x10,
FN_LOCK=0x20, META=0x40, NUM_LOCK=0x80, SCROLL_LOCK=0x100, SHIFT=0x200, SYMBOL=0x400,
SYMBOL_LOCK=0x800, HYPER=0x1000, SUPER=0x2000`.

`servfox_common::Modifiers(pub u8)` (`/workspace/crates/servfox-common/src/lib.rs:136-177`) uses
its **own bits**: `CTRL_BIT=1<<0, SHIFT_BIT=1<<1, ALT_BIT=1<<2, META_BIT=1<<3` — **do not cast**,
translate:

```rust
use keyboard_types::Modifiers as KMods;
fn to_blitz_mods(m: servfox_common::Modifiers) -> KMods {
    let mut k = KMods::empty();
    if m.ctrl() { k |= KMods::CONTROL; }
    if m.shift() { k |= KMods::SHIFT; }
    if m.alt() { k |= KMods::ALT; }
    if m.meta() { k |= KMods::META; }
    k
}
```

`Key`/`Code` parse from DOM strings: `key.parse::<keyboard_types::Key>()` (`Key::from_str`);
`Code::from_str`. Add direct deps: `keyboard-types = "0.7"`, `atomic_refcell = "0.1"` (already in
lock, v0.1.14); `SmolStr` is re-exported as `blitz_traits::SmolStr`.

Pointer mapping recipe (shell physical px → CSS px, `s = viewport.scale()`):

```rust
let css_x = px_x / s; let css_y = px_y / s;
let scroll = doc.viewport_scroll(); // Point<f64>
let coords = PointerCoords {
    page_x: css_x + scroll.x as f32, page_y: css_y + scroll.y as f32,
    screen_x: css_x, screen_y: css_y, client_x: css_x, client_y: css_y,
};
// ToTab PointerKind::Move  → UiEvent::PointerMove
//    Down(Left)            → PointerDown  { button: Main,      buttons: MouseEventButtons::Primary, .. }
//    Up(Left)              → PointerUp    { button: Main,      buttons: MouseEventButtons::None,   .. }
//    Right → Secondary / Middle → Auxiliary; Over/Out: no UiEvent — synthesise as PointerMove.
```

After every `handle_ui_event`: `doc.resolve(now_ms)` then repaint (see §8 for the loop).

---

## 5. Link-click navigation

### 5.1 What actually happens (verified in source)

`UiEvent::PointerUp` → `handle_pointerup` (`events/pointer.rs:554`) dispatches a
`DomEventData::Click`, whose **default action** `handle_click` (pointer.rs:613) walks **up the
ancestor chain** from the click target to the nearest element with an interesting tag. For `<a>`
(pointer.rs:733-756):

```rust
local_name!("a") => {
    if let Some(href) = el.attr(local_name!("href")).map(str::to_string) {
        if let Some(url) = doc.url.resolve_relative(&href) {
            if url.fragment().is_some() && doc.url.is_same_document(&url) {
                doc.scroll_to_fragment(url.fragment().unwrap_or_default());   // in-page, no hook
            } else {
                doc.navigation_provider.navigate_to(NavigationOptions::new(url, None, doc.id()));
            }
        }
    }
    …
}
```

So: **top-level navigation IS surfaced — via the `NavigationProvider` hook, not a DocumentEvent**.
Forms submit goes to the same hook (`form.rs:169`, method POST + `document_resource` body).
`DocumentEvent::NavigateIframe { node_id, url }` is produced only by `IframeNavigationProvider`
(`iframe.rs:39`), which sub-documents use; root docs never emit it.

`NavigationProvider` — `$REG/blitz-traits-0.3.0-beta.2/src/navigation.rs`:

```rust
pub trait NavigationProvider: Send + Sync + 'static {
    fn navigate_to(&self, options: NavigationOptions);   // :11 — called from the event driver (same thread!)
}
pub struct DummyNavigationProvider;                      // no-op default — link clicks would silently die
#[non_exhaustive]
pub struct NavigationOptions {                           // :24
    pub url: Url,
    pub content_type: Option<String>,
    pub source_document: usize,                          // doc.id() of the originating document
    pub method: http::Method,                            // GET for anchor clicks
    pub document_resource: Body,                         // Empty for GET
}
impl NavigationOptions {
    pub fn new(url: Url, content_type: Option<String>, source_document: usize) -> Self;
    pub fn set_method(self, Method) -> Self;
    pub fn set_document_resource(self, Body) -> Self;
    pub fn into_request(self) -> Request;                // ready-made blitz Request for refetch
}
```

**Implementation for #7b:** an `Arc<NavHook>` holding a `Mutex<Option<NavigationOptions>>` (or
`mpsc::Sender`); `navigate_to` records the pending nav (cheap, non-blocking — it is called inline
during `handle_ui_event`), the main loop sees it after the event and starts a top-level fetch
(ARCHITECTURE: main-document fetches always use our own reqwest fetcher, so this hook is exactly
the "navigation request round-trip" the contract asks for). Caveats: `target=_blank` and
click-modifier semantics are **not** handled by blitz (every anchor click is same-document-nav);
`href` resolution + same-URL-with-fragment filtering already done; `javascript:` URLs arrive as
ordinary `Url`s (scheme `javascript`) — refuse them in the hook (contract requires StatusText + no nav).

### 5.2 Getting the clicked/target node id

`BaseDocument` input-state fields are `pub(crate)`; use the accessors (`document.rs`):

```rust
doc.get_hover_node_id()        // :1936 — updated by PointerMove/Down/Up before default action
doc.get_mousedown_node_id()    // :1940 — set on PointerDown (nearest non-anonymous ancestor, :1666)
doc.get_focussed_node_id()     // :622
doc.element_from_point(x, y)   // :1560 — direct hit test (CSS px page coords)
doc.clear_hover()              // :1895 (driver calls it for finger-up on touch)
doc.set_hover_to(x, y) -> bool // :1816
```

### 5.3 Fallback recipe — resolve the anchor href ourselves (only needed if the NavigationProvider route is bypassed; e.g. to *intercept* before nav or to report status text on hover)

The click target may be a nested `<span>`/text inside the `<a>`, so walk ancestors like blitz does
(`status-bar link` at the bottom of pointer.rs:622-633). Note `anchor_element()` /
`has_scripting_hidden` **do not exist** in beta.2:

```rust
use blitz_dom::{local_name, Node, NodeId};
fn anchor_href(doc: &blitz_dom::BaseDocument, target: NodeId) -> Option<url::Url> {
    let mut cur = Some(target);
    while Some(id) = cur {
        let node = doc.get_node(id)?;                     // NodeData helpers: node.rs:748-763
        if node.is_element_with_tag_name(&local_name!("a"))
            || node.is_element_with_tag_name(&local_name!("area")) {
            if let Some(href) = node.attr(local_name!("href")) {   // NodeData::attr, node.rs:759
                return doc.url().join(href).ok();          // url() → &url::Url, document.rs:1149
            }
        }
        cur = node.parent;
    }
    None
}
// ElementData::is_link()/attr()/has_attr(): $REG/blitz-dom-0.3.0-beta.2/src/node/element.rs:422-444
```

---

## 6. blitz_traits::net — exact traits (implementation template for netmod.rs)

`$REG/blitz-traits-0.3.0-beta.2/src/net.rs` — re-exports: `pub use bytes::Bytes; pub use
http::{self, HeaderMap, Method}; pub use url::Url;` (http 1.5, bytes 1.12 in our lock).

```rust
pub trait NetProvider: Send + Sync + 'static {                       // :19
    fn fetch(&self, doc_id: usize, request: Request, handler: Box<dyn NetHandler>); // :20
    fn is_noop(&self) -> bool { false }                              // :27 — true only for DummyNetProvider
}

pub trait NetHandler: Send + Sync + 'static {                        // :34
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes);   // :35 — THE ONLY METHOD
}                                                                    // consumes self; no headers/status/complete/cancel/error path

pub trait NetWaker: Send + Sync + 'static { fn wake(&self, client_id: usize); }  // :40
impl<F: Fn(usize) + Send + Sync + 'static> NetWaker for F { … }      // :44 — closures work

#[non_exhaustive]
pub struct Request {                                                 // :53
    pub url: Url,
    pub method: Method,                     // http::Method
    pub content_type: Option<String>,
    pub headers: HeaderMap,                 // http::HeaderMap — blitz-dom puts NOTHING in headers (UA set only by blitz-net)
    pub body: Body,                         // enum { Bytes(Bytes), Form(FormData), Empty } (:81)
    pub signal: Option<AbortSignal>,        // cheap Arc<AtomicBool>, check signal.aborted() (:201-211)
}
impl Request { pub fn get(url: Url) -> Self; pub fn signal(self, AbortSignal) -> Self; }

// FormData(Vec<Entry>); Entry { name: String, value: EntryValue }
// EntryValue::{String(String), File(PathBuf), EmptyFile}
pub struct DummyNetProvider;                                          // :167 (is_noop = true)
```

**No `Response` type, no `ResourceType`** (removed in beta). Sub-resource kind is implicit in the
handler closure blitz-dom attaches (`blitz-dom/src/net.rs:57` `enum Resource { Image(ImageType,
u32,u32,Arc<Vec<u8>>), Svg(..), Css(DocumentStyleSheet), Font(Bytes, FontFaceOverrides),
DocumentSrc(String), None }`; `util::ImageType { Image, Background(usize), Mask(usize) }`).

### How blitz-net's Provider drives the handler (`$REG/blitz-net-0.3.0-beta.2/src/lib.rs`)

```rust
impl NetProvider for Provider {                                       // :270
    fn fetch(&self, doc_id: usize, mut request: Request, handler: Box<dyn NetHandler>) {
        let client = self.client.clone();                             // reqwest 0.13 (native-tls!), per-host Semaphore(6)
        let waker = self.waker.clone();
        spawn(async move {                                            // tokio::spawn — needs a tokio Runtime!!
            let signal = request.signal.take();
            let result = /* AbortFetch::new(signal, fetch_inner(…)) */ … ;
            waker.wake(doc_id);                                       // :296 — NetWaker fires even on error
            match result {
                Ok((response_url, bytes)) => handler.bytes(response_url, bytes), // :300 — success ONLY path
                Err(e) => { /* tracing error; handler DROPPED — no error delivery */ }
            }
        });
    }
}
// fetch_inner (:148): "data" → data_url decode; "file" → std::fs::read(path); else fetch_http:
//   per-host permit → client.request(method,url).headers(request.headers).header("User-Agent", FIREFOX_UA)
//   → send → status.is_success()? → response.bytes().await  → Ok((final_url, bytes))
//   non-2xx ⇒ Err(HttpStatus) ⇒ handler never called (resource silently fails; pending criticals would hang!)
// Provider::shared(waker: Option<Arc<dyn NetWaker>>) -> Arc<dyn NetProvider>   // :127
// `cache` feature = http-cache-reqwest disk cache; NOT enabled by our deps.
```

Our own provider must: spawn onto a worker thread (own `std::thread` + channel, **or** a tokio
runtime — note blitz-net's `spawn` is `tokio::spawn`, which *panics without a runtime*: the spike
only worked because its fixture had no network subresources). Then on completion call
`handler.bytes(resolved_url, Bytes)` and `waker.wake(doc_id)` (write our self-pipe). Because
errors have no channel, on failure **drop the Box<dyn NetHandler>** (matches blitz-net) — but if
the pending resource was *critical* (head `<link rel=stylesheet>`), dropping means the doc never
unblocks `has_pending_critical_resources`… same failure mode as upstream; mitigate by resolving on
a tick so at least non-critical flow continues. Cookies: blitz-dom sends none — inject in
`Managed` mode in our provider (per ARCHITECTURE, subresources get cookies only in Managed).

### Resource plumbing inside the document (who calls our handler)

`blitz-dom/src/net.rs`: internal `ResourceHandler<T>` (:70) wraps every sub-fetch; on
completion `respond()` (:112-123) does `tx.send(DocumentEvent::ResourceLoad(response))` (:121) +
`shell_provider.request_redraw()` (:122). `NetHandler::bytes` impls parse (utf8 CSS → stylo sheet, font
bytes, image decode via anyrender, `<iframe src>` HTML string) and respond. `tx` belongs to the
mpsc pair created in `BaseDocument::new` (`document.rs:450`); `resolve`/`handle_messages` drain it
on the document thread. So the **only** way bytes reach the doc is: NetHandler (any thread) →
channel → `handle_messages` (doc thread). Also: `StylesheetLoader` (`net.rs:174-220`) recursively
fetches `@import`/`url()`s through the same NetProvider.

---

## 7. Paint: paint_scene + render_to_buffer, pixel layout

```rust
// $REG/blitz-paint-0.3.0-beta.2/src/lib.rs:43
pub fn paint_scene(
    scene: &mut impl anyrender::PaintScene,
    doc: &mut blitz_dom::BaseDocument,   // &mut HtmlDocument coerces via DerefMut
    scale: f64,                          // viewport scale (physical/css)
    width: u32, height: u32,             // PHYSICAL px
    x_offset: u32, y_offset: u32,        // paint-window offset (0,0 for us)
);

// $REG/anyrender-0.13.0/src/lib.rs:172
pub fn render_to_buffer<R: ImageRenderer, F: FnOnce(&mut R::ScenePainter<'_>)>(
    draw_fn: F, width: u32, height: u32,
) -> Vec<u8>;                            // fresh Vec of exactly w*h*4 bytes

// $REG/anyrender_vello_cpu-0.17.0/src/image_renderer.rs:6/:29
pub struct VelloCpuImageRenderer;        // impl ImageRenderer; type ScenePainter<'a> = VelloCpuScenePainter
                                         // new(w,h) / resize / reset / render_to_vec / render(draw_fn, &mut [u8])
```

Verified usage (spike.rs): white `scene.fill(Fill::NonZero, Affine::IDENTITY,
Color::from_rgb8(255,255,255), None, &Rect…)` then `paint_scene(scene, &mut doc, scale as f64,
w, h, 0, 0)`; buffer length asserted `w*h*4`. Note `blitz_paint::render::BlitzRender::paint_scene`
(:175) early-returns while `has_pending_critical_resources` — but we use the free fn.

**Pixel layout: RGBA8, row-major, tightly packed, but PREMULTIPLIED alpha** — vello_cpu 0.1.0
renders into `PremulRgba8` (`vello_cpu-0.1.0/src/lib.rs:25,:45` doc examples). With our opaque
white background most pixels read identically to straight RGBA; only semi-transparent edges
(anti-aliased text/images) carry premultiplied RGB. ARCHITECTURE §4 says "RGBA8 straight alpha" —
if the shell ever needs straight alpha, un-premultiply (or make Slint happy the other way:
`slint::Image::from_rgba8` *expects premultiplied* data, so the buffer can ship as-is). Reusing one
`VelloCpuImageRenderer` across frames is supported (`resize`) but `render_to_buffer` builds a new
one each call — fine at ≤30fps, optimise later. Also `VelloCpuImageRenderer::with_image_cache_config`
+ `clear_image_cache` exist for image-heavy pages; `take_pending_resource_deallocations` must be
drained if we ever keep renderer state (see blitz-shell upstream habit).

---

## 8. Send-ness, threading model, repaint loop

**Neither `BaseDocument` nor `HtmlDocument` is `Send` or `Sync`** (compile-probed with
`fn assert_send::<HtmlDocument>()` / `assert_sync` inside the workspace — E0277: `(dyn
style::device::servo::FontMetricsProvider)` / `dyn HtmlParserProvider` are not Send/Sync; only
`node/node.rs:127-128` has `unsafe impl Send+Sync for Node`). The `Document` trait is just
`Any + 'static` (`document.rs:130`) — **no Send bound anywhere**. ⇒ Drive the whole document on
**one dedicated thread** (in servfox-tab: the main/poll() thread; the IPC reader can stay on the
same thread since stdin is polled, no separate engine thread needed). `NetProvider`,
`NetHandler`, `NetWaker`, `ShellProvider`, `NavigationProvider` are all `Send + Sync + 'static`
because they're called from worker threads — our impls must be thread-safe (`Mutex` etc.).
`NavigationProvider::navigate_to` however runs **on the doc thread** (inline in handle_ui_event).
Never hold `style_threading: Parallel` across threads (config.rs doc; `Sequential` default safe).

**Eventing model** — three channels tell us "something changed":

1. **Sub-resource completions** (images/fonts/CSS): internal mpsc → drained by
   `handle_messages()`, which `resolve()` calls itself; plus `shell_provider.request_redraw()`
   from `ResourceHandler::respond` (blitz-dom net.rs:117). Implement `ShellProvider::request_redraw`
   to poke the loop (self-pipe write or an AtomicBool + `wake_fd`); also supply a `NetWaker` to the
   provider (`Provider::new(Some(Arc<our_waker>))`) so *even failed* fetches wake us (blitz-net's
   error path never touches the doc channel, lib.rs:296).
2. **Shell state changes**: `set_window_title` (title changed → `FromTab::TitleChanged`),
   `set_cursor` (optional).
3. **Input default actions**: already mutate doc state inline in `handle_ui_event`; hover/scroll
   changes call `request_redraw` internally too.

Recommended loop (fits ARCHITECTURE §"no winit" lifecycle):

```rust
// on the doc thread
loop {
    poll([stdin, nav_wake_fd, redraw_fd], tick_timeout());
    // drain stdin frames → events
    //   input:  doc.handle_ui_event(ui); doc.resolve(now_ms()); maybe_paint();
    //   resize: doc.set_viewport(vp);  doc.resolve(now_ms()); maybe_paint();
    //   destroy: flush cookies, _exit(0)
    // drain nav queue (from NavigationProvider) → top-level fetch → new Document
    // drain redraw flags (from ShellProvider / NetWaker):
    //   doc.resolve(now_ms()); if !doc.has_changes() /* ⚠ inverted: false ⇒ changed */ || forced
    //       { paint + FromTab::Frame (throttle ≤30fps) }
    // tick_timeout(): doc.is_animating() ? ~16ms : None|long ; poll_subdocuments(None) for iframes
}
```

`resolve` early-returns while critical resources pend, so after the NetWaker fires for a
stylesheet we simply `resolve` again and the pending set clears (document.rs:1266+ / resolve.rs:61).
Title check per frame: compare `find_title_node().map(text_content)` (or push from
`set_window_title`) to cached → `FromTab::TitleChanged`; same for `url()` → `UrlChanged`.

---

## 9. Handy extras (for Boa layer #8 + status text)

- `doc.query_selector("script")` / `query_selector_all(...)` → `Option<NodeId>` / `Vec<NodeId>`
  (`query_selector.rs:62/:124`); iterate node text with `Node::text_content()`; `el.attr(local_name!("src"))`.
- `<script>` execution order: `query_selector_all_in` (scoped) exists if we walk per-`<head>`/`<body>`.
- `Node` field/data surface (`node/node.rs:96-130`): `id`, `parent`, `children: ThinVec<NodeId>`,
  `data: NodeData`; `data.downcast_element()/downcast_element_mut()` (:732/:740),
  `is_element_with_tag_name(&local_name!("…"))` (:748), `attr/has_attr` (:759/:763);
  `blitz_dom::{local_name, LocalName, NodeKind}` re-exports (`lib.rs:84-90`).
- `BaseDocument::mutate()` → `DocumentMutator` (tree edits, flush triggers title+style updates).
- Deps to add in #7b: `keyboard-types = "0.7"`, `atomic_refcell = "0.1"`; types also reachable via
  `blitz_traits::{events::*, net::*, navigation::*, shell::*, SmolStr}`. `kurbo/peniko` already pinned.
- **Do not** construct `DocumentEvent` (unnameable) or call `load_resource` with our own types;
  all ingestion is via the NetHandler→channel path.
- blitz-net default `Provider` uses **reqwest 0.13 + native-tls** (separate from our reqwest 0.12
  rustls — both in lock). Our netmod replaces it entirely; sandbox needs `/etc/ssl/certs` ro for us.

*Verified against the workspace's exact dependency graph (`cargo build -p servfox-tab` green at
task 7a time). Line numbers refer to the vendored files above.*

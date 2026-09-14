//! Scratch repro: does blitz-dom focus an <input> on click, accept typed
//! text, and handle <button> clicks? Run: rustc-less, via `cargo run --example repro` in tab crate.
use blitz_dom::{Document, DocumentConfig};
use blitz_html::HtmlDocument;
use blitz_traits::events::{
    MouseEventButton, MouseEventButtons, PointerCoords, PointerDetails, UiEvent,
    BlitzPointerEvent, BlitzPointerId, BlitzKeyEvent, KeyState,
};
use keyboard_types::{Code, Key, Location, Modifiers};

const PAGE: &str = r#"<!doctype html>
<html><head><title>F</title></head>
<body style="margin:0">
<input id="i" style="display:block;width:200px;height:40px">
<button id="b" style="display:block;width:100px;height:40px">Go</button>
</body></html>"#;

fn coords(x: f32, y: f32) -> PointerCoords {
    PointerCoords { page_x: x, page_y: y, screen_x: x, screen_y: y, client_x: x, client_y: y }
}

fn ptr(x: f32, y: f32, buttons: MouseEventButtons) -> BlitzPointerEvent {
    BlitzPointerEvent {
        id: BlitzPointerId::Mouse,
        is_primary: true,
        coords: coords(x, y),
        button: MouseEventButton::Main,
        buttons,
        mods: Modifiers::empty(),

        details: PointerDetails::default(),
        element: Default::default(),
        active_pointers: Default::default(),
    }
}

fn main() {
    let mut doc = HtmlDocument::from_html(PAGE, DocumentConfig::default());
    doc.resolve(0.);
    // find input node
    let ids = doc.query_selector_all("input").unwrap();
    println!("inputs found: {}", ids.len());
    let input = ids[0];

    doc.handle_ui_event(UiEvent::PointerMove(ptr(100., 20., MouseEventButtons::None)));
    doc.handle_ui_event(UiEvent::PointerDown(ptr(100., 20., MouseEventButtons::Primary)));
    doc.handle_ui_event(UiEvent::PointerUp(ptr(100., 20., MouseEventButtons::None)));
    doc.resolve(0.);

    println!("focused node == input? {:?}", doc.get_focussed_node_id() == Some(input));

    for c in "hi".chars() {
        let k = BlitzKeyEvent {
            key: Key::Character(c.to_string()),
            code: Code::Unidentified,
            modifiers: Modifiers::empty(),
            location: Location::Standard,
            is_auto_repeating: false,
            is_composing: false,
            state: KeyState::Pressed,
            text: Some(c.to_string().into()),
        };
        doc.handle_ui_event(UiEvent::KeyDown(k.clone()));
        doc.handle_ui_event(UiEvent::KeyUp(BlitzKeyEvent { state: KeyState::Released, text: None, ..k }));
    }
    doc.resolve(0.);

    let node = doc.get_node(input).unwrap();
    let el = node.element_data().unwrap();
    let val = el.text_input_data().map(|t| t.editor.raw_text().to_string());
    println!("editor value after typing: {val:?}");

    // value attribute?
    println!("value attribute: {:?}", el.attr(blitz_dom::local_name!("value")));
}

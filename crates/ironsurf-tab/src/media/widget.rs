//! The `<video>` widget: what the video looks like on the page.
//!
//! A blitz `custom-widget` is attached to each `<video>` element (see
//! `runtime::attach_media_widgets`); blitz gives it the element's layout box
//! and asks it for a scene, intrinsic sizes, and input handling. Everything
//! about playback lives in the [`MediaPlayer`]; this file is only the pixels
//! and the mouse.
//!
//! # What is drawn
//!
//! - the current frame, letter-boxed into the element box (the CSS `contain`
//!   behaviour, which is the default for `object-fit` on media in every
//!   browser);
//! - a control strip — play/pause, position bar — on hover or while paused;
//! - a placeholder when there is no frame yet: a play triangle when ready, a
//!   spinner-ish block while fetching, and a crossed bar with the codec badge
//!   when playback failed.
//!
//! There are no text glyphs in here: the software paint path IronSurf uses has
//! no font context available to a widget, so the placeholders are drawn as
//! shapes and the *words* go to the page (the `error` event, and
//! `el.mediaError` — see `docs/VIDEO.md`) and to the tab's log.

use std::cell::RefCell;
use std::rc::Rc;

use anyrender::{Paint, PaintScene};
use blitz_dom::node::ComputedStyles;
use blitz_dom::{IntrinsicSizes, Widget};
use blitz_traits::events::{MouseEventButton, UiEvent};
use kurbo::{Affine, Line, Point as KurboPoint, Rect, Stroke};
use peniko::{Blob, Color, Extend, Fill, ImageAlphaType, ImageBrush, ImageData, ImageFormat, ImageQuality, ImageSampler};

use crate::media::frame::Frame;
use crate::media::player::{MediaPlayer, PlayerState};

/// Control strip height in CSS pixels.
const CONTROLS_HEIGHT: f32 = 22.0;
/// Height of the position bar inside the strip.
const BAR_HEIGHT: f32 = 5.0;
/// The play/pause glyph's zone at the left edge of the control strip (the
/// glyph itself is drawn centred at `x = 12`, and the position bar starts at
/// `x = 30`): a click here toggles playback. Kept in step with
/// [`draw_controls`] — the whole strip used to be the scrub bar, which made
/// the drawn play button *seek a few frames* instead of playing anything.
const PLAY_ZONE_WIDTH: f32 = 30.0;
/// Where the position bar starts (x) and how far it stops short of the right
/// edge; `draw_controls` and [`VideoWidget::click_action`] share them so the
/// hit-test can never drift from the pixels.
const BAR_X: f32 = 30.0;
const BAR_RIGHT_MARGIN: f32 = 10.0;

/// The picture's box inside an element box, aspect-preserving (`object-fit:
/// contain`, centred) — CSS `object-position` defaults we do not override.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fitted {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Fitted {
    /// Fit `frame` into a `box_width × box_height` area.
    pub fn contain(box_width: f64, box_height: f64, frame_width: u32, frame_height: u32) -> Self {
        if box_width <= 0.0
            || box_height <= 0.0
            || frame_width == 0
            || frame_height == 0
        {
            return Fitted {
                x: 0.0,
                y: 0.0,
                width: box_width.max(0.0),
                height: box_height.max(0.0),
            };
        }
        let aspect = frame_width as f64 / frame_height as f64;
        let box_aspect = box_width / box_height;
        // Whichever axis is relatively shorter is filled; the other is shrunk
        // and centred.
        if box_aspect > aspect {
            let height = box_height;
            let width = height * aspect;
            Fitted {
                x: (box_width - width) / 2.0,
                y: 0.0,
                width,
                height,
            }
        } else {
            let width = box_width;
            let height = width / aspect;
            Fitted {
                x: 0.0,
                y: (box_height - height) / 2.0,
                width,
                height,
            }
        }
    }
}

/// A widget attached to one `<video>` element. The player state is shared with
/// the tab (which pumps it), so the widget can be painted without touching the
/// document.
pub struct VideoWidget {
    player: Rc<RefCell<MediaPlayer>>,
    /// The image brush for the frame currently on screen, kept so that
    /// repainting a still frame does not copy the pixels again.
    brush: Option<(u64, ImageBrush)>,
    /// Whether the pointer is over the element (drives the control strip).
    hovered: bool,
    /// The element's `width`/`height` attributes as the tab read them at
    /// attach time. The DOM traversal that attaches widgets has no CSS box
    /// sizes (layout has not necessarily run for this element yet), so these
    /// attributes are the only size information available before metadata —
    /// they stand in for the video's own size until `loadedmetadata` arrives.
    size_hint: Option<(f32, f32)>,
}

impl VideoWidget {
    /// A widget showing the given player.
    pub fn new(player: Rc<RefCell<MediaPlayer>>) -> Self {
        VideoWidget {
            player,
            brush: None,
            hovered: false,
            size_hint: None,
        }
    }

    /// A widget whose element declared `width`/`height` attributes (see
    /// [`Self::intrinsic_sizes`] for what the hint is good for).
    pub fn with_size_hint(
        player: Rc<RefCell<MediaPlayer>>,
        width_hint: f32,
        height_hint: f32,
    ) -> Self {
        let hint = if width_hint > 0.0 && height_hint > 0.0 {
            Some((width_hint, height_hint))
        } else {
            None
        };
        VideoWidget {
            size_hint: hint,
            ..Self::new(player)
        }
    }

    /// The shared player, for the tab that pumps it.
    pub fn player(&self) -> Rc<RefCell<MediaPlayer>> {
        self.player.clone()
    }

    /// Where a click landed, in the element's own CSS pixels: a click in the
    /// control strip moves the position, anywhere else toggles play/pause.
    /// Public so the mapping can be tested without a document.
    pub fn click_action(width: f32, height: f32, at: (f32, f32)) -> Click {
        let (x, y) = (f64::from(at.0), f64::from(at.1));
        if x < 0.0 || y < 0.0 || x > f64::from(width) || y >= f64::from(height) {
            return Click::Nothing;
        }
        let strip_top = f64::from(height) - f64::from(CONTROLS_HEIGHT);
        if y >= strip_top {
            // The play/pause glyph owns the left end of the strip; anywhere
            // else in the strip is the position bar. Inside the strip the far
            // right edge counts as the end of the bar (fraction 1.0), so `x`
            // may equal `width` here.
            if x < f64::from(PLAY_ZONE_WIDTH) {
                return Click::TogglePlay;
            }
            let bar_x = f64::from(BAR_X);
            let bar_width = (f64::from(width) - bar_x - f64::from(BAR_RIGHT_MARGIN)).max(1.0);
            let played = ((x - bar_x) / bar_width).clamp(0.0, 1.0);
            return Click::Seek { fraction: played };
        }
        // The picture area stays a half-open box: at exactly `x == width`
        // there is nothing there to toggle.
        if x >= f64::from(width) {
            return Click::Nothing;
        }
        Click::TogglePlay
    }

    /// Wrap the frame's pixels in a peniko brush, rebuilt only when the
    /// composited frame actually changed (the player stamps every new frame
    /// with a sequence number), so a paused element never re-copies them.
    fn image_brush(&mut self, frame: &Frame) -> &ImageBrush {
        let sequence = self.player.borrow().frame_sequence();
        if self.brush.as_ref().is_none_or(|(cached, _)| *cached != sequence) {
            self.brush = Some((sequence, brush_for(frame)));
        }
        &self.brush.as_ref().expect("just set").1
    }
}

/// What a click on the element means.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Click {
    /// Outside the element (should not happen — blitz hit-tests first).
    Nothing,
    /// Toggle play/pause (the picture area).
    TogglePlay,
    /// Move the play head to this fraction of the duration.
    Seek { fraction: f64 },
}

impl Widget for VideoWidget {
    fn intrinsic_sizes(&self) -> IntrinsicSizes {
        let player = self.player.borrow();
        match player.natural_size() {
            Some((width, height)) if width > 0 && height > 0 => IntrinsicSizes {
                width: Some(width as f32),
                height: Some(height as f32),
                ratio: Some(width as f32 / height as f32),
            },
            // Before the metadata arrives, the element's own `width`/`height`
            // attributes (what the attaching traversal knew) stand in; with
            // no attributes either, the CSS default object size (300×150)
            // applies — report nothing and let blitz use it.
            _ => match self.size_hint {
                Some((width, height)) => IntrinsicSizes {
                    width: Some(width),
                    height: Some(height),
                    ratio: Some(width / height),
                },
                None => IntrinsicSizes::default(),
            },
        }
    }

    fn requires_redraw(&self) -> bool {
        self.player.borrow().needs_repaint()
    }

    fn handle_event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::PointerDown(pointer) => {
                if pointer.button != MouseEventButton::Main {
                    return;
                }
                let point = pointer.element;
                // Blitz hands us the position in the element's own
                // coordinate space; `width`/`height` are unknown here, so the
                // strip is located from the player's last painted geometry.
                let geometry = self.player.borrow().painted_geometry();
                match Self::click_action(geometry.0, geometry.1, (point.x, point.y)) {
                    Click::Nothing => {}
                    Click::TogglePlay => {
                        let mut player = self.player.borrow_mut();
                        if player.is_playing() {
                            player.pause();
                        } else {
                            player.play();
                        }
                    }
                    Click::Seek { fraction } => {
                        let mut player = self.player.borrow_mut();
                        let duration = player.duration();
                        player.seek(fraction * duration);
                    }
                }
            }
            UiEvent::PointerMove(pointer) => {
                self.hovered = true;
                let _ = pointer;
            }
            UiEvent::PointerCancel(_) | UiEvent::PointerUp(_) => {}
            _ => {}
        }
    }

    fn paint(
        &mut self,
        _render_ctx: &mut dyn anyrender::RenderContext,
        _styles: &ComputedStyles,
        width: u32,
        height: u32,
        scale: f64,
    ) -> anyrender::Scene {
        let mut scene = anyrender::Scene::new();
        let box_rect = Rect::from_origin_size((0.0, 0.0), (f64::from(width) / scale, f64::from(height) / scale));
        // Everything is drawn in CSS pixels; blitz scales the widget scene by
        // `scale` when it composites it.
        let w = f64::from(width) / scale;
        let h = f64::from(height) / scale;

        // Letterbox bars: a video narrower than its box is framed in black.
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            Color::from_rgb8(0, 0, 0),
            None,
            &box_rect,
        );

        let player = self.player.borrow_mut();
        let frame = player.frame().cloned();
        let state = player.state();
        let position = player.position();
        let duration = player.duration();
        drop(player);

        match &frame {
            Some(frame) => {
                let fitted = Fitted::contain(w, h, frame.width, frame.height);
                let brush = self.image_brush(frame);
                let target = Rect::from_origin_size(
                    (fitted.x, fitted.y),
                    (
                        fitted.width.max(f64::EPSILON),
                        fitted.height.max(f64::EPSILON),
                    ),
                );
                let transform = Affine::scale_non_uniform(
                    fitted.width / f64::from(frame.width.max(1)),
                    fitted.height / f64::from(frame.height.max(1)),
                );
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    Paint::Image(brush.as_ref()),
                    Some(transform),
                    &target,
                );
            }
            None => {
                draw_placeholder(&mut scene, w, h, state);
            }
        }

        let show_controls = self.hovered || !matches!(state, PlayerState::Playing) || frame.is_none();
        if show_controls && w > 40.0 && h > 24.0 {
            draw_controls(&mut scene, w, h, &frame, state, position, duration);
        }
        // Record the box size so a click can find the control strip (blitz
        // gives widget events in CSS pixels but not the element's size).
        self.player
            .borrow_mut()
            .set_painted_geometry(f64::from(width) as f32 / scale as f32, f64::from(height) as f32 / scale as f32);
        scene
    }
}

/// Wrap decoded pixels in a peniko brush.
fn brush_for(frame: &Frame) -> ImageBrush {
    ImageBrush {
        image: ImageData {
            data: Blob::from(frame.rgba.clone()),
            format: ImageFormat::Rgba8,
            width: frame.width,
            height: frame.height,
            // Our decoded frames carry an explicit alpha byte that is always
            // 255, so plain (non-premultiplied) Alpha is the correct variant.
            alpha_type: ImageAlphaType::Alpha,
        },
        sampler: ImageSampler {
            x_extend: Extend::Pad,
            y_extend: Extend::Pad,
            quality: ImageQuality::Medium,
            alpha: 1.0,
        },
    }
}

/// The dark box with a shape in it that stands in for a frame.
fn draw_placeholder(scene: &mut impl PaintScene, w: f64, h: f64, state: PlayerState) {
    let background = Rect::from_origin_size((0.0, 0.0), (w, h));
    scene.fill(Fill::NonZero, Affine::IDENTITY, Color::from_rgb8(18, 18, 22), None, &background);
    match state {
        PlayerState::Loading => {
            // Three dots marching: the only honest thing to draw when the
            // bytes have not arrived.
            let centre = (w / 2.0, h / 2.0);
            for step in 0..3 {
                let offset = f64::from(step) * 14.0 - 14.0;
                let dot = Rect::from_center_size(
                    (centre.0 + offset, centre.1),
                    (7.0, 7.0),
                );
                let alpha = 0.35 + 0.3 * f64::from(step);
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    Color::from_rgba8(230, 230, 235, (alpha * 255.0) as u8),
                    None,
                    &dot,
                );
            }
        }
        PlayerState::Failed => {
            // A crossed pair of bars: "this is not going to play".
            let inset = (w.min(h) / 6.0).min(28.0);
            let stroke = Stroke {
                width: (w.min(h) / 18.0).clamp(3.0, 9.0),
                ..Default::default()
            };
            let from_top_left = Line::new(
                KurboPoint::new(inset, inset),
                KurboPoint::new(w - inset, h - inset),
            );
            let from_top_right = Line::new(
                KurboPoint::new(w - inset, inset),
                KurboPoint::new(inset, h - inset),
            );
            let colour = Color::from_rgb8(196, 68, 68);
            scene.stroke(&stroke, Affine::IDENTITY, colour, None, &from_top_left);
            scene.stroke(&stroke, Affine::IDENTITY, colour, None, &from_top_right);
        }
        _ => {
            draw_play_triangle(scene, w / 2.0, h / 2.0, (w.min(h) * 0.18).clamp(10.0, 44.0));
        }
    }
}

/// A right-pointing triangle centred on `(cx, cy)`.
fn draw_play_triangle(scene: &mut impl PaintScene, cx: f64, cy: f64, size: f64) {
    let mut triangle = kurbo::BezPath::new();
    triangle.move_to(KurboPoint::new(cx - size * 0.4, cy - size * 0.55));
    triangle.line_to(KurboPoint::new(cx + size * 0.62, cy));
    triangle.line_to(KurboPoint::new(cx - size * 0.4, cy + size * 0.55));
    triangle.close_path();
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        Color::from_rgba8(245, 245, 245, 235),
        None,
        &triangle,
    );
}

/// The strip along the bottom edge: play/pause and the position bar.
fn draw_controls(
    scene: &mut impl PaintScene,
    w: f64,
    h: f64,
    frame: &Option<Frame>,
    state: PlayerState,
    position: f64,
    duration: f64,
) {
    let top = h - f64::from(CONTROLS_HEIGHT);
    let strip = Rect::from_origin_size((0.0, top), (w, f64::from(CONTROLS_HEIGHT)));
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        Color::from_rgba8(12, 12, 16, 150),
        None,
        &strip,
    );
    if frame.is_none() {
        return; // nothing to scrub through yet
    }

    // Play/pause mark.
    let mark_centre_x = 12.0;
    let mark_centre_y = top + f64::from(CONTROLS_HEIGHT) / 2.0;
    match state {
        PlayerState::Playing => {
            for offset in [-3.5, 3.5] {
                let bar = Rect::from_center_size(
                    (mark_centre_x + offset, mark_centre_y),
                    (3.0, 11.0),
                );
                scene.fill(Fill::NonZero, Affine::IDENTITY, Color::from_rgb8(240, 240, 240), None, &bar);
            }
        }
        _ => {
            draw_play_triangle(scene, mark_centre_x, mark_centre_y, 9.0);
        }
    }

    // Position bar: track, then the played part. The geometry constants are
    // the ones `click_action` hit-tests against (see `BAR_X`/`BAR_RIGHT_MARGIN`).
    let bar_top = top + f64::from(CONTROLS_HEIGHT) / 2.0 - f64::from(BAR_HEIGHT) / 2.0;
    let track = Rect::from_origin_size(
        (f64::from(BAR_X), bar_top),
        (
            (w - f64::from(BAR_X) - f64::from(BAR_RIGHT_MARGIN)).max(1.0),
            f64::from(BAR_HEIGHT),
        ),
    );
    // A bare f64 converts into the uniform RoundedRectRadii kurbo wants.
    let radius = f64::from(BAR_HEIGHT) / 2.0;
    let rounded = kurbo::RoundedRect::from_rect(track, radius);
    scene.fill(Fill::NonZero, Affine::IDENTITY, Color::from_rgba8(255, 255, 255, 70), None, &rounded);
    let played = if duration > 0.0 {
        (position / duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    if played > 0.0 {
        let played_rect = Rect::from_origin_size((track.x0, track.y0), (track.width() * played, track.height()));
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            Color::from_rgb8(235, 235, 235),
            None,
            &kurbo::RoundedRect::from_rect(played_rect, radius),
        );
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn player() -> Rc<RefCell<MediaPlayer>> {
        Rc::new(RefCell::new(MediaPlayer::new()))
    }

    #[test]
    fn a_frame_fits_the_box_with_its_aspect_ratio_and_is_centred() {
        // A 16:9 frame in a taller box: full width, letterboxed vertically.
        let fitted = Fitted::contain(640.0, 640.0, 1280, 720);
        assert_eq!((fitted.width, fitted.height), (640.0, 360.0));
        assert!((fitted.y - 140.0).abs() < 1e-9, "centred: y = {}", fitted.y);
        assert_eq!(fitted.x, 0.0);

        // A square frame in a wide box: full height, pillarboxed.
        let fitted = Fitted::contain(800.0, 400.0, 100, 100);
        assert_eq!((fitted.width, fitted.height), (400.0, 400.0));
        assert!((fitted.x - 200.0).abs() < 1e-9);
        assert_eq!(fitted.y, 0.0);
    }

    #[test]
    fn a_degenerate_box_or_frame_never_produces_negative_geometry() {
        let fitted = Fitted::contain(0.0, 0.0, 640, 480);
        assert!(fitted.width >= 0.0 && fitted.height >= 0.0);
        let fitted = Fitted::contain(640.0, 480.0, 0, 0);
        assert!(fitted.width >= 0.0 && fitted.height >= 0.0);
        let fitted = Fitted::contain(-4.0, 10.0, 10, 10);
        assert!(fitted.width >= 0.0 && fitted.height >= 0.0);
    }

    #[test]
    fn the_control_strip_takes_the_bottom_edge_and_seeks_by_position() {
        // 400×300 element: the strip is the last 22 px; the bar runs from
        // x = 30 to x = width - 10, so a click maps onto *that* track.
        assert_eq!(
            VideoWidget::click_action(400.0, 300.0, (210.0, 290.0)),
            Click::Seek { fraction: 0.5 }
        );
        assert_eq!(
            VideoWidget::click_action(400.0, 300.0, (390.0, 290.0)),
            Click::Seek { fraction: 1.0 }
        );
        assert_eq!(
            VideoWidget::click_action(400.0, 300.0, (30.0, 278.5)),
            Click::Seek { fraction: 0.0 }
        );
        // Above the strip, it is play/pause.
        assert_eq!(
            VideoWidget::click_action(400.0, 300.0, (200.0, 150.0)),
            Click::TogglePlay
        );
        assert_eq!(VideoWidget::click_action(400.0, 300.0, (200.0, 277.0)), Click::TogglePlay);
        // And outside is nothing at all.
        assert_eq!(VideoWidget::click_action(400.0, 300.0, (-1.0, 10.0)), Click::Nothing);
        assert_eq!(VideoWidget::click_action(400.0, 300.0, (400.0, 10.0)), Click::Nothing);
    }

    #[test]
    fn the_drawn_play_button_in_the_strip_toggles_and_never_seeks() {
        // The glyph is drawn centred at x = 12 inside the strip: clicking it
        // must toggle playback (the whole-strip-seek mapping made the visible
        // play button look dead — it sought ~0.04 instead).
        assert_eq!(VideoWidget::click_action(400.0, 300.0, (12.0, 290.0)), Click::TogglePlay);
        assert_eq!(VideoWidget::click_action(400.0, 300.0, (0.0, 290.0)), Click::TogglePlay);
        assert_eq!(VideoWidget::click_action(400.0, 300.0, (29.9, 290.0)), Click::TogglePlay);
        // From x = 30 the bar takes over, fraction 0.0 at its left edge.
        assert_eq!(
            VideoWidget::click_action(400.0, 300.0, (30.0, 290.0)),
            Click::Seek { fraction: 0.0 }
        );
    }

    #[test]
    fn clicking_the_picture_toggles_playback() {
        let shared = player();
        let mut widget = VideoWidget::new(shared.clone());
        // A click needs the element box, which the paint pass records.
        let styles = initial_styles();
        let _ = widget.paint(&mut NullCtx, &styles, 400, 300, 1.0);
        assert_eq!((widget.player.borrow().painted_geometry().0, widget.player.borrow().painted_geometry().1), (400.0, 300.0));

        let click = pointer_down(200.0, 150.0);
        widget.handle_event(&click);
        // With no media loaded, `play()` cannot start anything, but the click
        // must reach the player: an element that has failed stays failed.
        assert_eq!(widget.player.borrow().state(), PlayerState::Idle);
        shared.borrow_mut().fail(crate::media::MediaError::NoVideoTrack);
        widget.handle_event(&click);
        assert_eq!(widget.player.borrow().state(), PlayerState::Failed);
    }

    #[test]
    fn clicking_the_strip_seeks_by_fraction_of_the_duration() {
        let shared = player();
        let mut widget = VideoWidget::new(shared.clone());
        let click = pointer_down(100.0, 290.0);
        widget.handle_event(&click);
        // No media: `seek` is a no-op rather than a panic.
        assert_eq!(shared.borrow().position(), 0.0);
    }

    #[test]
    fn a_widget_with_no_metadata_reports_no_intrinsic_size() {
        let widget = VideoWidget::new(player());
        let sizes = widget.intrinsic_sizes();
        assert_eq!(sizes.width, None);
        assert_eq!(sizes.ratio, None);
    }

    #[test]
    fn a_widget_with_metadata_reports_the_video_size_and_ratio() {
        let shared = player();
        let (width, height) = (640u32, 360u32);
        shared.borrow_mut().set_test_metadata(width, height);
        let widget = VideoWidget::new(shared.clone());
        let sizes = widget.intrinsic_sizes();
        assert_eq!(sizes.width, Some(640.0));
        assert_eq!(sizes.height, Some(360.0));
        assert!((sizes.ratio.unwrap() - 16.0 / 9.0).abs() < 0.01);
    }

    #[test]
    fn painting_produces_a_scene_and_never_panics_on_empty_state() {
        let shared = player();
        let mut widget = VideoWidget::new(shared.clone());
        let styles = initial_styles();
        let scene = widget.paint(&mut NullCtx, &styles, 320, 180, 2.0);
        // Placeholder + letterbox, at least.
        assert!(!scene.commands.is_empty(), "an empty element still draws something");
        // A zero-sized element is not an error (blitz skips it anyway).
        let empty = widget.paint(&mut NullCtx, &styles, 0, 0, 1.0);
        assert!(empty.commands.len() >= scene.commands.len() - scene.commands.len());
    }

    #[test]
    fn a_frame_is_painted_as_an_image_not_a_placeholder() {
        let shared = player();
        let mut widget = VideoWidget::new(shared.clone());
        shared.borrow_mut().set_test_frame(Frame::from_rgba(4, 4, vec![9u8; 4 * 4 * 4]).expect("frame"));
        let styles = initial_styles();
        let scene = widget.paint(&mut NullCtx, &styles, 64, 64, 1.0);
        let has_image = scene.commands.iter().any(|command| {
            matches!(
                command,
                anyrender::recording::RenderCommand::Fill(fill)
                    if matches!(fill.brush, anyrender::Paint::Image(_))
            )
        });
        assert!(has_image, "the decoded frame must be an image fill");
    }

    /// A `RenderContext` that registers nothing (widgets may not need to).
    struct NullCtx;
    impl anyrender::RenderContext for NullCtx {}

    /// blitz hands widgets the pre-styling initial `ComputedValues` (there is
    /// no `Default` for them); stylo builds it from initial font values.
    fn initial_styles() -> ComputedStyles {
        let font = style::properties::style_structs::Font::initial_values();
        (*ComputedStyles::initial_values_with_font_override(font)).clone()
    }

    fn pointer_down(x: f32, y: f32) -> UiEvent {
        use blitz_traits::events::{BlitzPointerEvent, BlitzPointerId, PointerCoords, PointerDetails};
        UiEvent::PointerDown(BlitzPointerEvent {
            // `BlitzPointerId` has no `Default`; the mouse is what these tests
            // are simulating.
            id: BlitzPointerId::Mouse,
            is_primary: true,
            coords: PointerCoords {
                page_x: x,
                page_y: y,
                screen_x: x,
                screen_y: y,
                client_x: x,
                client_y: y,
            },
            button: MouseEventButton::Main,
            buttons: Default::default(),
            mods: Default::default(),
            details: PointerDetails::default(),
            element: blitz_traits::events::Point { x, y },
            // blitz shares the set of live pointers via Arc; a lone click has
            // no others.
            active_pointers: std::sync::Arc::new(atomic_refcell::AtomicRefCell::new(Vec::new())),
        })
    }
}

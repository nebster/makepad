//! Pure screen-span model: resolving a request to span one, several
//! adjacent, or all screens into an index range and a bounding rect, with
//! no platform or `Cx` dependency so it unit-tests without a display.
//!
//! `WmRequest`/`WmEvent` (see `lib.rs`) will carry [`ScreenSpan`] and
//! [`WmScreen`] values over the wire once the protocol grows a span
//! request (a later change); this module only resolves them against a
//! screen list.

use makepad_widgets_core::makepad_micro_serde::*;

/// Which screens an app is asking to span.
#[derive(Clone, Debug, PartialEq, SerJson, DeJson)]
pub enum ScreenSpan {
    /// Connector names; must resolve to a contiguous run of `screens`,
    /// left to right (duplicates in the list are fine).
    Screens(Vec<String>),
    /// Every live screen.
    All,
    /// The screen the window is already on.
    Current,
}

/// One screen, as the window manager reports it. The rect is in whatever
/// coordinate space the caller agreed on (desktop space, or a receiving
/// window's local space after [`screens_in_window`]).
#[derive(Clone, Debug, PartialEq, SerJson, DeJson)]
pub struct WmScreen {
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub primary: bool,
}

/// Why a [`ScreenSpan`] could not be resolved against a screen list.
#[derive(Clone, Debug, PartialEq, SerJson, DeJson)]
pub enum SpanError {
    /// Nothing to span: an empty `Screens` list, no screens for `All`, or
    /// no current screen for `Current`.
    Empty,
    /// A name in `Screens` is not in `screens`.
    UnknownScreen(String),
    /// The resolved screens are not a contiguous run in `screens`' order.
    NotAdjacent,
}

/// Resolve `span` against `screens` (left to right). Returns the
/// half-open index range it covers and the bounding box of those
/// screens' rects (their union, including any dead space between
/// different-height screens). `current` is the window's own screen
/// index, used only by [`ScreenSpan::Current`].
pub fn span_rect(
    screens: &[WmScreen],
    span: &ScreenSpan,
    current: Option<usize>,
) -> Result<(std::ops::Range<usize>, (f64, f64, f64, f64)), SpanError> {
    let range = match span {
        ScreenSpan::Screens(names) => {
            if names.is_empty() {
                return Err(SpanError::Empty);
            }
            let mut indices = Vec::new();
            for name in names {
                let idx = screens
                    .iter()
                    .position(|s| &s.name == name)
                    .ok_or_else(|| SpanError::UnknownScreen(name.clone()))?;
                if !indices.contains(&idx) {
                    indices.push(idx);
                }
            }
            indices.sort_unstable();
            let lo = *indices.first().unwrap();
            let hi = *indices.last().unwrap();
            if indices.len() != hi - lo + 1 {
                return Err(SpanError::NotAdjacent);
            }
            lo..hi + 1
        }
        ScreenSpan::All => {
            if screens.is_empty() {
                return Err(SpanError::Empty);
            }
            0..screens.len()
        }
        ScreenSpan::Current => {
            let idx = current.filter(|&idx| idx < screens.len()).ok_or(SpanError::Empty)?;
            idx..idx + 1
        }
    };
    let rect = bounding_box(&screens[range.clone()]);
    Ok((range, rect))
}

/// The union bounding box of `screens`' rects: `(x, y, w, h)`.
fn bounding_box(screens: &[WmScreen]) -> (f64, f64, f64, f64) {
    let x0 = screens.iter().map(|s| s.x).fold(f64::INFINITY, f64::min);
    let y0 = screens.iter().map(|s| s.y).fold(f64::INFINITY, f64::min);
    let x1 = screens
        .iter()
        .map(|s| s.x + s.w)
        .fold(f64::NEG_INFINITY, f64::max);
    let y1 = screens
        .iter()
        .map(|s| s.y + s.h)
        .fold(f64::NEG_INFINITY, f64::max);
    (x0, y0, x1 - x0, y1 - y0)
}

/// `screens` translated so a window sitting at `(window_x, window_y)` in
/// the same space `screens` are given in sees its own position as the
/// origin: each rect shifts by `(-window_x, -window_y)`, sizes unchanged.
pub fn screens_in_window(screens: &[WmScreen], window_x: f64, window_y: f64) -> Vec<WmScreen> {
    screens
        .iter()
        .map(|s| WmScreen {
            name: s.name.clone(),
            x: s.x - window_x,
            y: s.y - window_y,
            w: s.w,
            h: s.h,
            primary: s.primary,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(name: &str, x: f64, y: f64, w: f64, h: f64) -> WmScreen {
        WmScreen {
            name: name.into(),
            x,
            y,
            w,
            h,
            primary: false,
        }
    }

    /// Three same-height screens left to right: a (0..1920), b
    /// (1920..3840), c (3840..5760).
    fn three() -> Vec<WmScreen> {
        vec![
            screen("a", 0.0, 0.0, 1920.0, 1080.0),
            screen("b", 1920.0, 0.0, 1920.0, 1080.0),
            screen("c", 3840.0, 0.0, 1920.0, 1080.0),
        ]
    }

    #[test]
    fn adjacent_subset_resolves() {
        let screens = three();
        let span = ScreenSpan::Screens(vec!["b".into(), "c".into()]);
        let (range, rect) = span_rect(&screens, &span, None).unwrap();
        assert_eq!(range, 1..3);
        assert_eq!(rect, (1920.0, 0.0, 3840.0, 1080.0));
    }

    #[test]
    fn non_adjacent_subset_errors() {
        let screens = three();
        let span = ScreenSpan::Screens(vec!["a".into(), "c".into()]);
        assert_eq!(span_rect(&screens, &span, None), Err(SpanError::NotAdjacent));
    }

    #[test]
    fn unknown_name_errors() {
        let screens = three();
        let span = ScreenSpan::Screens(vec!["z".into()]);
        assert_eq!(
            span_rect(&screens, &span, None),
            Err(SpanError::UnknownScreen("z".into()))
        );
    }

    #[test]
    fn empty_screens_list_errors() {
        let screens = three();
        let span = ScreenSpan::Screens(vec![]);
        assert_eq!(span_rect(&screens, &span, None), Err(SpanError::Empty));
    }

    #[test]
    fn current_without_an_index_errors() {
        let screens = three();
        assert_eq!(
            span_rect(&screens, &ScreenSpan::Current, None),
            Err(SpanError::Empty)
        );
    }

    #[test]
    fn current_out_of_range_errors() {
        let screens = three();
        assert_eq!(
            span_rect(&screens, &ScreenSpan::Current, Some(9)),
            Err(SpanError::Empty)
        );
    }

    #[test]
    fn current_resolves_its_own_screen() {
        let screens = three();
        let (range, rect) = span_rect(&screens, &ScreenSpan::Current, Some(1)).unwrap();
        assert_eq!(range, 1..2);
        assert_eq!(rect, (1920.0, 0.0, 1920.0, 1080.0));
    }

    #[test]
    fn all_spans_every_screen() {
        let screens = three();
        let (range, rect) = span_rect(&screens, &ScreenSpan::All, None).unwrap();
        assert_eq!(range, 0..3);
        assert_eq!(rect, (0.0, 0.0, 5760.0, 1080.0));
    }

    #[test]
    fn all_with_no_screens_errors() {
        assert_eq!(span_rect(&[], &ScreenSpan::All, None), Err(SpanError::Empty));
    }

    #[test]
    fn mixed_heights_bounding_box_includes_dead_space() {
        let screens = vec![
            screen("a", 0.0, 0.0, 1920.0, 1080.0),
            screen("b", 1920.0, 200.0, 1280.0, 720.0),
        ];
        let span = ScreenSpan::Screens(vec!["a".into(), "b".into()]);
        let (range, rect) = span_rect(&screens, &span, None).unwrap();
        assert_eq!(range, 0..2);
        // Union: left/top from a, right from b's far edge, bottom is the
        // lower of the two far edges (a's, since b sits higher and ends
        // at 200+720=920 < 1080).
        assert_eq!(rect, (0.0, 0.0, 3200.0, 1080.0));
    }

    #[test]
    fn duplicate_names_tolerated() {
        let screens = three();
        let span = ScreenSpan::Screens(vec!["b".into(), "b".into(), "c".into()]);
        let (range, rect) = span_rect(&screens, &span, None).unwrap();
        assert_eq!(range, 1..3);
        assert_eq!(rect, (1920.0, 0.0, 3840.0, 1080.0));
    }

    #[test]
    fn screens_in_window_translates_to_window_local_coordinates() {
        let screens = three();
        let local = screens_in_window(&screens, 1920.0, 10.0);
        assert_eq!(local[0].x, -1920.0);
        assert_eq!(local[0].y, -10.0);
        assert_eq!(local[1].x, 0.0);
        assert_eq!(local[2].x, 1920.0);
        // Names, sizes and primary flag are untouched by translation.
        assert_eq!(local[1].name, "b");
        assert_eq!(local[1].w, 1920.0);
        assert_eq!(local[1].h, 1080.0);
    }

    #[test]
    fn screen_span_json_round_trip() {
        for span in [
            ScreenSpan::All,
            ScreenSpan::Current,
            ScreenSpan::Screens(vec!["a".into(), "b".into()]),
        ] {
            let json = span.serialize_json();
            let parsed = ScreenSpan::deserialize_json(&json).unwrap();
            assert_eq!(parsed, span);
        }
    }

    #[test]
    fn wm_screen_json_round_trip() {
        let s = screen("a", 1.5, 2.5, 3.5, 4.5);
        let json = s.serialize_json();
        let parsed = WmScreen::deserialize_json(&json).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn span_error_json_round_trip() {
        for err in [
            SpanError::Empty,
            SpanError::NotAdjacent,
            SpanError::UnknownScreen("x".into()),
        ] {
            let json = err.serialize_json();
            let parsed = SpanError::deserialize_json(&json).unwrap();
            assert_eq!(parsed, err);
        }
    }
}

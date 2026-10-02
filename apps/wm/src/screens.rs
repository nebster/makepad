//! Pure screen geometry for per-screen layouts on a wide Linux desktop.
//!
//! Nothing here touches a `Cx` or any platform type: every function takes
//! plain `LRect`s and primitives, so it unit-tests without a display and
//! without the GPU. The gate that decides whether per-screen behaviour runs
//! at all (`per_screen_enabled`) is a pure function for the same reason —
//! see `docs/research/2026-10-02-wm-per-screen-map.md` §4.
//!
//! Coordinate space: the same window-coordinate space as `MouseEvent.abs`
//! and the WM's own `LRect` (see `layout.rs`). On Linux direct this is the
//! wide desktop's window coordinates; callers must not feed this module
//! macOS/Windows `screens()` output, which is in OS display coordinates.

use crate::layout::LRect;

/// All of: running the Linux direct backend, not a mobile style, not the
/// gallery, and at least two screens published. Any single failing
/// condition means per-screen behaviour is off and the WM uses one
/// fallback entry covering the whole desk (today's behaviour).
pub fn per_screen_enabled(linux_direct: bool, mobile: bool, gallery: bool, screen_count: usize) -> bool {
    linux_direct && !mobile && !gallery && screen_count >= 2
}

/// The intersection of `screen` and `desk`; `None` if they don't overlap
/// (or touch only along an edge, which has zero area).
pub fn clip_to_desk(screen: LRect, desk: LRect) -> Option<LRect> {
    let x0 = screen.x.max(desk.x);
    let y0 = screen.y.max(desk.y);
    let x1 = (screen.x + screen.w).min(desk.x + desk.w);
    let y1 = (screen.y + screen.h).min(desk.y + desk.h);
    if x1 > x0 && y1 > y0 {
        Some(LRect::new(x0, y0, x1 - x0, y1 - y0))
    } else {
        None
    }
}

/// The per-screen rects the desk should use, left to right. When
/// `per_screen` is false, `names` and `geoms` disagree in length, or
/// clipping each screen to `desk` leaves fewer than two non-empty rects,
/// the whole desk is one fallback entry named `""` — exactly today's
/// single-screen behaviour (also what runs on macOS/Windows, where
/// `screens()` isn't desk coordinates, and in mobile/gallery styles).
/// Otherwise each surviving screen is `(name, clip_to_desk(geom, desk))`,
/// in the same left-to-right order as `names`/`geoms`; a screen whose clip
/// is empty (fully off the desk) is skipped.
pub fn screen_rects_for(per_screen: bool, names: &[String], geoms: &[LRect], desk: LRect) -> Vec<(String, LRect)> {
    let fallback = || vec![("".to_string(), desk)];
    if !per_screen || names.len() != geoms.len() {
        return fallback();
    }
    let clipped: Vec<(String, LRect)> = names
        .iter()
        .zip(geoms.iter())
        .filter_map(|(name, geom)| clip_to_desk(*geom, desk).map(|r| (name.clone(), r)))
        .collect();
    if clipped.len() < 2 {
        return fallback();
    }
    clipped
}

/// Squared Euclidean distance from `(x, y)` to the nearest point of `r`
/// (zero when the point is inside or on its boundary).
fn dist_sq_to_rect(x: f64, y: f64, r: &LRect) -> f64 {
    let dx = if x < r.x {
        r.x - x
    } else if x > r.x + r.w {
        x - (r.x + r.w)
    } else {
        0.0
    };
    let dy = if y < r.y {
        r.y - y
    } else if y > r.y + r.h {
        y - (r.y + r.h)
    } else {
        0.0
    };
    dx * dx + dy * dy
}

/// Which screen a point belongs to. Edges are inclusive, so a point on a
/// seam shared by two screens belongs to the first of them in `rects`
/// order. Outside every screen (e.g. the dead band under a shorter screen
/// on a wide desktop), the nearest screen by Euclidean distance wins.
/// `None` only when `rects` is empty.
pub fn screen_at(rects: &[LRect], x: f64, y: f64) -> Option<usize> {
    if rects.is_empty() {
        return None;
    }
    for (i, r) in rects.iter().enumerate() {
        if x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h {
            return Some(i);
        }
    }
    rects
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            dist_sq_to_rect(x, y, a)
                .partial_cmp(&dist_sq_to_rect(x, y, b))
                .unwrap()
        })
        .map(|(i, _)| i)
}

/// A screen's usable tiling area: `rect` with `reserved_bottom` (the dock,
/// per that screen) removed from its bottom edge, then inset by
/// `gaps_out` on every side.
pub fn screen_area(rect: LRect, reserved_bottom: f64, gaps_out: f64) -> LRect {
    let h = (rect.h - reserved_bottom).max(0.0);
    let w = (rect.w - gaps_out * 2.0).max(0.0);
    let h = (h - gaps_out * 2.0).max(0.0);
    LRect::new(rect.x + gaps_out, rect.y + gaps_out, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> String {
        s.to_string()
    }

    // --- per_screen_enabled ---------------------------------------------

    #[test]
    fn per_screen_enabled_needs_linux_direct() {
        assert!(!per_screen_enabled(false, false, false, 2));
    }

    #[test]
    fn per_screen_enabled_off_for_mobile() {
        assert!(!per_screen_enabled(true, true, false, 2));
    }

    #[test]
    fn per_screen_enabled_off_for_gallery() {
        assert!(!per_screen_enabled(true, false, true, 2));
    }

    #[test]
    fn per_screen_enabled_needs_two_screens() {
        assert!(!per_screen_enabled(true, false, false, 1));
    }

    #[test]
    fn per_screen_enabled_true_when_all_hold() {
        assert!(per_screen_enabled(true, false, false, 2));
    }

    // --- screen_rects_for: fallback cases --------------------------------

    #[test]
    fn screen_rects_for_off_falls_back_to_desk() {
        let desk = LRect::new(0.0, 0.0, 3840.0, 1080.0);
        let names = vec![n("A"), n("B")];
        let geoms = vec![
            LRect::new(0.0, 0.0, 1920.0, 1080.0),
            LRect::new(1920.0, 0.0, 1920.0, 1080.0),
        ];
        let got = screen_rects_for(false, &names, &geoms, desk);
        assert_eq!(got, vec![(n(""), desk)]);
    }

    #[test]
    fn screen_rects_for_mismatched_lengths_falls_back_to_desk() {
        let desk = LRect::new(0.0, 0.0, 3840.0, 1080.0);
        let names = vec![n("A"), n("B")];
        let geoms = vec![LRect::new(0.0, 0.0, 1920.0, 1080.0)];
        let got = screen_rects_for(true, &names, &geoms, desk);
        assert_eq!(got, vec![(n(""), desk)]);
    }

    #[test]
    fn screen_rects_for_single_surviving_screen_falls_back_to_desk() {
        // One geom entirely off the desk clips to empty, leaving only one
        // non-empty rect -- fewer than two, so it falls back.
        let desk = LRect::new(0.0, 0.0, 1920.0, 1080.0);
        let names = vec![n("A"), n("B")];
        let geoms = vec![
            LRect::new(0.0, 0.0, 1920.0, 1080.0),
            LRect::new(5000.0, 0.0, 1920.0, 1080.0),
        ];
        let got = screen_rects_for(true, &names, &geoms, desk);
        assert_eq!(got, vec![(n(""), desk)]);
    }

    // --- screen_rects_for: the two-screen wide-desktop cases -------------

    #[test]
    fn two_equal_screens_clip_below_the_bar() {
        // Bar strip on top: desk starts at y=26.
        let desk = LRect::new(0.0, 26.0, 3840.0, 1080.0 - 26.0);
        let names = vec![n("DP-1"), n("DP-2")];
        let geoms = vec![
            LRect::new(0.0, 0.0, 1920.0, 1080.0),
            LRect::new(1920.0, 0.0, 1920.0, 1080.0),
        ];
        let got = screen_rects_for(true, &names, &geoms, desk);
        assert_eq!(
            got,
            vec![
                (n("DP-1"), LRect::new(0.0, 26.0, 1920.0, 1080.0 - 26.0)),
                (n("DP-2"), LRect::new(1920.0, 26.0, 1920.0, 1080.0 - 26.0)),
            ]
        );
    }

    #[test]
    fn shorter_right_screen_has_no_dead_band() {
        let desk = LRect::new(0.0, 0.0, 3840.0, 1080.0);
        let names = vec![n("DP-1"), n("DP-2")];
        let geoms = vec![
            LRect::new(0.0, 0.0, 1920.0, 1080.0),
            LRect::new(1920.0, 0.0, 1920.0, 720.0),
        ];
        let got = screen_rects_for(true, &names, &geoms, desk);
        assert_eq!(
            got,
            vec![
                (n("DP-1"), LRect::new(0.0, 0.0, 1920.0, 1080.0)),
                (n("DP-2"), LRect::new(1920.0, 0.0, 1920.0, 720.0)),
            ]
        );
    }

    #[test]
    fn ai_pane_clips_the_left_screen_start() {
        // AI pane open: desk starts at x=400.
        let desk = LRect::new(400.0, 0.0, 3440.0, 1080.0);
        let names = vec![n("DP-1"), n("DP-2")];
        let geoms = vec![
            LRect::new(0.0, 0.0, 1920.0, 1080.0),
            LRect::new(1920.0, 0.0, 1920.0, 1080.0),
        ];
        let got = screen_rects_for(true, &names, &geoms, desk);
        assert_eq!(
            got,
            vec![
                (n("DP-1"), LRect::new(400.0, 0.0, 1920.0 - 400.0, 1080.0)),
                (n("DP-2"), LRect::new(1920.0, 0.0, 1920.0, 1080.0)),
            ]
        );
    }

    // --- screen_at --------------------------------------------------------

    #[test]
    fn screen_at_seam_belongs_to_the_first_screen() {
        let rects = vec![
            LRect::new(0.0, 0.0, 1920.0, 1080.0),
            LRect::new(1920.0, 0.0, 1920.0, 720.0),
        ];
        // x = 1920 is the right edge of screen 0 and the left edge of
        // screen 1 -- the first in order wins.
        assert_eq!(screen_at(&rects, 1920.0, 100.0), Some(0));
    }

    #[test]
    fn screen_at_dead_band_picks_the_nearest_screen() {
        let rects = vec![
            LRect::new(0.0, 0.0, 1920.0, 1080.0),
            LRect::new(1920.0, 0.0, 1920.0, 720.0),
        ];
        // Below the shorter right screen's bottom edge, still within its
        // x-range: outside both rects, nearest is screen 1.
        assert_eq!(screen_at(&rects, 2500.0, 900.0), Some(1));
    }

    #[test]
    fn screen_at_empty_is_none() {
        let rects: Vec<LRect> = vec![];
        assert_eq!(screen_at(&rects, 0.0, 0.0), None);
    }

    // --- screen_area --------------------------------------------------------

    #[test]
    fn screen_area_applies_reserved_bottom_and_gaps() {
        let rect = LRect::new(0.0, 26.0, 1920.0, 1054.0);
        let got = screen_area(rect, 60.0, 10.0);
        // Height loses 60 off the bottom, then 10 off every side.
        assert_eq!(
            got,
            LRect::new(10.0, 36.0, 1920.0 - 20.0, 1054.0 - 60.0 - 20.0)
        );
    }
}

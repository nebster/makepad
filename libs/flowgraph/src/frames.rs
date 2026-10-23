//! Group-frame geometry: pure rectangle rules shared by the canvas and its
//! hosts. All rects are in graph-world units.

use makepad_widgets_core::*;

/// The height of a frame's header band, where its host draws the title row.
pub const FRAME_HEADER_H: f64 = 56.0;
/// A corner's hit square, in world units at zoom 1 (divided by the camera
/// scale when zoomed out, as the card resize grip is).
pub const FRAME_GRIP: f64 = 22.0;
/// The smallest a frame may be resized to.
pub const MIN_FRAME: DVec2 = dvec2(240.0, 176.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Open intervals `a0..a1` and `b0..b1` share interior.
fn spans_overlap(a0: f64, a1: f64, b0: f64, b1: f64) -> bool {
    a0 < b1 && b0 < a1
}

/// The interiors of `a` and `b` intersect (rects that only touch do not).
pub fn overlaps(a: Rect, b: Rect) -> bool {
    spans_overlap(a.pos.x, a.pos.x + a.size.x, b.pos.x, b.pos.x + b.size.x)
        && spans_overlap(a.pos.y, a.pos.y + a.size.y, b.pos.y, b.pos.y + b.size.y)
}

/// The card's centre is inside the frame; a centre on an edge is inside.
pub fn holds(frame: Rect, card: Rect) -> bool {
    let c = card.pos + card.size / 2.0;
    c.x >= frame.pos.x
        && c.x <= frame.pos.x + frame.size.x
        && c.y >= frame.pos.y
        && c.y <= frame.pos.y + frame.size.y
}

/// The corner whose `grip` × `grip` square, inside the frame, holds `point`.
pub fn corner_at(frame: Rect, point: DVec2, grip: f64) -> Option<Corner> {
    let left = point.x >= frame.pos.x && point.x <= frame.pos.x + grip;
    let right = point.x <= frame.pos.x + frame.size.x && point.x >= frame.pos.x + frame.size.x - grip;
    let top = point.y >= frame.pos.y && point.y <= frame.pos.y + grip;
    let bottom = point.y <= frame.pos.y + frame.size.y && point.y >= frame.pos.y + frame.size.y - grip;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(Corner::TopLeft),
        (_, true, true, _) => Some(Corner::TopRight),
        (true, _, _, true) => Some(Corner::BottomLeft),
        (_, true, _, true) => Some(Corner::BottomRight),
        _ => None,
    }
}

/// A move of `delta` along one axis of the span `lo..hi`, stopped at the
/// nearest facing edge `others` (as `(near, far)` spans on that axis) put in
/// its way. Only spans wholly on the side of travel block it.
fn clamp_axis(lo: f64, hi: f64, delta: f64, others: impl Iterator<Item = (f64, f64)>) -> f64 {
    let mut delta = delta;
    for (o0, o1) in others {
        if delta > 0.0 && o0 >= hi && o0 < hi + delta {
            delta = o0 - hi;
        } else if delta < 0.0 && o1 <= lo && o1 > lo + delta {
            delta = o1 - lo;
        }
    }
    delta
}

/// The delta `rect` may move toward `delta` without its interior overlapping any of `others`: x clamped first (to the nearest blocking edge in the direction of travel), then y. A rect already overlapping another is not pushed.
pub fn clamp_move(rect: Rect, delta: DVec2, others: &[Rect]) -> DVec2 {
    if others.iter().any(|other| overlaps(rect, *other)) {
        return delta;
    }
    let (x0, y0) = (rect.pos.x, rect.pos.y);
    let (x1, y1) = (x0 + rect.size.x, y0 + rect.size.y);
    let dx = clamp_axis(
        x0,
        x1,
        delta.x,
        others
            .iter()
            .filter(|o| spans_overlap(y0, y1, o.pos.y, o.pos.y + o.size.y))
            .map(|o| (o.pos.x, o.pos.x + o.size.x)),
    );
    let dy = clamp_axis(
        y0,
        y1,
        delta.y,
        others
            .iter()
            .filter(|o| spans_overlap(x0 + dx, x1 + dx, o.pos.x, o.pos.x + o.size.x))
            .map(|o| (o.pos.y, o.pos.y + o.size.y)),
    );
    dvec2(dx, dy)
}

/// `rect` with `corner` dragged by `delta`: no side under `min`, and each moving side stopped at the nearest other frame's edge it would cross.
pub fn clamp_resize(rect: Rect, corner: Corner, delta: DVec2, min: DVec2, others: &[Rect]) -> Rect {
    let (x0, y0) = (rect.pos.x, rect.pos.y);
    let (x1, y1) = (x0 + rect.size.x, y0 + rect.size.y);
    let left = matches!(corner, Corner::TopLeft | Corner::BottomLeft);
    let top = matches!(corner, Corner::TopLeft | Corner::TopRight);
    // The moving edges take the delta and give way to the minimum.
    let (mut nx0, mut nx1) = if left { ((x0 + delta.x).min(x1 - min.x), x1) } else { (x0, (x1 + delta.x).max(x0 + min.x)) };
    let (mut ny0, mut ny1) = if top { ((y0 + delta.y).min(y1 - min.y), y1) } else { (y0, (y1 + delta.y).max(y0 + min.y)) };
    let (gx0, gx1, gy0, gy1) = (nx0, nx1, ny0, ny1);
    for o in others {
        let (o0x, o1x) = (o.pos.x, o.pos.x + o.size.x);
        let (o0y, o1y) = (o.pos.y, o.pos.y + o.size.y);
        if spans_overlap(gy0, gy1, o0y, o1y) {
            if left && o1x <= x0 && o1x > nx0 {
                nx0 = o1x;
            } else if !left && o0x >= x1 && o0x < nx1 {
                nx1 = o0x;
            }
        }
        if spans_overlap(gx0, gx1, o0x, o1x) {
            if top && o1y <= y0 && o1y > ny0 {
                ny0 = o1y;
            } else if !top && o0y >= y1 && o0y < ny1 {
                ny1 = o0y;
            }
        }
    }
    Rect {
        pos: dvec2(nx0, ny0),
        size: dvec2(nx1 - nx0, ny1 - ny0),
    }
}

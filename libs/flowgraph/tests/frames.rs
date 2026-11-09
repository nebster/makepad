use makepad_flowgraph::frames::*;
use makepad_widgets_core::*;

fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect {
        pos: dvec2(x, y),
        size: dvec2(w, h),
    }
}

#[test]
fn overlaps_is_interior_only() {
    assert!(!overlaps(r(0.0, 0.0, 100.0, 100.0), r(100.0, 0.0, 100.0, 100.0)));
    assert!(overlaps(r(0.0, 0.0, 100.0, 100.0), r(99.0, 0.0, 100.0, 100.0)));
    assert!(overlaps(r(0.0, 0.0, 100.0, 100.0), r(20.0, 20.0, 10.0, 10.0)));
    assert!(overlaps(r(20.0, 20.0, 10.0, 10.0), r(0.0, 0.0, 100.0, 100.0)));
}

#[test]
fn holds_is_the_centre_rule() {
    let frame = r(0.0, 0.0, 400.0, 300.0);
    assert!(holds(frame, r(100.0, 100.0, 200.0, 100.0)));
    assert!(!holds(frame, r(350.0, 250.0, 300.0, 160.0)));
    // Centre (400, 150) sits exactly on the right edge.
    assert!(holds(frame, r(300.0, 100.0, 200.0, 100.0)));
}

#[test]
fn corner_at_finds_each_grip() {
    let frame = r(0.0, 0.0, 400.0, 300.0);
    assert_eq!(corner_at(frame, dvec2(5.0, 5.0), 22.0), Some(Corner::TopLeft));
    assert_eq!(corner_at(frame, dvec2(395.0, 295.0), 22.0), Some(Corner::BottomRight));
    assert_eq!(corner_at(frame, dvec2(395.0, 5.0), 22.0), Some(Corner::TopRight));
    assert_eq!(corner_at(frame, dvec2(5.0, 295.0), 22.0), Some(Corner::BottomLeft));
    assert_eq!(corner_at(frame, dvec2(200.0, 150.0), 22.0), None);
    assert_eq!(corner_at(frame, dvec2(30.0, 30.0), 22.0), None);
}

#[test]
fn a_move_stops_at_another_frames_edge() {
    let rect = r(0.0, 0.0, 400.0, 300.0);
    let others = [r(500.0, 0.0, 400.0, 300.0)];
    assert_eq!(clamp_move(rect, dvec2(300.0, 0.0), &others), dvec2(100.0, 0.0));
    assert_eq!(clamp_move(rect, dvec2(-300.0, 0.0), &others), dvec2(-300.0, 0.0));
    assert_eq!(clamp_move(rect, dvec2(300.0, 350.0), &others), dvec2(100.0, 350.0));
}

#[test]
fn a_move_slides_along_a_side() {
    let rect = r(0.0, 0.0, 400.0, 300.0);
    let others = [r(0.0, 300.0, 400.0, 300.0)];
    assert_eq!(clamp_move(rect, dvec2(50.0, 400.0), &others), dvec2(50.0, 0.0));
}

#[test]
fn an_already_overlapping_frame_is_not_pushed() {
    let rect = r(0.0, 0.0, 400.0, 300.0);
    let others = [r(100.0, 100.0, 400.0, 300.0)];
    assert_eq!(clamp_move(rect, dvec2(10.0, 10.0), &others), dvec2(10.0, 10.0));
}

#[test]
fn a_resize_keeps_the_minimum_and_stops_at_a_neighbour() {
    let rect = r(0.0, 0.0, 400.0, 300.0);
    assert_eq!(
        clamp_resize(rect, Corner::BottomRight, dvec2(-300.0, -300.0), dvec2(240.0, 176.0), &[]),
        r(0.0, 0.0, 240.0, 176.0)
    );
    assert_eq!(
        clamp_resize(rect, Corner::BottomRight, dvec2(300.0, 0.0), MIN_FRAME, &[r(500.0, 0.0, 400.0, 300.0)]),
        r(0.0, 0.0, 500.0, 300.0)
    );
    assert_eq!(
        clamp_resize(rect, Corner::TopLeft, dvec2(100.0, 100.0), MIN_FRAME, &[]),
        r(100.0, 100.0, 300.0, 200.0)
    );
    assert_eq!(
        clamp_resize(rect, Corner::TopLeft, dvec2(-100.0, -100.0), MIN_FRAME, &[r(-500.0, -500.0, 450.0, 450.0)]),
        r(-50.0, -50.0, 450.0, 350.0)
    );
}

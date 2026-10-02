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

use std::collections::HashMap;

use crate::layout::{fit_inside, transfer_client, ClientId, LRect, WmLayout, SCRATCHPAD};

/// How long (seconds) a screen name must stay missing before its windows
/// migrate (map §6.3 rule 5): mode changes and `active=false` flaps must
/// not scatter windows.
pub const REMOVAL_DEBOUNCE: f64 = 2.0;

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

/// One physical screen and the layout that tiles it.
pub struct ScreenLayout {
    /// Connector name (`""` for the single fallback entry).
    pub name: String,
    /// Desk-clipped screen rect, before gaps and the dock reservation.
    pub rect: LRect,
    pub layout: WmLayout,
}

/// One `WmLayout` per screen, left to right in `screens()` order. Screens
/// waiting out the removal debounce stay at the END of `screens` (after
/// every live screen) until they migrate or come back.
pub struct ScreenSet {
    pub screens: Vec<ScreenLayout>,
    /// The pointer / explicit-focus screen: new windows, menus and
    /// workspace keys act here. Always a live screen.
    pub active: usize,
    /// The primary screen (dock), from `main_name`, else 0.
    pub main: usize,
    /// In-session: the connector a migrated window came from, so it goes
    /// back when that screen returns.
    pub homes: HashMap<ClientId, String>,
    /// Screen name and the time it was first seen missing.
    pending_removal: Vec<(String, f64)>,
}

fn contains_rect(outer: LRect, inner: LRect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.x + outer.w >= inner.x + inner.w
        && outer.y + outer.h >= inner.y + inner.h
}

/// How far a window moves when it goes from screen `src` to `dst`: not at
/// all when `dst` covers `src` (a merge into the whole desk keeps
/// positions), otherwise by the origin delta (same place on the new screen).
fn carry_offset(src: LRect, dst: LRect) -> (f64, f64) {
    if contains_rect(dst, src) {
        (0.0, 0.0)
    } else {
        (dst.x - src.x, dst.y - src.y)
    }
}

/// `a` and `b` (distinct) of one slice, both mutably.
fn two_mut<T>(v: &mut [T], a: usize, b: usize) -> (&mut T, &mut T) {
    assert_ne!(a, b);
    if a < b {
        let (l, r) = v.split_at_mut(b);
        (&mut l[a], &mut r[0])
    } else {
        let (l, r) = v.split_at_mut(a);
        (&mut r[0], &mut l[b])
    }
}

/// Put `c` (detached from `src`, with its float rect and desktop-style
/// window) on `dst`'s workspace `ws`, shifting and fitting its rects.
fn place(
    src_rect: LRect,
    dst: &mut ScreenLayout,
    ws: usize,
    c: ClientId,
    float: Option<LRect>,
    desk: Option<crate::desktop_layout::DesktopWindow>,
    area: LRect,
    gap: f64,
) {
    let off = carry_offset(src_rect, dst.rect);
    let carry = |r: LRect| fit_inside(LRect::new(r.x + off.0, r.y + off.1, r.w, r.h), area);
    dst.layout.adopt(ws, c, float.map(carry), area, gap);
    if let Some(mut w) = desk {
        w.rect = carry(w.rect);
        dst.layout.desktop.windows.retain(|x| x.client != c);
        dst.layout.desktop.windows.push(w);
    }
}

impl ScreenSet {
    /// Today's single-desk WM: one fallback entry named `""` over `desk`.
    pub fn new(layout: WmLayout, desk: LRect) -> Self {
        Self {
            screens: vec![ScreenLayout { name: String::new(), rect: desk, layout }],
            active: 0,
            main: 0,
            homes: HashMap::new(),
            pending_removal: Vec::new(),
        }
    }

    pub fn active_layout(&self) -> &WmLayout {
        &self.screens[self.active].layout
    }

    pub fn active_layout_mut(&mut self) -> &mut WmLayout {
        &mut self.screens[self.active].layout
    }

    /// Which screen's layout holds `c`.
    pub fn screen_of(&self, c: ClientId) -> Option<usize> {
        self.screens.iter().position(|s| s.layout.workspace_of(c).is_some())
    }

    /// The active screen's focused client.
    pub fn focused_client(&self) -> Option<ClientId> {
        self.active_layout().focused_client()
    }

    pub fn all_clients(&self) -> Vec<ClientId> {
        self.screens.iter().flat_map(|s| s.layout.all_clients()).collect()
    }

    /// Every screen's rect, index-aligned with `screens` (a screen waiting
    /// out the removal debounce included, at the end).
    pub fn rects(&self) -> Vec<LRect> {
        self.screens.iter().map(|s| s.rect).collect()
    }

    fn is_pending(&self, name: &str) -> bool {
        self.pending_removal.iter().any(|(n, _)| n == name)
    }

    /// Indices of the screens that are really there (not waiting out the
    /// removal debounce), left to right.
    fn live(&self) -> Vec<usize> {
        (0..self.screens.len()).filter(|&i| !self.is_pending(&self.screens[i].name)).collect()
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.screens.iter().position(|s| s.name == name)
    }

    /// A kept screen got `rect`: shift its floats by the move, keep them
    /// reachable, and hand the layout its new outer rect.
    fn set_rect(s: &mut ScreenLayout, rect: LRect, reserved_bottom: f64, gaps_out: f64) {
        if s.rect != rect {
            s.layout.translate(rect.x - s.rect.x, rect.y - s.rect.y);
            s.layout.fit_floats(screen_area(rect, reserved_bottom, gaps_out));
        }
        s.layout.set_outer(rect);
        s.rect = rect;
    }

    /// Every client of screen `src` goes to `dst` on the same workspace
    /// number (scratchpad included), tiles re-inserted in focus order, each
    /// recorded in `homes` (an earlier home wins). `dst` keeps the focus it
    /// had on a workspace that already had one.
    fn migrate_all(&mut self, src: usize, dst: usize, gap: f64, reserved_bottom: f64, gaps_out: f64) {
        let src_name = self.screens[src].name.clone();
        let (s, d) = two_mut(&mut self.screens, src, dst);
        let area = screen_area(d.rect, reserved_bottom, gaps_out);
        for ws in 0..=SCRATCHPAD {
            let desks: Vec<_> = s
                .layout
                .clients_on(ws)
                .into_iter()
                .filter_map(|c| s.layout.desktop.get(c).cloned())
                .collect();
            let taken = s.layout.take_workspace_clients(ws);
            if taken.is_empty() {
                continue;
            }
            let prior = d.layout.workspaces[ws].focus;
            for (c, float) in taken {
                let desk = desks.iter().find(|w| w.client == c).cloned();
                place(s.rect, d, ws, c, float, desk, area, gap);
                if !src_name.is_empty() {
                    self.homes.entry(c).or_insert_with(|| src_name.clone());
                }
            }
            if prior.is_some() {
                d.layout.workspaces[ws].focus = prior;
            }
        }
    }

    /// Clients whose home is screen `dst` go back to it from wherever they
    /// are now (same workspace number) and leave `homes`.
    fn return_home(&mut self, dst: usize, gap: f64, reserved_bottom: f64, gaps_out: f64) {
        let name = self.screens[dst].name.clone();
        let mut back: Vec<ClientId> =
            self.homes.iter().filter(|(_, h)| **h == name).map(|(c, _)| *c).collect();
        back.sort();
        for c in back {
            self.homes.remove(&c);
            let Some(src) = self.screen_of(c) else { continue };
            if src == dst {
                continue;
            }
            let (s, d) = two_mut(&mut self.screens, src, dst);
            let desk = s.layout.desktop.get(c).cloned();
            let Some((ws, float)) = s.layout.detach(c) else { continue };
            let area = screen_area(d.rect, reserved_bottom, gaps_out);
            place(s.rect, d, ws, c, float, desk, area, gap);
        }
    }

    /// Bring the set in line with the screens now published (`new`, left to
    /// right, from `screen_rects_for`). `now` is in seconds. See map §6.3:
    /// - same name: layout kept, floats moved with the screen and fitted;
    /// - the old single `""` entry is renamed to `main_name` (else the
    ///   first new name), never migrated;
    /// - back to the single `""` entry: every other screen merges into the
    ///   survivor (the main screen) at once, same workspace numbers;
    /// - added name: an empty layout, then its `homes` clients come back;
    /// - removed name: kept (at the end) until missing for
    ///   `REMOVAL_DEBOUNCE`, then migrated to the main screen (else the
    ///   active one) on the same workspace numbers, recorded in `homes`.
    /// `active` stays on its screen by name (the main screen if that one
    /// went), `main` is `main_name`'s index, else 0.
    pub fn reconcile(
        &mut self,
        new: &[(String, LRect)],
        main_name: Option<&str>,
        now: f64,
        gap: f64,
        reserved_bottom: f64,
        gaps_out: f64,
    ) {
        if new.is_empty() {
            return;
        }
        let main_name = main_name.filter(|m| new.iter().any(|(n, _)| n == m));
        let active_name = self.screens.get(self.active).map(|s| s.name.clone());

        if new.len() == 1 && new[0].0.is_empty() {
            // Back to one desk: merge everything into the survivor.
            let keep = self
                .index_of("")
                .unwrap_or_else(|| self.main.min(self.screens.len() - 1));
            Self::set_rect(&mut self.screens[keep], new[0].1, reserved_bottom, gaps_out);
            for i in (0..self.screens.len()).rev() {
                if i != keep {
                    self.migrate_all(i, keep, gap, reserved_bottom, gaps_out);
                }
            }
            let survivor = self.screens.swap_remove(keep);
            self.screens.clear();
            self.screens.push(ScreenLayout { name: String::new(), ..survivor });
            self.pending_removal.clear();
            self.active = 0;
            self.main = 0;
            self.prune_homes();
            return;
        }

        if self.screens.len() == 1 && self.screens[0].name.is_empty() {
            // The fallback becomes the main screen; windows stay.
            self.screens[0].name = main_name.unwrap_or(&new[0].0).to_string();
        }

        let mut old = std::mem::take(&mut self.screens);
        let mut added = Vec::new();
        for (name, rect) in new {
            if let Some(i) = old.iter().position(|s| &s.name == name) {
                let mut s = old.remove(i);
                Self::set_rect(&mut s, *rect, reserved_bottom, gaps_out);
                self.screens.push(s);
            } else {
                let mut layout = WmLayout::new();
                layout.set_outer(*rect);
                self.screens.push(ScreenLayout { name: name.clone(), rect: *rect, layout });
                added.push(self.screens.len() - 1);
            }
        }
        // What is left in `old` is missing: start (or keep) its debounce.
        self.pending_removal.retain(|(n, _)| old.iter().any(|s| &s.name == n));
        for s in &old {
            if !self.pending_removal.iter().any(|(n, _)| *n == s.name) {
                self.pending_removal.push((s.name.clone(), now));
            }
        }
        self.screens.extend(old);

        self.main = main_name.and_then(|m| self.index_of(m)).unwrap_or(0);
        self.active = active_name
            .and_then(|n| self.index_of(&n))
            .filter(|&i| i < new.len())
            .unwrap_or(self.main);

        let expired: Vec<String> = self
            .pending_removal
            .iter()
            .filter(|(_, t)| now - *t >= REMOVAL_DEBOUNCE)
            .map(|(n, _)| n.clone())
            .collect();
        for name in expired {
            self.pending_removal.retain(|(n, _)| *n != name);
            let Some(src) = self.index_of(&name) else { continue };
            // Pending screens sit after every live one, so `dst < src`.
            let dst = if main_name.is_some() { self.main } else { self.active };
            self.migrate_all(src, dst, gap, reserved_bottom, gaps_out);
            self.screens.remove(src);
        }

        for dst in added {
            self.return_home(dst, gap, reserved_bottom, gaps_out);
        }
        self.prune_homes();
        if self.active >= self.screens.len() {
            self.active = self.main.min(self.screens.len() - 1);
        }
    }

    /// Forget homes of windows that closed.
    fn prune_homes(&mut self) {
        let all = self.all_clients();
        self.homes.retain(|c, _| all.contains(c));
    }

    /// The pointer is at (x, y): `Some(screen)` when it crossed into a
    /// different live screen (which becomes active), `None` otherwise.
    pub fn on_pointer(&mut self, x: f64, y: f64) -> Option<usize> {
        let live = self.live();
        let rects: Vec<LRect> = live.iter().map(|&i| self.screens[i].rect).collect();
        let i = live[screen_at(&rects, x, y)?];
        if i == self.active {
            return None;
        }
        self.active = i;
        Some(i)
    }

    /// Move `c` to the next (or previous) live screen, wrapping, onto that
    /// screen's active workspace; the target becomes active and `c` its
    /// focus. `None` with one screen or when `c` is unknown.
    pub fn move_to_screen(
        &mut self,
        c: ClientId,
        forward: bool,
        gap: f64,
        reserved_bottom: f64,
        gaps_out: f64,
    ) -> Option<usize> {
        let live = self.live();
        if live.len() < 2 {
            return None;
        }
        let from = self.screen_of(c)?;
        let pos = live.iter().position(|&i| i == from).unwrap_or(0);
        let n = live.len();
        let to = live[if forward { (pos + 1) % n } else { (pos + n - 1) % n }];
        if to == from {
            return None;
        }
        let (s, d) = two_mut(&mut self.screens, from, to);
        let offset = (d.rect.x - s.rect.x, d.rect.y - s.rect.y);
        let area = screen_area(d.rect, reserved_bottom, gaps_out);
        if !transfer_client(&mut s.layout, &mut d.layout, c, offset, area, gap) {
            return None;
        }
        self.homes.remove(&c);
        self.active = to;
        Some(to)
    }
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
    // --- ScreenSet ----------------------------------------------------------

    const GAP: f64 = 0.0;
    const RB: f64 = 0.0;
    const GO: f64 = 0.0;
    const DESK: LRect = LRect { x: 0.0, y: 0.0, w: 3840.0, h: 1080.0 };
    const RA: LRect = LRect { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 };
    const RB_: LRect = LRect { x: 1920.0, y: 0.0, w: 1920.0, h: 1080.0 };

    fn ab() -> Vec<(String, LRect)> {
        vec![(n("A"), RA), (n("B"), RB_)]
    }

    fn sorted(mut v: Vec<ClientId>) -> Vec<ClientId> {
        v.sort();
        v
    }

    /// Fallback "" layout: 1, 2 tiled on workspace 0, float 3 on ws 0.
    fn fallback_set() -> ScreenSet {
        let mut l = WmLayout::new();
        let area = screen_area(DESK, RB, GO);
        l.insert(1, area, GAP);
        l.insert(2, area, GAP);
        l.add_float(3, LRect::new(100.0, 100.0, 400.0, 300.0), 0);
        ScreenSet::new(l, DESK)
    }

    /// [A, B] with B holding 10, 11 tiled on ws 0, 12 tiled on ws 3,
    /// float 13 on ws 0 and 14 on the scratchpad; A holds 1 on ws 0.
    fn two_screens() -> ScreenSet {
        let mut set = ScreenSet::new(WmLayout::new(), DESK);
        set.reconcile(&ab(), Some("A"), 0.0, GAP, RB, GO);
        let aa = screen_area(RA, RB, GO);
        let ba = screen_area(RB_, RB, GO);
        set.screens[0].layout.insert(1, aa, GAP);
        let b = &mut set.screens[1].layout;
        b.insert_on(0, 10, ba, GAP);
        b.insert_on(0, 11, ba, GAP);
        b.insert_on(3, 12, ba, GAP);
        b.add_float(13, LRect::new(2000.0, 100.0, 400.0, 300.0), 0);
        b.insert_on(SCRATCHPAD, 14, ba, GAP);
        set
    }

    #[test]
    fn new_is_one_fallback_entry() {
        let set = fallback_set();
        assert_eq!(set.screens.len(), 1);
        assert_eq!(set.screens[0].name, "");
        assert_eq!(set.rects(), vec![DESK]);
        assert_eq!(set.active, 0);
        assert_eq!(sorted(set.all_clients()), vec![1, 2, 3]);
    }

    #[test]
    fn fallback_is_renamed_to_main_and_keeps_its_windows() {
        let mut set = fallback_set();
        set.reconcile(&ab(), Some("A"), 0.0, GAP, RB, GO);
        assert_eq!(set.screens.len(), 2);
        assert_eq!(set.screens[0].name, "A");
        assert_eq!(set.screens[1].name, "B");
        assert_eq!(set.rects(), vec![RA, RB_]);
        assert_eq!(set.main, 0);
        assert_eq!(sorted(set.screens[0].layout.all_clients()), vec![1, 2, 3]);
        assert!(set.screens[1].layout.all_clients().is_empty());
        assert_eq!(set.screen_of(3), Some(0));
        assert!(set.homes.is_empty());
    }

    #[test]
    fn fallback_is_renamed_to_a_main_on_the_right() {
        let mut set = fallback_set();
        set.reconcile(&ab(), Some("B"), 0.0, GAP, RB, GO);
        assert_eq!(set.main, 1);
        assert_eq!(sorted(set.screens[1].layout.all_clients()), vec![1, 2, 3]);
        assert!(set.screens[0].layout.all_clients().is_empty());
    }

    #[test]
    fn fallback_without_main_goes_to_the_first_screen() {
        let mut set = fallback_set();
        set.reconcile(&ab(), None, 0.0, GAP, RB, GO);
        assert_eq!(set.main, 0);
        assert_eq!(sorted(set.screens[0].layout.all_clients()), vec![1, 2, 3]);
    }

    #[test]
    fn removal_waits_for_the_debounce() {
        let mut set = two_screens();
        set.reconcile(&[(n("A"), RA)], Some("A"), 10.0, GAP, RB, GO);
        set.reconcile(&[(n("A"), RA)], Some("A"), 11.9, GAP, RB, GO);
        assert_eq!(set.screens.len(), 2);
        assert_eq!(set.screens[1].name, "B");
        assert_eq!(sorted(set.screens[1].layout.all_clients()), vec![10, 11, 12, 13, 14]);
        assert_eq!(set.screens[0].layout.all_clients(), vec![1]);
        assert!(set.homes.is_empty());
    }

    #[test]
    fn a_flap_shorter_than_the_debounce_changes_nothing() {
        let mut set = two_screens();
        set.reconcile(&[(n("A"), RA)], Some("A"), 10.0, GAP, RB, GO);
        set.reconcile(&ab(), Some("A"), 11.0, GAP, RB, GO);
        // Gone again later: the debounce starts over.
        set.reconcile(&[(n("A"), RA)], Some("A"), 12.5, GAP, RB, GO);
        assert_eq!(set.screens.len(), 2);
        assert_eq!(sorted(set.screens[1].layout.all_clients()), vec![10, 11, 12, 13, 14]);
    }

    #[test]
    fn removal_after_the_debounce_migrates_to_main_on_the_same_workspaces() {
        let mut set = two_screens();
        set.reconcile(&[(n("A"), RA)], Some("A"), 10.0, GAP, RB, GO);
        set.reconcile(&[(n("A"), RA)], Some("A"), 12.0, GAP, RB, GO);
        assert_eq!(set.screens.len(), 1);
        assert_eq!(set.screens[0].name, "A");
        let a = &set.screens[0].layout;
        assert_eq!(sorted(a.clients_on(0)), vec![1, 10, 11, 13]);
        assert_eq!(a.clients_on(3), vec![12]);
        assert_eq!(a.clients_on(SCRATCHPAD), vec![14]);
        // The float moved with its screen (B's origin to A's) and stayed whole.
        assert_eq!(a.float_rect(13), Some(LRect::new(80.0, 100.0, 400.0, 300.0)));
        for c in [10, 11, 12, 13, 14] {
            assert_eq!(set.homes.get(&c).map(|s| s.as_str()), Some("B"));
        }
        assert!(set.homes.get(&1).is_none());
    }

    #[test]
    fn a_returning_screen_takes_its_windows_back() {
        let mut set = two_screens();
        set.reconcile(&[(n("A"), RA)], Some("A"), 10.0, GAP, RB, GO);
        set.reconcile(&[(n("A"), RA)], Some("A"), 12.0, GAP, RB, GO);
        set.reconcile(&ab(), Some("A"), 20.0, GAP, RB, GO);
        assert_eq!(set.screens.len(), 2);
        assert_eq!(set.screens[0].layout.all_clients(), vec![1]);
        let b = &set.screens[1].layout;
        assert_eq!(sorted(b.clients_on(0)), vec![10, 11, 13]);
        assert_eq!(b.clients_on(3), vec![12]);
        assert_eq!(b.clients_on(SCRATCHPAD), vec![14]);
        assert_eq!(b.float_rect(13), Some(LRect::new(2000.0, 100.0, 400.0, 300.0)));
        assert!(set.homes.is_empty());
    }

    #[test]
    fn a_moved_screen_moves_its_floats() {
        let mut set = two_screens();
        let moved = LRect::new(2020.0, 0.0, 1920.0, 1080.0);
        set.reconcile(&[(n("A"), RA), (n("B"), moved)], Some("A"), 1.0, GAP, RB, GO);
        assert_eq!(set.screens[1].rect, moved);
        assert_eq!(
            set.screens[1].layout.float_rect(13),
            Some(LRect::new(2100.0, 100.0, 400.0, 300.0))
        );
        assert_eq!(sorted(set.screens[1].layout.all_clients()), vec![10, 11, 12, 13, 14]);
    }

    #[test]
    fn going_back_to_the_fallback_merges_every_client() {
        let mut set = two_screens();
        set.active = 1;
        set.reconcile(&[(n(""), DESK)], None, 1.0, GAP, RB, GO);
        assert_eq!(set.screens.len(), 1);
        assert_eq!(set.screens[0].name, "");
        assert_eq!(set.screens[0].rect, DESK);
        assert_eq!(sorted(set.all_clients()), vec![1, 10, 11, 12, 13, 14]);
        let l = &set.screens[0].layout;
        assert_eq!(l.clients_on(3), vec![12]);
        assert_eq!(l.clients_on(SCRATCHPAD), vec![14]);
        // B lies inside the desk, so its float keeps its place.
        assert_eq!(l.float_rect(13), Some(LRect::new(2000.0, 100.0, 400.0, 300.0)));
        assert_eq!(set.active, 0);
        assert_eq!(set.main, 0);
    }

    #[test]
    fn active_is_clamped_after_a_removal() {
        let mut set = two_screens();
        set.active = 1;
        set.reconcile(&[(n("A"), RA)], Some("A"), 10.0, GAP, RB, GO);
        set.reconcile(&[(n("A"), RA)], Some("A"), 12.0, GAP, RB, GO);
        assert_eq!(set.screens.len(), 1);
        assert_eq!(set.active, 0);
        assert_eq!(set.active_layout().all_clients().len(), 6);
    }

    #[test]
    fn active_follows_its_screen_by_name() {
        let mut set = two_screens();
        set.active = 1;
        // A new screen on the left shifts B to index 2.
        let rl = LRect::new(-1920.0, 0.0, 1920.0, 1080.0);
        let new = vec![(n("L"), rl), (n("A"), RA), (n("B"), RB_)];
        set.reconcile(&new, Some("A"), 1.0, GAP, RB, GO);
        assert_eq!(set.screens[set.active].name, "B");
        assert_eq!(set.main, 1);
    }

    #[test]
    fn on_pointer_reports_only_a_crossing() {
        let mut set = two_screens();
        assert_eq!(set.active, 0);
        assert_eq!(set.on_pointer(100.0, 100.0), None);
        assert_eq!(set.on_pointer(2500.0, 100.0), Some(1));
        assert_eq!(set.active, 1);
        assert_eq!(set.on_pointer(2600.0, 200.0), None);
        assert_eq!(set.on_pointer(10.0, 10.0), Some(0));
        assert_eq!(set.active, 0);
    }

    #[test]
    fn focused_client_is_the_active_screens() {
        let mut set = two_screens();
        assert_eq!(set.focused_client(), Some(1));
        set.on_pointer(2500.0, 100.0);
        assert_eq!(set.focused_client(), set.screens[1].layout.focused_client());
        assert!(set.focused_client().is_some());
    }

    #[test]
    fn move_to_screen_wraps_and_follows() {
        let mut set = two_screens();
        assert_eq!(set.move_to_screen(1, true, GAP, RB, GO), Some(1));
        assert_eq!(set.screen_of(1), Some(1));
        assert_eq!(set.active, 1);
        assert_eq!(set.focused_client(), Some(1));
        // Forward from the last screen wraps to the first.
        assert_eq!(set.move_to_screen(1, true, GAP, RB, GO), Some(0));
        assert_eq!(set.screen_of(1), Some(0));
        // Backward from the first wraps to the last.
        assert_eq!(set.move_to_screen(1, false, GAP, RB, GO), Some(1));
        assert_eq!(set.screen_of(1), Some(1));
    }

    #[test]
    fn move_to_screen_is_none_with_one_screen() {
        let mut set = fallback_set();
        assert_eq!(set.move_to_screen(1, true, GAP, RB, GO), None);
        assert_eq!(set.screen_of(1), Some(0));
    }

    #[test]
    fn move_to_screen_of_an_unknown_client_is_none() {
        let mut set = two_screens();
        assert_eq!(set.move_to_screen(99, true, GAP, RB, GO), None);
    }
}

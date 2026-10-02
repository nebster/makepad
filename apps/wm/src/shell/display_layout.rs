//! The saved display arrangement — order, main screen, per-screen modes
//! and the render-on GPU — as `~/.config/makepad/wm/display-layout`.
//!
//! This module is the pure model only: parsing, serializing and resolving
//! against this boot's [`LinuxDisplaySnapshot`]. It never touches the
//! filesystem; the worker reads and writes the file through
//! `display_settings_path` and `write_display_setting` (`system_linux.rs`)
//! the same way it does for the other display settings files.
//!
//! # File format
//!
//! ```text
//! makepad-display-layout 1
//! render-on 0000:01:00.0 10de:2b85
//! screen 0000:00:02.0 HDMI-A-2 mode=3840x2160@30
//! screen 0000:01:00.0 HDMI-A-1 main
//! ```
//!
//! Lines are whitespace-separated tokens. `#` starts a comment; blank
//! lines and unknown keywords are ignored. A screen's key is `(card PCI
//! address, connector without its cardN- prefix)`, which survives card
//! renumbering across boots: `LinuxDisplayOutput::pci` plus `name` minus
//! its `cardN-` prefix. `order` is the order of the `screen` lines, left
//! to right; `main` marks at most one screen as the main screen (the
//! first `main` wins when more than one line claims it); `mode=` is the
//! platform's mode syntax (`WxH`, `WxH@Hz`, `WxH-Hz`), validated the way
//! `mode_string_is_valid` does (`vulkan_linux.rs`, not public there, so
//! mirrored below); `render-on` has exactly `validate_gpu_choice`'s shape
//! (`<pci-address> <vendor>:<device>`). A duplicate screen key keeps its
//! first occurrence's position and flags.
//!
//! `display-layout` supersedes the older `display-source` and
//! `display-gpu` files; [`DisplayLayout::migrate`] reads them once to
//! build the first saved layout.
//!
//! Compiled for Linux only (the inner `cfg` below makes the module empty
//! elsewhere), so no other platform's behaviour changes.

#![cfg(all(target_os = "linux", not(target_env = "ohos")))]

use makepad_widgets::makepad_platform::linux_display::{LinuxDisplayOutput, LinuxDisplaySnapshot};

use super::system_linux::validate_gpu_choice;

/// The header line's keyword.
const HEADER_KEYWORD: &str = "makepad-display-layout";
/// The header line's version, written on every save.
const FORMAT_VERSION: &str = "1";

/// A screen's identity across reboots and card renumbering: the card's PCI
/// address (`LinuxDisplayOutput::pci`) and the connector name with its
/// `cardN-` prefix removed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScreenKey {
    pub pci: String,
    pub connector: String,
}

/// One `screen` line.
#[derive(Clone, Debug, PartialEq)]
pub struct ScreenEntry {
    pub key: ScreenKey,
    pub main: bool,
    pub mode: Option<String>,
}

/// The whole saved file: the render-on GPU (`None` is Auto) and the
/// screens in left-to-right order.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct DisplayLayout {
    pub render_on: Option<String>,
    pub screens: Vec<ScreenEntry>,
}

impl DisplayLayout {
    /// Parses the file's text. Tolerant: a line that is empty, a `#`
    /// comment, an unknown keyword, or malformed for its keyword is
    /// skipped rather than failing the whole parse. A duplicate screen key
    /// keeps its first occurrence; a second `main` flag (on any screen) is
    /// dropped, so at most one screen ends up `main`. An invalid `mode=`
    /// or `render-on` value is dropped, leaving that field unset.
    pub fn parse(text: &str) -> DisplayLayout {
        let mut render_on = None;
        let mut screens: Vec<ScreenEntry> = Vec::new();
        let mut main_seen = false;
        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut tokens = line.split_whitespace();
            let Some(keyword) = tokens.next() else { continue };
            match keyword {
                HEADER_KEYWORD => {}
                "render-on" => {
                    if render_on.is_some() {
                        continue;
                    }
                    let rest: Vec<&str> = tokens.collect();
                    let value = rest.join(" ");
                    if validate_gpu_choice(&value).is_ok() {
                        render_on = Some(value);
                    }
                }
                "screen" => {
                    let Some(pci) = tokens.next() else { continue };
                    let Some(connector) = tokens.next() else { continue };
                    if pci.is_empty() || connector.is_empty() {
                        continue;
                    }
                    let key = ScreenKey { pci: pci.to_string(), connector: connector.to_string() };
                    if screens.iter().any(|entry: &ScreenEntry| entry.key == key) {
                        continue; // duplicate key: first occurrence wins
                    }
                    let mut main = false;
                    let mut mode = None;
                    for token in tokens {
                        if token == "main" {
                            if !main_seen {
                                main = true;
                            }
                        } else if let Some(value) = token.strip_prefix("mode=") {
                            if mode_string_is_valid(value) {
                                mode = Some(value.to_string());
                            }
                        }
                    }
                    if main {
                        main_seen = true;
                    }
                    screens.push(ScreenEntry { key, main, mode });
                }
                _ => {}
            }
        }
        DisplayLayout { render_on, screens }
    }

    /// Serializes back to the file's text. Round-trips through
    /// [`DisplayLayout::parse`] for any layout this module itself built
    /// (at most one `main`, a valid `mode` and `render_on`, no duplicate
    /// keys — exactly what the mutating methods below maintain).
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        out.push_str(HEADER_KEYWORD);
        out.push(' ');
        out.push_str(FORMAT_VERSION);
        out.push('\n');
        if let Some(render_on) = &self.render_on {
            out.push_str("render-on ");
            out.push_str(render_on);
            out.push('\n');
        }
        for entry in &self.screens {
            out.push_str("screen ");
            out.push_str(&entry.key.pci);
            out.push(' ');
            out.push_str(&entry.key.connector);
            if entry.main {
                out.push_str(" main");
            }
            if let Some(mode) = &entry.mode {
                out.push_str(" mode=");
                out.push_str(mode);
            }
            out.push('\n');
        }
        out
    }

    /// The current arrangement, built straight from a snapshot rather than
    /// parsed: placed screens only (`desktop_position.is_some()`), ordered
    /// left to right by `desktop_position.x`, `main` from `primary`, mode
    /// from `mode_override`. An output without a resolvable `pci` is
    /// skipped (it could never be matched back on a later boot).
    pub fn from_snapshot(snap: &LinuxDisplaySnapshot, render_on: Option<String>) -> DisplayLayout {
        let mut placed: Vec<&LinuxDisplayOutput> =
            snap.outputs.iter().filter(|output| output.desktop_position.is_some()).collect();
        placed.sort_by_key(|output| output.desktop_position.map(|(x, _)| x).unwrap_or(0));
        let screens = placed
            .into_iter()
            .filter_map(|output| {
                screen_key(output).map(|key| ScreenEntry {
                    key,
                    main: output.primary,
                    mode: output.mode_override.clone(),
                })
            })
            .collect();
        DisplayLayout { render_on, screens }
    }

    /// Each saved screen that resolves against this boot's snapshot (its
    /// key matches an output's `pci` and connector suffix), as
    /// `(index into screens, that output's current name)`. A screen that
    /// does not resolve this boot is dropped — never matched by connector
    /// alone, so the same connector on another GPU is not mistaken for it.
    pub fn resolve(&self, snap: &LinuxDisplaySnapshot) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        for (index, entry) in self.screens.iter().enumerate() {
            if let Some(output) =
                snap.outputs.iter().find(|output| screen_key(output).as_ref() == Some(&entry.key))
            {
                out.push((index, output.name.clone()));
            }
        }
        out
    }

    /// This boot's output names in the saved left-to-right order, followed
    /// by any output the saved file did not place, in the snapshot's own
    /// (default) order — the shape `linux_set_display_order` wants.
    pub fn order_names(&self, snap: &LinuxDisplaySnapshot) -> Vec<String> {
        let resolved = self.resolve(snap);
        let mut names: Vec<String> = Vec::with_capacity(snap.outputs.len());
        for (_, name) in &resolved {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        for output in &snap.outputs {
            if !names.contains(&output.name) {
                names.push(output.name.clone());
            }
        }
        names
    }

    /// `(this boot's name, saved mode)` for every saved screen that both
    /// resolves and has a saved mode.
    pub fn modes(&self, snap: &LinuxDisplaySnapshot) -> Vec<(String, String)> {
        self.resolve(snap)
            .into_iter()
            .filter_map(|(index, name)| self.screens[index].mode.clone().map(|mode| (name, mode)))
            .collect()
    }

    /// The main screen's this-boot name, if the saved main screen (the
    /// first `screen` entry with `main` set) resolves.
    pub fn main_name(&self, snap: &LinuxDisplaySnapshot) -> Option<String> {
        let resolved = self.resolve(snap);
        let (main_index, _) = self.screens.iter().enumerate().find(|(_, entry)| entry.main)?;
        resolved.into_iter().find(|(index, _)| *index == main_index).map(|(_, name)| name)
    }

    /// Swaps the screen keyed by `key` with its left (`dir < 0`) or right
    /// (`dir > 0`) neighbour. `false`, no change, when the key is not
    /// found or the swap would run past an end.
    pub fn move_screen(&mut self, key: &ScreenKey, dir: i32) -> bool {
        let Some(index) = self.screens.iter().position(|entry| &entry.key == key) else {
            return false;
        };
        let target = index as i32 + dir;
        if target < 0 || target as usize >= self.screens.len() {
            return false;
        }
        self.screens.swap(index, target as usize);
        true
    }

    /// Marks the screen keyed by `key` as the (only) main screen. No-op
    /// when `key` is not one of the saved screens.
    pub fn set_main(&mut self, key: &ScreenKey) {
        if !self.screens.iter().any(|entry| &entry.key == key) {
            return;
        }
        for entry in &mut self.screens {
            entry.main = &entry.key == key;
        }
    }

    /// Sets the screen keyed by `key`'s mode, or clears it with `None`.
    /// A mode that fails `mode_string_is_valid` is treated as `None`
    /// instead of being saved. No-op when `key` is not one of the saved
    /// screens.
    pub fn set_mode(&mut self, key: &ScreenKey, mode: Option<String>) {
        let Some(entry) = self.screens.iter_mut().find(|entry| &entry.key == key) else {
            return;
        };
        entry.mode = mode.filter(|mode| mode_string_is_valid(mode));
    }

    /// Builds the first saved layout from the legacy files: `display-source`
    /// (a card-prefixed connector name such as `card1-HDMI-A-1`, resolved to
    /// a boot-stable key through `card_pci`, the caller's `cardN -> pci`
    /// lookup, becoming the one saved screen, `main`) and `display-gpu`
    /// (carried over to `render_on` as-is, when it has `validate_gpu_choice`'s
    /// shape). Either input can be absent or fail to resolve; the result is
    /// then missing that part rather than failing outright.
    pub fn migrate(
        display_source: Option<&str>,
        display_gpu: Option<&str>,
        card_pci: &dyn Fn(&str) -> Option<String>,
    ) -> DisplayLayout {
        let mut screens = Vec::new();
        if let Some(source) = display_source {
            if let Some((card, connector)) = split_card_prefix(source) {
                if let Some(pci) = card_pci(card) {
                    screens.push(ScreenEntry {
                        key: ScreenKey { pci, connector: connector.to_string() },
                        main: true,
                        mode: None,
                    });
                }
            }
        }
        let render_on =
            display_gpu.and_then(|gpu| validate_gpu_choice(gpu).ok().map(str::to_string));
        DisplayLayout { render_on, screens }
    }
}

/// `output`'s screen key, or `None` when it has no resolvable `pci` (it
/// could never be matched back on a later boot, so it is not worth
/// saving).
pub fn screen_key(output: &LinuxDisplayOutput) -> Option<ScreenKey> {
    let pci = output.pci.clone()?;
    Some(ScreenKey { pci, connector: connector_suffix(&output.name) })
}

/// `name` minus its `cardN-` prefix, or `name` unchanged when it does not
/// have that shape.
fn connector_suffix(name: &str) -> String {
    match split_card_prefix(name) {
        Some((_, connector)) => connector.to_string(),
        None => name.to_string(),
    }
}

/// Splits `cardN-<connector>` into `("cardN", "<connector>")`, or `None`
/// when `name` does not start with `card` followed by digits then `-`.
fn split_card_prefix(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("card")?;
    let dash = rest.find('-')?;
    let digits = &rest[..dash];
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let card_len = "card".len() + dash;
    Some((&name[..card_len], &name[card_len + 1..]))
}

/// Whether `mode` is `WxH`, `WxH@Hz` or `WxH-Hz` with digits only — the
/// forms `MAKEPAD_DRM_MODE`, `MAKEPAD_DRM_MODES` and
/// `direct_request_display_mode` accept. Mirrors the platform's
/// `mode_string_is_valid` (`platform/src/os/linux/vulkan_linux.rs`), which
/// is private and `cfg(linux_direct)`-gated, so it is not reachable from
/// here.
fn mode_string_is_valid(mode: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|byte| byte.is_ascii_digit());
    let resolution = match mode.split_once('@').or_else(|| mode.split_once('-')) {
        Some((resolution, rate)) => {
            if !digits(rate) {
                return false;
            }
            resolution
        }
        None => mode,
    };
    match resolution.split_once('x') {
        Some((width, height)) => digits(width) && digits(height),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str, pci: &str) -> LinuxDisplayOutput {
        LinuxDisplayOutput { name: name.to_string(), pci: Some(pci.to_string()), ..Default::default() }
    }

    fn placed(name: &str, pci: &str, x: u32, primary: bool) -> LinuxDisplayOutput {
        LinuxDisplayOutput {
            desktop_position: Some((x, 0)),
            primary,
            ..output(name, pci)
        }
    }

    fn snapshot(outputs: Vec<LinuxDisplayOutput>) -> LinuxDisplaySnapshot {
        LinuxDisplaySnapshot { direct: true, outputs }
    }

    #[test]
    fn round_trip() {
        let layout = DisplayLayout {
            render_on: Some("0000:01:00.0 10de:2b85".to_string()),
            screens: vec![
                ScreenEntry {
                    key: ScreenKey { pci: "0000:00:02.0".to_string(), connector: "HDMI-A-2".to_string() },
                    main: false,
                    mode: Some("3840x2160@30".to_string()),
                },
                ScreenEntry {
                    key: ScreenKey { pci: "0000:01:00.0".to_string(), connector: "HDMI-A-1".to_string() },
                    main: true,
                    mode: None,
                },
            ],
        };
        let text = layout.serialize();
        assert_eq!(DisplayLayout::parse(&text), layout);
    }

    #[test]
    fn junk_and_comments_are_ignored() {
        let text = "\
            # a comment\n\
            \n\
            makepad-display-layout 1\n\
            mystery-key some value\n\
            screen\n\
            screen 0000:00:02.0\n\
            screen 0000:01:00.0 HDMI-A-1 main\n\
            # another comment\n\
        ";
        let layout = DisplayLayout::parse(text);
        assert_eq!(layout.render_on, None);
        assert_eq!(layout.screens.len(), 1);
        assert_eq!(layout.screens[0].key.connector, "HDMI-A-1");
        assert!(layout.screens[0].main);
    }

    #[test]
    fn card_renumbering_resolves_by_pci_and_connector_suffix() {
        let layout = DisplayLayout::parse("screen 0000:00:02.0 HDMI-A-2\n");
        let snap = snapshot(vec![output("card1-HDMI-A-2", "0000:00:02.0")]);
        assert_eq!(layout.resolve(&snap), vec![(0, "card1-HDMI-A-2".to_string())]);
    }

    #[test]
    fn same_connector_on_two_gpus_resolves_by_pci() {
        let layout = DisplayLayout::parse("screen 0000:01:00.0 HDMI-A-1\n");
        let snap = snapshot(vec![
            output("card0-HDMI-A-1", "0000:00:02.0"),
            output("card1-HDMI-A-1", "0000:01:00.0"),
        ]);
        assert_eq!(layout.resolve(&snap), vec![(0, "card1-HDMI-A-1".to_string())]);
    }

    #[test]
    fn missing_screen_is_dropped() {
        let layout = DisplayLayout::parse(
            "screen 0000:00:02.0 HDMI-A-2\nscreen 0000:ff:00.0 DP-9\n",
        );
        let snap = snapshot(vec![output("card0-HDMI-A-2", "0000:00:02.0")]);
        assert_eq!(layout.resolve(&snap), vec![(0, "card0-HDMI-A-2".to_string())]);
        assert_eq!(layout.order_names(&snap), vec!["card0-HDMI-A-2".to_string()]);
    }

    #[test]
    fn two_main_flags_keep_the_first() {
        let layout = DisplayLayout::parse(
            "screen 0000:00:02.0 HDMI-A-2 main\nscreen 0000:01:00.0 HDMI-A-1 main\n",
        );
        assert!(layout.screens[0].main);
        assert!(!layout.screens[1].main);
    }

    #[test]
    fn move_screen_at_both_ends() {
        let mut layout = DisplayLayout::parse(
            "screen 0000:00:02.0 A\nscreen 0000:00:03.0 B\nscreen 0000:00:04.0 C\n",
        );
        let a = ScreenKey { pci: "0000:00:02.0".to_string(), connector: "A".to_string() };
        let c = ScreenKey { pci: "0000:00:04.0".to_string(), connector: "C".to_string() };
        assert!(!layout.move_screen(&a, -1));
        assert!(!layout.move_screen(&c, 1));
        assert!(layout.move_screen(&a, 1));
        assert_eq!(layout.screens[0].key.connector, "B");
        assert_eq!(layout.screens[1].key.connector, "A");
    }

    #[test]
    fn invalid_mode_is_dropped() {
        let layout = DisplayLayout::parse("screen 0000:00:02.0 HDMI-A-2 mode=garbage\n");
        assert_eq!(layout.screens[0].mode, None);
        let layout = DisplayLayout::parse("screen 0000:00:02.0 HDMI-A-2 mode=3840x2160@30\n");
        assert_eq!(layout.screens[0].mode, Some("3840x2160@30".to_string()));
    }

    #[test]
    fn from_snapshot_orders_by_desktop_position_x_and_skips_unplaced_and_no_pci() {
        let mut unplaced = output("card0-DP-9", "0000:00:09.0");
        unplaced.desktop_position = None;
        let mut no_pci = placed("card0-DP-8", "0000:00:08.0", 0, false);
        no_pci.pci = None;
        let snap = snapshot(vec![
            placed("card1-HDMI-A-1", "0000:01:00.0", 1920, true),
            placed("card0-HDMI-A-2", "0000:00:02.0", 0, false),
            unplaced,
            no_pci,
        ]);
        let layout = DisplayLayout::from_snapshot(&snap, None);
        assert_eq!(layout.screens.len(), 2);
        assert_eq!(layout.screens[0].key.connector, "HDMI-A-2");
        assert!(!layout.screens[0].main);
        assert_eq!(layout.screens[1].key.connector, "HDMI-A-1");
        assert!(layout.screens[1].main);
    }

    #[test]
    fn migrate_maps_display_source_and_display_gpu() {
        let card_pci = |card: &str| -> Option<String> {
            match card {
                "card1" => Some("0000:01:00.0".to_string()),
                _ => None,
            }
        };
        let layout = DisplayLayout::migrate(
            Some("card1-HDMI-A-1"),
            Some("0000:01:00.0 10de:2b85"),
            &card_pci,
        );
        assert_eq!(layout.render_on, Some("0000:01:00.0 10de:2b85".to_string()));
        assert_eq!(layout.screens.len(), 1);
        assert!(layout.screens[0].main);
        assert_eq!(
            layout.screens[0].key,
            ScreenKey { pci: "0000:01:00.0".to_string(), connector: "HDMI-A-1".to_string() }
        );
    }

    #[test]
    fn migrate_drops_unresolvable_source_and_invalid_gpu() {
        let card_pci = |_: &str| -> Option<String> { None };
        let layout = DisplayLayout::migrate(Some("card1-HDMI-A-1"), Some("not a gpu choice"), &card_pci);
        assert_eq!(layout.screens.len(), 0);
        assert_eq!(layout.render_on, None);
    }

    #[test]
    fn screen_key_is_none_without_pci() {
        let mut no_pci = output("card0-HDMI-A-2", "0000:00:02.0");
        no_pci.pci = None;
        assert_eq!(screen_key(&no_pci), None);
        let with_pci = output("card0-HDMI-A-2", "0000:00:02.0");
        assert_eq!(
            screen_key(&with_pci),
            Some(ScreenKey { pci: "0000:00:02.0".to_string(), connector: "HDMI-A-2".to_string() })
        );
    }
}

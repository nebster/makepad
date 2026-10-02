//! Linux desktop controls. The UI owns handles and immutable snapshots;
//! all device operations belong to the system worker.
//!
//! Two display settings are the UI's own to apply, with only their
//! *persistence* going to the worker:
//!
//! * The display scale: the slider's release goes to
//!   `Cx::set_window_dpi_override` on the main window (the platform rewrites
//!   the geometry and every child tile follows). The saved value is applied
//!   once at start-up, when the window exists, unless the person is already
//!   at the slider or has set a scale this session.
//! * The display layout (`display-layout`, see `shell::display_layout`):
//!   screen order, main screen, per-screen modes and the render-on GPU.
//!   The WM keeps a working copy; every edit updates it and queues the
//!   whole file for saving (one slot, newest intent). At start-up the
//!   saved layout is restored as a safety net behind the session script's
//!   environment (`restore_plan`): order and modes once, on the first
//!   display generation that has a laid-out desktop; the main screen
//!   through `Cx::linux_set_display_source` as soon as that screen is
//!   active (or at once when it is a peer GPU's screen that has not joined
//!   yet, which the renderer accepts early), else after a bounded wait.
//!   Once the person changes order, main or mode this session the restore
//!   stops, so it never undoes them. The renderer's snapshot (`primary`)
//!   is the only word on which screen is main, so a request stays
//!   "pending" until it is observed there, and is reported if it never is.
//! * Pointer speed: each drag goes to `linux_input::set_pointer_speed`
//!   (process-wide, mouse and touchpad separately). The saved values are
//!   applied once at start-up unless the person already set that device
//!   this session; a late read never undoes a speed set here.
use crate::{shell, App, ClientId};
use makepad_widgets::*;
use makepad_widgets::makepad_platform::linux_input::{
    set_pointer_speed, POINTER_SPEED_MAX, POINTER_SPEED_MIN,
};
use makepad_widgets::makepad_platform::linux_display::LinuxDisplaySnapshot;
use shell::display_layout::{
    is_joining, restore_plan, restore_waiting, screen_key, DisplayLayout, RestoreOp, ScreenEntry,
};
use shell::panels::{PanelKind, ShellPanel};
use shell::system_linux::{
    validate_display_source, validate_gpu_choice, BatteryStatus, CommandResult, SystemCommand,
    SystemController, DISPLAY_SCALE_MAX, DISPLAY_SCALE_MIN,
};
use std::collections::VecDeque;

/// How long a requested source may go unconfirmed by the renderer's
/// snapshot before the panel stops calling it pending and says so.
const SOURCE_PENDING_TIMEOUT: f64 = 5.0;
/// How long a saved source whose output is listed but not presenting yet
/// (`active` means a frame was shown) may wait for its first frame at
/// start-up before the renderer's default is kept.
const SOURCE_RESTORE_TIMEOUT: f64 = 10.0;

#[derive(Clone, Debug, PartialEq)]
pub enum ControlAction {
    OutputVolume(u32),
    InputVolume(u32),
    Brightness(u32),
    /// The display scale in hundredths (130 = 1.30×), emitted on release.
    DpiScale(u32),
    /// The "Optimize for" output: a connector name from the renderer's
    /// inventory, whose native pixels define the shared framebuffer.
    DisplaySource(String),
    /// Live compositor GPU: `Some(identity)` as `GpuInfo::identity` gives
    /// it, `None` for the display GPU. Applied now; persisted only after
    /// the switch is accepted and finishes.
    GpuChoice(Option<String>),
    /// Live per-app GPU for this running client: `None` follows the
    /// compositor. Not persisted.
    AppGpuChoice { client: ClientId, choice: Option<String> },
    /// Pointer speed in hundredths (100 = 1.00×). Applied on every update;
    /// `touchpad` selects the device (`false` mouse, `true` touchpad).
    PointerSpeed { touchpad: bool, value: u32 },
    Command(SystemCommand),
}

#[derive(Default)]
pub struct LinuxControls {
    controller: Option<SystemController>,
    attempted: bool,
    waiting: VecDeque<SystemCommand>,
    seen: VecDeque<u32>,
    notice: String,
    /// The scale still to be persisted: one slot, so a newer release
    /// replaces an older one that the full queue held back.
    dpi_save: Option<u32>,
    /// The person set a scale this session; a start-up read that lands
    /// afterwards must not undo it.
    dpi_user_set: bool,
    /// The start-up read has been applied (or deliberately skipped) once.
    dpi_loaded_applied: bool,
    /// The working display layout: the saved one once the worker's
    /// start-up read lands, with this session's edits on top. `None` until
    /// either happens.
    layout: Option<DisplayLayout>,
    /// The worker's start-up read has been taken into `layout`.
    layout_loaded: bool,
    /// The whole layout still to be persisted, same one-slot rule. Held
    /// until `layout_loaded`: a whole-file save made before the start-up
    /// read is taken in would drop what the file held.
    layout_save: Option<DisplayLayout>,
    /// This session edited the layout (any part); once the start-up read
    /// is merged in, the merged layout is queued for saving again.
    layout_edited: bool,
    /// The person changed order, main screen or a mode this session: the
    /// start-up restore must not override them, and a late read keeps
    /// their screens.
    layout_user_set: bool,
    /// The person chose a render-on GPU this session; a late read keeps it.
    gpu_user_set: bool,
    /// The saved order and modes have been restored (or deliberately
    /// skipped) once.
    layout_restore_done: bool,
    /// The display generation the order/mode restore last looked at; the
    /// plan is computed once per new generation until it is done.
    layout_restore_generation: Option<u64>,
    /// Order and mode requests already sent by the restore: one identical
    /// request is never sent twice, so waiting for a joining screen does
    /// not loop through rescans.
    layout_restore_sent: Vec<RestoreOp>,
    /// The saved main screen, still joining, has been asked for early once.
    main_early_tried: bool,
    /// The display generation and time the restore last sent an order or
    /// mode request: the reacquire it starts can outlast the main screen's
    /// 5 s confirm, so the confirm waits for it (`main_restore_confirms`).
    layout_restore_sent_at: Option<(u64, f64)>,
    /// Accepted compositor switch whose persistence waits until
    /// `finish_linux_gpu_choice`. `Some(None)` is the display GPU.
    gpu_after_commit: Option<Option<String>>,
    /// Newest unsent pointer speed per device (0 mouse, 1 touchpad).
    pointer_save: [Option<u32>; 2],
    /// The person set that device's speed this session; a start-up read
    /// that lands afterwards must not undo it.
    pointer_user_set: [bool; 2],
    /// The start-up read has been applied (or deliberately skipped) once
    /// per device.
    pointer_loaded_applied: [bool; 2],
    /// The saved main screen has been asked for (or deliberately skipped)
    /// once.
    source_loaded_applied: bool,
    /// When the start-up restoration first saw an inventory to decide on;
    /// the bounded wait for a not-yet-presenting output counts from here.
    source_restore_since: Option<f64>,
    /// Armed by a main-window geometry change: the frame after it, the open
    /// flyout re-reads its bar module's rect, which the bar has re-laid out.
    reanchor_frame: NextFrame,
}

/// `slot`'s layout to send now: nothing until the start-up read has been
/// taken in (`loaded`), since a whole-file save before it would drop what
/// the file held.
fn take_layout_save(loaded: bool, slot: &mut Option<DisplayLayout>) -> Option<DisplayLayout> {
    if loaded { slot.take() } else { None }
}

/// Whether the start-up restore's main-screen request gets the bounded
/// pending confirm. Not while an order or mode request the restore sent
/// (at generation `sent.0`, time `sent.1`) may still be reacquiring: the
/// renderer has to have moved past that generation and the confirm's own
/// span has to have passed since the send, else a slow reacquire would
/// show a false "did not switch" notice.
fn main_restore_confirms(sent: Option<(u64, f64)>, generation: u64, now: f64) -> bool {
    match sent {
        None => true,
        Some((sent_generation, sent_time)) => {
            generation > sent_generation && now - sent_time > SOURCE_PENDING_TIMEOUT
        }
    }
}

fn valid_dpi(dpi: f64) -> bool {
    dpi.is_finite() && dpi > 0.0
}

impl LinuxControls {
    /// Edit the working layout and queue the whole file for saving. With a
    /// snapshot, every live screen is made part of the layout first, so an
    /// edit of one that was not saved yet is not lost.
    fn edit_layout(&mut self, displays: Option<&LinuxDisplaySnapshot>, edit: impl FnOnce(&mut DisplayLayout)) {
        let layout = self.layout.get_or_insert_with(DisplayLayout::default);
        if let Some(displays) = displays {
            layout.include_snapshot(displays);
        }
        edit(layout);
        self.layout_edited = true;
        self.layout_save = Some(layout.clone());
    }

    /// The layout to send to the worker now, if any: nothing until the
    /// start-up read has been taken in.
    #[cfg(test)]
    fn take_layout_save(&mut self) -> Option<DisplayLayout> {
        take_layout_save(self.layout_loaded, &mut self.layout_save)
    }

    /// The person changed order, main screen or a mode: the start-up
    /// restore is over for this session.
    fn layout_set_by_person(&mut self) {
        self.layout_user_set = true;
        self.layout_restore_done = true;
        self.source_loaded_applied = true;
    }

    /// The worker's start-up read: the working copy unless this session
    /// already edited it first, in which case it is merged in
    /// (`DisplayLayout::merge_loaded`: the person's edits stand, every
    /// other saved screen and an untouched render-on come from the read).
    /// Marks the read as taken in; a layout edited before it is queued
    /// again, merged, so the whole-file save keeps what the file held.
    fn adopt_loaded_layout(&mut self, loaded: &DisplayLayout) {
        self.layout_loaded = true;
        match self.layout.as_mut() {
            None => self.layout = Some(loaded.clone()),
            Some(layout) => layout.merge_loaded(loaded, self.layout_user_set, self.gpu_user_set),
        }
        if self.layout_edited {
            self.layout_save = self.layout.clone();
        }
    }
}

impl App {
    /// The main window, once the platform has created it.
    fn main_window_id(&self, cx: &mut Cx) -> Option<WindowId> {
        let window_id = self.ui.window(cx, ids!(main_window)).window_id()?;
        (cx.windows.is_valid(window_id) && cx.windows[window_id].is_created).then_some(window_id)
    }

    /// The main window's effective layout scale, as the platform holds it.
    fn main_window_dpi(&self, cx: &mut Cx) -> Option<f64> {
        let window_id = self.main_window_id(cx)?;
        Some(cx.windows[window_id].window_geom.dpi_factor).filter(|dpi| valid_dpi(*dpi))
    }

    /// Apply a display scale to the main window. The platform queues the
    /// `WindowGeomChange`; the flyout is re-anchored on the frame after it.
    fn apply_dpi_scale(&mut self, cx: &mut Cx, hundredths: u32) -> bool {
        let Some(window_id) = self.main_window_id(cx) else {
            self.linux_controls.notice = "Display scale: the desktop window is not up yet".into();
            log!("wm: display scale {hundredths} not applied: no main window");
            return false;
        };
        cx.set_window_dpi_override(window_id, Some(hundredths as f64 / 100.0));
        self.linux_controls.reanchor_frame = cx.new_next_frame();
        true
    }

    /// Map a sysfs identity `<pci> <vendor>:<device>` onto a Vulkan UUID
    /// from the cached GPU snapshot. PCI address and vendor/device must
    /// all match; vendor/device alone is never enough.
    fn resolve_gpu_uuid(&self, cx: &Cx, identity: &str) -> Result<[u8; 16], String> {
        let (pci, ids) = identity.split_once(' ').ok_or_else(|| "GPU identity is not `<pci-address> <vendor>:<device>`".to_string())?;
        let (vendor, device) = ids.split_once(':').ok_or_else(|| "GPU identity is not `<pci-address> <vendor>:<device>`".to_string())?;
        let vendor_id = u32::from_str_radix(vendor, 16).map_err(|_| "GPU vendor id is not hexadecimal".to_string())?;
        let device_id = u32::from_str_radix(device, 16).map_err(|_| "GPU device id is not hexadecimal".to_string())?;
        let snapshot = cx.linux_gpu_snapshot();
        let mut found = None;
        for gpu in &snapshot.devices {
            let Some(address) = gpu.pci_address.as_deref().filter(|a| !a.is_empty()) else {
                continue;
            };
            if address == pci && gpu.vendor_id == vendor_id && gpu.device_id == device_id {
                if found.is_some() {
                    return Err("GPU identity matches more than one device".into());
                }
                found = Some(gpu.uuid);
            }
        }
        found.ok_or_else(|| "GPU is not available".into())
    }

    /// Compositor `None` is the snapshot's display GPU. An identity uses
    /// the same snapshot match as a per-app choice.
    fn resolve_compositor_uuid(&self, cx: &Cx, choice: Option<&str>) -> Result<[u8; 16], String> {
        match choice {
            None => cx
                .linux_gpu_snapshot()
                .display
                .as_ref()
                .map(|gpu| gpu.uuid)
                .ok_or_else(|| "Display GPU is not available".into()),
            Some(identity) => self.resolve_gpu_uuid(cx, identity),
        }
    }

    /// Persistence for an accepted compositor switch. Root calls this
    /// once the transition finishes. Failure drops the pending save.
    pub(crate) fn finish_linux_gpu_choice(&mut self, cx: &mut Cx, success: bool) {
        let pending = self.linux_controls.gpu_after_commit.take();
        if success {
            if let Some(choice) = pending {
                self.linux_controls.edit_layout(None, |layout| layout.render_on = choice);
            }
        }
        if !self.linux_gpu.notice.is_empty() {
            self.linux_controls.notice = self.linux_gpu.notice.clone();
        }
        self.poll_linux_controls(cx);
    }

    /// Ask the renderer for a source. `Ok` means the request is queued,
    /// not that the source changed: with `confirm` the flyout shows the
    /// name as pending until the snapshot's `primary` says it is the one.
    /// A peer GPU's screen that has not joined yet becomes `primary` only
    /// once it starts, however long that takes, so its start-up restore
    /// goes without the bounded confirmation.
    fn request_display_source(&mut self, cx: &mut Cx, name: &str, confirm: bool) -> bool {
        match cx.linux_set_display_source(name) {
            Ok(()) => {
                if confirm {
                    let panel = self.ui.widget(cx, ids!(shell_panel));
                    if let Some(mut p) = panel.borrow_mut::<ShellPanel>() {
                        p.display_source_pending = Some((name.to_string(), Cx::monotonic_now()));
                        p.redraw(cx);
                    };
                }
                true
            }
            Err(message) => {
                self.linux_controls.notice = format!("Optimize for {name}: {message}");
                log!("wm: display source {name} refused: {message}");
                false
            }
        }
    }

    /// An open flyout follows its bar module: after the bar re-lays out
    /// (a DPI change, a window resize) the module's rect is read again.
    fn reanchor_shell_panel(&mut self, cx: &mut Cx) {
        let panel = self.ui.widget(cx, ids!(shell_panel));
        let Some(kind) = panel.borrow::<ShellPanel>().and_then(|p| p.open) else {
            return;
        };
        let bar = self.ui.widget(cx, ids!(shell_bar));
        let anchor = bar.borrow::<shell::bar::ShellBar>().and_then(|b| b.module_rect(self.shell_panel_segment, kind.module()));
        if let Some(anchor) = anchor {
            if let Some(mut p) = panel.borrow_mut::<ShellPanel>() {
                p.reanchor(cx, anchor);
            }
        }
    }

    /// Events the controls watch besides the tick and the signal: the
    /// re-anchor frame and the main window's geometry changes.
    pub(crate) fn linux_controls_event(&mut self, cx: &mut Cx, event: &Event) {
        if self.linux_controls.reanchor_frame.is_event(event).is_some() {
            self.reanchor_shell_panel(cx);
        }
        if let Event::WindowGeomChange(ev) = event {
            if Some(ev.window_id) != self.main_window_id(cx) {
                return;
            }
            // The flyout's readout and its anchor move with the window: the
            // readout now, the anchor once the bar has drawn at the new size.
            let panel = self.ui.widget(cx, ids!(shell_panel));
            if let Some(mut p) = panel.borrow_mut::<ShellPanel>() {
                if valid_dpi(ev.new_geom.dpi_factor) {
                    p.current_dpi = ev.new_geom.dpi_factor;
                }
                if p.open.is_some() {
                    p.redraw(cx);
                }
            }
            self.linux_controls.reanchor_frame = cx.new_next_frame();
        }
    }

    pub(crate) fn linux_control(&mut self, cx: &mut Cx, action: ControlAction) {
        self.poll_linux_controls(cx);
        let action = match action {
            ControlAction::DpiScale(value) => {
                // The scale is applied here whatever the worker's state: the
                // person sees it at once, and only the saving can fail.
                let value = value.clamp(DISPLAY_SCALE_MIN, DISPLAY_SCALE_MAX);
                self.linux_controls.notice.clear();
                self.linux_controls.dpi_user_set = true;
                self.linux_controls.dpi_loaded_applied = true;
                self.apply_dpi_scale(cx, value);
                self.linux_controls.dpi_save = Some(value);
                self.poll_linux_controls(cx);
                self.redraw_all(cx);
                return;
            }
            ControlAction::DisplaySource(name) => {
                self.linux_controls.notice.clear();
                match validate_display_source(&name) {
                    // Persist only what the renderer accepted to try, as
                    // the layout's main screen (keyed by PCI address and
                    // connector, not this boot's card number).
                    Ok(name) if self.request_display_source(cx, name, true) => {
                        self.linux_controls.layout_set_by_person();
                        let displays = cx.linux_display_snapshot();
                        match displays.outputs.iter().find(|o| o.name == name).and_then(screen_key) {
                            Some(key) => self.linux_controls.edit_layout(Some(&displays), |layout| {
                                if !layout.screens.iter().any(|entry| entry.key == key) {
                                    layout.screens.push(ScreenEntry { key: key.clone(), main: false, mode: None });
                                }
                                layout.set_main(&key);
                            }),
                            None => log!("wm: main screen {name} not saved: its card has no PCI address"),
                        }
                    }
                    Ok(_) => {}
                    Err(message) => self.linux_controls.notice = format!("Optimize for: {message}"),
                }
                self.poll_linux_controls(cx);
                self.redraw_all(cx);
                return;
            }
            ControlAction::GpuChoice(choice) => {
                self.linux_controls.notice.clear();
                let invalid = choice.as_deref().and_then(|identity| validate_gpu_choice(identity).err());
                if let Some(message) = invalid {
                    self.linux_controls.notice = format!("Render on: {message}");
                } else {
                    match self.resolve_compositor_uuid(cx, choice.as_deref()) {
                        Err(message) => self.linux_controls.notice = format!("Render on: {message}"),
                        Ok(uuid) => match self.change_compositor_gpu(cx, uuid) {
                            Err(message) => self.linux_controls.notice = format!("Render on: {message}"),
                            Ok(()) => {
                                self.linux_controls.gpu_user_set = true;
                                if self.linux_gpu.active() {
                                    self.linux_controls.gpu_after_commit = Some(choice);
                                } else {
                                    self.linux_controls
                                        .edit_layout(None, |layout| layout.render_on = choice);
                                }
                            }
                        },
                    }
                }
                self.poll_linux_controls(cx);
                self.redraw_all(cx);
                return;
            }
            ControlAction::AppGpuChoice { client, choice } => {
                self.linux_controls.notice.clear();
                let uuid = match choice.as_deref() {
                    None => Ok(None),
                    Some(identity) => match validate_gpu_choice(identity) {
                        Err(message) => Err(message),
                        Ok(identity) => self.resolve_gpu_uuid(cx, identity).map(Some),
                    },
                };
                match uuid {
                    Err(message) => self.linux_controls.notice = format!("App GPU: {message}"),
                    Ok(uuid) => {
                        if let Err(message) = self.change_app_gpu(cx, client, uuid) {
                            self.linux_controls.notice = format!("App GPU: {message}");
                        }
                    }
                }
                self.poll_linux_controls(cx);
                self.redraw_all(cx);
                return;
            }
            ControlAction::PointerSpeed { touchpad, value } => {
                // Applied here whatever the worker's state: the person sees
                // it at once, and only the saving can fail.
                let value = value.clamp(POINTER_SPEED_MIN, POINTER_SPEED_MAX);
                let i = touchpad as usize;
                self.linux_controls.notice.clear();
                self.linux_controls.pointer_user_set[i] = true;
                self.linux_controls.pointer_loaded_applied[i] = true;
                set_pointer_speed(touchpad, value);
                self.linux_controls.pointer_save[i] = Some(value);
                self.poll_linux_controls(cx);
                self.redraw_all(cx);
                return;
            }
            other => other,
        };
        let state = &mut self.linux_controls;
        state.notice.clear();
        if let Some(controller) = state.controller.as_mut().filter(|c| c.worker_alive()) {
            match action {
                ControlAction::OutputVolume(value) => { controller.set_output_volume(value); }
                ControlAction::InputVolume(value) => { controller.set_input_volume(value); }
                // Keep a visible panel while adjusting from the desktop.
                ControlAction::Brightness(value) => { controller.set_brightness(value.max(1)); }
                ControlAction::DpiScale(_)
                | ControlAction::DisplaySource(_)
                | ControlAction::GpuChoice(_)
                | ControlAction::AppGpuChoice { .. }
                | ControlAction::PointerSpeed { .. } => {}
                ControlAction::Command(command) => {
                    if state.waiting.len() < 32 {
                        state.waiting.push_back(command);
                    } else {
                        state.notice = "Controls are busy; please try again".into();
                        log!("wm: system control queue full");
                    }
                }
            }
        } else {
            state.notice = "System controls are unavailable".into();
        }
        self.poll_linux_controls(cx);
        self.redraw_all(cx);
    }

    pub(crate) fn poll_linux_controls(&mut self, cx: &mut Cx) {
        if self.gallery { return; }
        let gpu_busy = self.linux_gpu.active();
        let gpu_notice = (gpu_busy && !self.linux_gpu.notice.is_empty()).then(|| self.linux_gpu.notice.clone());
        let gpu_app = self.linux_gpu_app_status();
        let state = &mut self.linux_controls;
        if !state.attempted {
            state.attempted = true;
            match SystemController::start(&cx.thread_spawner()) {
                Ok(controller) => state.controller = Some(controller),
                Err(error) => {
                    state.notice = format!("System controls could not start: {error}");
                    log!("wm: {}", state.notice);
                }
            }
        }
        let mut changed = false;
        let snapshot = if let Some(controller) = state.controller.as_mut() {
            changed = controller.poll();
            if controller.worker_alive() {
                // The display settings first: each is the newest intent in
                // its own slot, so a backlog of other commands never drops it.
                if let Some(value) = state.dpi_save.take() {
                    if controller.send(SystemCommand::SaveDpiScale(value)).is_err() {
                        state.dpi_save = Some(value);
                    }
                }
                if let Some(layout) = take_layout_save(state.layout_loaded, &mut state.layout_save) {
                    if let Err(SystemCommand::SaveDisplayLayout(layout)) =
                        controller.send(SystemCommand::SaveDisplayLayout(layout))
                    {
                        state.layout_save = Some(layout);
                    }
                }
                for i in 0..2 {
                    if let Some(value) = state.pointer_save[i].take() {
                        let touchpad = i != 0;
                        if controller
                            .send(SystemCommand::SavePointerSpeed { touchpad, value })
                            .is_err()
                        {
                            state.pointer_save[i] = Some(value);
                        }
                    }
                }
                while let Some(command) = state.waiting.pop_front() {
                    if let Err(command) = controller.send(command) {
                        state.waiting.push_front(command);
                        break;
                    }
                }
            } else {
                state.waiting.clear();
                let unsaved = state.dpi_save.take().is_some()
                    | state.layout_save.take().is_some()
                    | state.pointer_save[0].take().is_some()
                    | state.pointer_save[1].take().is_some();
                state.notice = if unsaved {
                    "Setting applied, but not saved: system controls stopped".into()
                } else {
                    "System controls stopped; restart the WM to retry".into()
                };
            }
            let snapshot = controller.snapshot().clone();
            for outcome in &snapshot.outcomes {
                if state.seen.contains(&outcome.seq) { continue; }
                if state.seen.len() == 64 { state.seen.pop_front(); }
                state.seen.push_back(outcome.seq);
                if let CommandResult::Failed(message) = &outcome.result {
                    state.notice = message.clone();
                    log!("wm: system control {:?} failed: {}", outcome.kind, message);
                }
            }
            Some(snapshot)
        } else { None };
        // The saved settings, once each. A late read after the person
        // already acted is ignored; nothing saved means the launch (scale)
        // or the renderer's own (layout) default stands.
        let mut initial_scale = None;
        let mut initial_pointer = [None; 2];
        if let Some(snapshot) = snapshot.as_ref().filter(|s| s.display_settings_loaded) {
            if !state.dpi_loaded_applied {
                if state.dpi_user_set {
                    state.dpi_loaded_applied = true;
                } else if let Some(value) = snapshot.dpi_scale {
                    initial_scale = Some(value);
                } else {
                    state.dpi_loaded_applied = true;
                }
            }
            if !state.layout_loaded {
                state.adopt_loaded_layout(&snapshot.display_layout);
                if snapshot.display_layout.screens.is_empty() {
                    // Nothing saved to arrange: the renderer's defaults stand.
                    state.layout_restore_done = true;
                    state.source_loaded_applied = true;
                }
            }
        }
        let restore_pending = snapshot.as_ref().is_some_and(|s| s.display_settings_loaded)
            && !state.layout_user_set
            && (!state.layout_restore_done || !state.source_loaded_applied);
        if let Some(snapshot) = snapshot.as_ref().filter(|s| s.input_settings_loaded) {
            for i in 0..2 {
                if !state.pointer_loaded_applied[i] {
                    if state.pointer_user_set[i] {
                        state.pointer_loaded_applied[i] = true;
                    } else if let Some(value) = snapshot.pointer_speeds[i] {
                        initial_pointer[i] = Some(value);
                    } else {
                        state.pointer_loaded_applied[i] = true;
                    }
                }
            }
        }
        let panel = self.ui.widget(cx, ids!(shell_panel));
        let (adjusting, monitor_open, pending) = panel
            .borrow::<ShellPanel>()
            .map(|p| (p.dpi_adjusting(), p.open == Some(PanelKind::Monitor), p.display_source_pending.clone()))
            .unwrap_or((false, false, None));
        if let Some(value) = initial_scale {
            // Not under a drag: a saved value landing mid-preview would move
            // the window under the slider. It waits for the next poll.
            if !adjusting && self.main_window_id(cx).is_some() {
                self.linux_controls.dpi_loaded_applied = true;
                log!("wm: applying saved display scale {value}");
                self.apply_dpi_scale(cx, value);
                changed = true;
            }
        }
        for (i, value) in initial_pointer.into_iter().enumerate() {
            if let Some(value) = value {
                self.linux_controls.pointer_loaded_applied[i] = true;
                let name = if i == 0 { "mouse" } else { "touchpad" };
                log!("wm: applying saved {name} speed {value}");
                set_pointer_speed(i != 0, value);
                changed = true;
            }
        }
        // The renderer's inventory, whenever something here needs it: the
        // open Display panel, a saved source to ask for, a request to
        // confirm. Immutable cached snapshots, no lock, no Vulkan, no
        // filesystem. GPU identity is only for the open Display readout.
        let displays = (monitor_open || restore_pending || pending.is_some())
            .then(|| cx.linux_display_snapshot());
        let gpus = monitor_open.then(|| cx.linux_gpu_snapshot());
        if let (true, Some(displays)) = (restore_pending, displays.as_ref()) {
            changed |= self.restore_display_layout(cx, displays, adjusting);
        }
        let actual_dpi = self.main_window_dpi(cx);
        let state = &mut self.linux_controls;
        if let Some(mut panel) = panel.borrow_mut::<ShellPanel>() {
            // The readout is the window's actual scale, never the saved one.
            if let Some(dpi) = actual_dpi {
                if panel.current_dpi != dpi {
                    panel.current_dpi = dpi;
                    changed = true;
                }
            }
            if let Some(displays) = displays {
                if panel.display_snapshot != displays {
                    panel.display_snapshot = displays;
                    changed = true;
                }
                // A request is confirmed only by the renderer naming that
                // output as the primary; one that never is gets reported.
                let observed = panel.display_source_pending.as_ref().is_some_and(|(name, _)| {
                    panel.display_snapshot.outputs.iter().any(|o| o.primary && o.name == *name)
                });
                let expired = panel.display_source_pending.as_ref()
                    .is_some_and(|(_, since)| Cx::monotonic_now() - *since > SOURCE_PENDING_TIMEOUT);
                if observed {
                    panel.display_source_pending = None;
                    changed = true;
                } else if expired {
                    if let Some((name, _)) = panel.display_source_pending.take() {
                        state.notice = format!("Optimize for {name}: the renderer did not switch to it");
                        log!("wm: {}", state.notice);
                    }
                    changed = true;
                }
            }
            if let Some(gpus) = gpus {
                if panel.gpu_snapshot != gpus {
                    panel.gpu_snapshot = gpus;
                    changed = true;
                }
            }
            if panel.gpu_busy != gpu_busy {
                panel.gpu_busy = gpu_busy;
                changed = true;
            }
            if panel.gpu_app != gpu_app {
                if panel.gpu_app.as_ref().map(|app| app.0) != gpu_app.as_ref().map(|app| app.0) {
                    // A list drawn for one client cannot silently acquire
                    // another client's label when keyboard focus changes.
                    panel.close_gpu_lists();
                }
                panel.gpu_app = gpu_app;
                changed = true;
            }
            if let Some(notice) = gpu_notice {
                state.notice = notice;
            }
            panel.system_notice = state.notice.clone();
            if let Some(snapshot) = snapshot {
                let audio = &snapshot.audio;
                self.bar_sample.volume = audio.default_output().map(|d| d.percent);
                self.bar_sample.muted = audio.default_output().is_some_and(|d| d.muted);
                self.bar_sample.brightness = snapshot.brightness.percent();
                self.bar_sample.battery = snapshot.power.percent.map(|percent| shell::bar::Battery {
                    percent,
                    charging: snapshot.power.status == BatteryStatus::Charging,
                });
                panel.data.volume = self.bar_sample.volume;
                panel.data.muted = self.bar_sample.muted;
                panel.data.input_volume = audio.default_input().map(|d| d.percent);
                panel.data.brightness = self.bar_sample.brightness;
                panel.data.battery = self.bar_sample.battery;
                panel.system = snapshot;
            }
            if changed { panel.redraw(cx); }
        }
        if changed { self.update_bar(cx); }
        self.reanchor_shell_panel(cx);
    }

    /// The start-up restore of the saved layout, against this poll's
    /// display inventory. Returns whether it asked the renderer for
    /// anything. Order and modes: on each new display generation whose
    /// desktop is laid out, the requests not sent before (the renderer
    /// stores both before a peer GPU's screen joins, so it joins as saved),
    /// until no saved screen is still joining or `SOURCE_RESTORE_TIMEOUT`
    /// has passed since the first inventory. The main screen is asked for
    /// once it is active, or at once while it is joining (another GPU's
    /// screen, which the renderer accepts early); otherwise it gets the
    /// same bounded wait.
    fn restore_display_layout(&mut self, cx: &mut Cx, displays: &LinuxDisplaySnapshot, adjusting: bool) -> bool {
        if !displays.direct {
            // The desktop owns the outputs: nothing to arrange.
            log!("wm: saved display layout not applied: the desktop owns the outputs");
            self.linux_controls.layout_restore_done = true;
            self.linux_controls.source_loaded_applied = true;
            return false;
        }
        if displays.outputs.is_empty() || adjusting {
            // No inventory yet, or the scale slider is held: a layout
            // change moves the window as a scale change does. Wait;
            // nothing is decided or logged until then.
            return false;
        }
        let layout = self.linux_controls.layout.clone().unwrap_or_default();
        let plan = restore_plan(&layout, displays);
        let now = Cx::monotonic_now();
        let since = *self.linux_controls.source_restore_since.get_or_insert(now);
        let timed_out = now - since > SOURCE_RESTORE_TIMEOUT;
        let mut asked = false;
        let generation = cx.linux_display_generation();
        if !self.linux_controls.layout_restore_done {
            let laid_out = displays.outputs.iter().any(|o| o.desktop_position.is_some());
            if self.linux_controls.layout_restore_generation != Some(generation) && laid_out {
                self.linux_controls.layout_restore_generation = Some(generation);
                for op in &plan {
                    if self.linux_controls.layout_restore_sent.contains(op) {
                        continue;
                    }
                    match op {
                        RestoreOp::Order(order) => {
                            log!("wm: applying saved display order {}", order.join(","));
                            if let Err(message) = cx.linux_set_display_order(order) {
                                log!("wm: saved display order refused: {message}");
                            }
                        }
                        RestoreOp::Mode(name, mode) => {
                            let label = mode.as_deref().unwrap_or("automatic");
                            log!("wm: applying saved mode {label} for {name}");
                            if let Err(message) = cx.linux_set_display_mode(name, mode.as_deref()) {
                                log!("wm: saved mode {label} for {name} refused: {message}");
                            }
                        }
                        RestoreOp::Main(_) => continue,
                    }
                    self.linux_controls.layout_restore_sent.push(op.clone());
                    self.linux_controls.layout_restore_sent_at = Some((generation, now));
                    asked = true;
                }
            }
            // Open while a saved screen is still joining (so it is laid
            // out on its saved side), within the same bound as the main
            // screen's wait.
            if laid_out && !restore_waiting(&layout, displays) {
                self.linux_controls.layout_restore_done = true;
            } else if timed_out {
                log!("wm: saved display order: a saved screen did not join in time; keeping what is laid out");
                self.linux_controls.layout_restore_done = true;
            }
        }
        if !self.linux_controls.source_loaded_applied {
            let request = plan.iter().find_map(|op| match op {
                RestoreOp::Main(name) => Some(name.as_str()),
                _ => None,
            });
            let handled = match (request, layout.main_name(displays)) {
                (_, None) => {
                    if layout.screens.iter().any(|entry| entry.main) {
                        log!("wm: saved main screen is not connected; keeping the renderer's default");
                    }
                    true
                }
                (None, Some(name)) => {
                    log!("wm: saved main screen {name} is already the main screen");
                    true
                }
                (Some(name), _) => match displays.outputs.iter().find(|o| o.name == name) {
                    Some(output) if output.active => {
                        log!("wm: applying saved main screen {name}");
                        // Not confirmed against the 5 s bound while an order
                        // or mode change the restore sent may still be
                        // reacquiring: that can take longer.
                        let confirm = main_restore_confirms(
                            self.linux_controls.layout_restore_sent_at,
                            generation,
                            now,
                        );
                        self.request_display_source(cx, name, confirm);
                        asked = true;
                        true
                    }
                    Some(output) if is_joining(output) && !self.linux_controls.main_early_tried => {
                        // Another GPU's screen on its way: the renderer
                        // takes the name before it presents, and it turns
                        // primary only when it does, so no bounded confirm.
                        // A refusal (a screen of this GPU not ready yet)
                        // falls back to the wait below.
                        self.linux_controls.main_early_tried = true;
                        match cx.linux_set_display_source(name) {
                            Ok(()) => {
                                log!("wm: applying saved main screen {name} (joining)");
                                asked = true;
                                true
                            }
                            Err(message) => {
                                log!("wm: saved main screen {name} not accepted yet ({message}); waiting for it");
                                false
                            }
                        }
                    }
                    Some(_) if timed_out => {
                        log!("wm: saved main screen {name} did not become usable; keeping the renderer's default");
                        true
                    }
                    Some(_) => false,
                    None => true,
                },
            };
            if handled {
                self.linux_controls.source_loaded_applied = true;
            }
        }
        asked
    }

    pub(crate) fn shutdown_linux_controls(&mut self) {
        if let Some(mut controller) = self.linux_controls.controller.take() { controller.shutdown(); }
        self.linux_controls.waiting.clear();
        self.linux_controls.dpi_save = None;
        self.linux_controls.layout_save = None;
        self.linux_controls.gpu_after_commit = None;
        self.linux_controls.pointer_save = [None; 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(text: &str) -> DisplayLayout {
        DisplayLayout::parse(text)
    }

    #[test]
    fn layout_save_waits_for_the_start_up_read_and_merges_it() {
        let mut controls = LinuxControls::default();
        // A GPU switch committed before the worker's read landed.
        controls.gpu_user_set = true;
        controls.edit_layout(None, |l| l.render_on = Some("0000:01:00.0 10de:2b85".to_string()));
        assert_eq!(controls.take_layout_save(), None);
        let saved = layout("render-on 0000:00:02.0 8086:a780\nscreen 0000:00:02.0 HDMI-A-2 mode=3840x2160@30\n");
        controls.adopt_loaded_layout(&saved);
        let queued = controls.take_layout_save().expect("merged layout re-queued");
        assert_eq!(queued.render_on, Some("0000:01:00.0 10de:2b85".to_string()));
        assert_eq!(queued.screens, saved.screens);
        assert_eq!(controls.take_layout_save(), None);
    }

    #[test]
    fn early_main_pick_save_keeps_the_files_other_screens() {
        let mut controls = LinuxControls::default();
        // The person picked a main screen before the read landed.
        controls.layout_set_by_person();
        controls.edit_layout(None, |l| *l = layout("screen 0000:00:02.0 eDP-1 main\n"));
        assert_eq!(controls.take_layout_save(), None);
        controls.adopt_loaded_layout(&layout(
            "screen 0000:01:00.0 DP-3 main mode=2560x1440@144\nscreen 0000:00:02.0 eDP-1\n",
        ));
        let queued = controls.take_layout_save().expect("merged layout re-queued");
        assert_eq!(queued.screens.len(), 2);
        assert!(queued.screens[0].main);
        assert_eq!(queued.screens[1].key.connector, "DP-3");
        assert_eq!(queued.screens[1].mode, Some("2560x1440@144".to_string()));
        assert!(!queued.screens[1].main);
    }

    #[test]
    fn main_restore_confirm_waits_for_a_sent_reacquire() {
        assert!(main_restore_confirms(None, 3, 1.0));
        // Order sent at generation 4, t = 10: no confirm on the same
        // generation, nor before the confirm span has passed.
        assert!(!main_restore_confirms(Some((4, 10.0)), 4, 20.0));
        assert!(!main_restore_confirms(Some((4, 10.0)), 6, 12.0));
        assert!(main_restore_confirms(Some((4, 10.0)), 6, 16.0));
    }

    #[test]
    fn untouched_read_is_not_saved_back() {
        let mut controls = LinuxControls::default();
        controls.adopt_loaded_layout(&layout("screen 0000:00:02.0 HDMI-A-2\n"));
        assert_eq!(controls.take_layout_save(), None);
        assert_eq!(controls.layout, Some(layout("screen 0000:00:02.0 HDMI-A-2\n")));
    }
}

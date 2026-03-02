mod actions;
pub(crate) mod gestures;
mod pointer;

use smithay::{
    backend::{
        input::{
            AbsolutePositionEvent, Axis, Event, InputBackend, InputEvent, KeyState,
            KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
        },
        session::Session,
    },
    desktop::{layer_map_for_output, WindowSurfaceType},
    input::keyboard::FilterResult,
    input::pointer::{MotionEvent, RelativeMotionEvent},
    utils::{Point, SERIAL_COUNTER},
    wayland::shell::wlr_layer::Layer as WlrLayer,
};

use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;

use driftwm::canvas::{ScreenPos, screen_to_canvas};
use crate::decorations::DecorationHit;
use crate::state::{DriftWm, FocusTarget};

impl DriftWm {
    /// Process a single input event from any backend (winit, libinput, etc).
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        self.mark_all_dirty();

        // When locked, forward keyboard (VT switch + lock surface input) and
        // pointer events directly to smithay — no compositor grabs or gestures.
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            match event {
                InputEvent::Keyboard { event } => self.on_keyboard::<I>(event),
                InputEvent::PointerMotion { event } => self.on_pointer_motion_relative::<I>(event),
                InputEvent::PointerMotionAbsolute { event } => {
                    self.on_pointer_motion_absolute::<I>(event)
                }
                InputEvent::PointerButton { event } => {
                    let pointer = self.seat.get_pointer().unwrap();
                    pointer.button(
                        self,
                        &smithay::input::pointer::ButtonEvent {
                            button: PointerButtonEvent::button_code(&event),
                            state: PointerButtonEvent::state(&event),
                            serial: SERIAL_COUNTER.next_serial(),
                            time: Event::time_msec(&event),
                        },
                    );
                    pointer.frame(self);
                }
                InputEvent::PointerAxis { event } => {
                    let pointer = self.seat.get_pointer().unwrap();
                    let mut frame = smithay::input::pointer::AxisFrame::new(
                        Event::time_msec(&event),
                    ).source(event.source());
                    for axis in [Axis::Horizontal, Axis::Vertical] {
                        if let Some(amount) = event.amount(axis) {
                            frame = frame
                                .value(axis, amount)
                                .relative_direction(axis, event.relative_direction(axis));
                        }
                        if let Some(v120) = event.amount_v120(axis) {
                            frame = frame.v120(axis, v120 as i32);
                        }
                    }
                    pointer.axis(self, frame);
                    pointer.frame(self);
                }
                _ => {}
            }
            return;
        }

        match event {
            InputEvent::Keyboard { event } => self.on_keyboard::<I>(event),
            InputEvent::PointerMotion { event } => self.on_pointer_motion_relative::<I>(event),
            InputEvent::PointerMotionAbsolute { event } => {
                self.on_pointer_motion_absolute::<I>(event)
            }
            InputEvent::PointerButton { event } => self.on_pointer_button::<I>(event),
            InputEvent::PointerAxis { event } => self.on_pointer_axis::<I>(event),
            InputEvent::GestureSwipeBegin { event } => self.on_gesture_swipe_begin::<I>(event),
            InputEvent::GestureSwipeUpdate { event } => self.on_gesture_swipe_update::<I>(event),
            InputEvent::GestureSwipeEnd { event } => self.on_gesture_swipe_end::<I>(event),
            InputEvent::GesturePinchBegin { event } => self.on_gesture_pinch_begin::<I>(event),
            InputEvent::GesturePinchUpdate { event } => self.on_gesture_pinch_update::<I>(event),
            InputEvent::GesturePinchEnd { event } => self.on_gesture_pinch_end::<I>(event),
            InputEvent::GestureHoldBegin { event } => self.on_gesture_hold_begin::<I>(event),
            InputEvent::GestureHoldEnd { event } => self.on_gesture_hold_end::<I>(event),
            _ => {}
        }
    }

    fn on_keyboard<I: InputBackend>(&mut self, event: I::KeyboardKeyEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let key_state = event.state();
        let keycode = event.key_code();
        let keycode_u32: u32 = keycode.into();

        // When session is locked, only allow VT switching — forward everything else
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.input::<(), _>(
                self, keycode, key_state, serial, time,
                |state, _modifiers, handle| {
                    if key_state == KeyState::Pressed {
                        let raw = handle.modified_sym().raw();
                        if (0x1008FE01..=0x1008FE0C).contains(&raw) {
                            let vt = (raw - 0x1008FE01 + 1) as i32;
                            if let Some(ref mut session) = state.session
                                && let Err(e) = session.change_vt(vt)
                            {
                                tracing::warn!("Failed to switch to VT{vt}: {e}");
                            }
                        }
                    }
                    FilterResult::Forward
                },
            );
            return;
        }

        // Clear key repeat on release of the held key
        if key_state == KeyState::Released
            && let Some((held_keycode, _, _)) = &self.held_action
            && *held_keycode == keycode_u32
        {
            self.held_action = None;
        }

        let keyboard = self.seat.get_keyboard().unwrap();

        let action = keyboard.input(
            self,
            keycode,
            key_state,
            serial,
            time,
            |state, modifiers, handle| {
                // If cycling is active and the cycle modifier was released, end cycle
                if state.cycle_state.is_some()
                    && !state.config.cycle_modifier.is_pressed(modifiers)
                {
                    state.end_cycle();
                    return FilterResult::Forward;
                }

                if key_state == KeyState::Pressed {
                    let sym = handle.modified_sym();

                    // VT switching: Ctrl+Alt+F1..F12 produces XF86Switch_VT_1..12
                    let raw = sym.raw();
                    if (0x1008FE01..=0x1008FE0C).contains(&raw) {
                        let vt = (raw - 0x1008FE01 + 1) as i32;
                        if let Some(ref mut session) = state.session
                            && let Err(e) = session.change_vt(vt)
                        {
                            tracing::warn!("Failed to switch to VT{vt}: {e}");
                        }
                        return FilterResult::Intercept(None);
                    }

                    if let Some(action) = state.config.lookup(modifiers, sym) {
                        return FilterResult::Intercept(Some(action.clone()));
                    }
                }
                FilterResult::Forward
            },
        );

        // Update active layout name (may have changed via XKB group switch)
        let layout_name = keyboard.with_xkb_state(self, |ctx| {
            let xkb = ctx.xkb().lock().unwrap();
            let layout = xkb.active_layout();
            xkb.layout_name(layout).to_owned()
        });
        if self.active_layout != layout_name {
            self.active_layout = layout_name;
        }

        if let Some(ref action) = action.flatten() {
            // Set up key repeat for repeatable actions
            if action.is_repeatable() {
                let delay = std::time::Duration::from_millis(self.config.repeat_delay as u64);
                self.held_action = Some((keycode_u32, action.clone(), std::time::Instant::now() + delay));
            } else {
                // Non-repeatable action pressed — cancel any active repeat
                self.held_action = None;
            }
            self.execute_action(action);
        }
    }

    fn on_pointer_motion_absolute<I: InputBackend>(
        &mut self,
        event: I::PointerMotionAbsoluteEvent,
    ) {
        let output = match self.space.outputs().next() {
            Some(o) => o.clone(),
            None => return,
        };
        let output_geo = self.space.output_geometry(&output).unwrap();

        // position_transformed gives screen-local coords (0..width, 0..height)
        let screen_pos = event.position_transformed(output_geo.size);
        let canvas_pos = screen_to_canvas(ScreenPos(screen_pos), self.camera, self.zoom).0;

        // When locked, pointer only targets the lock surface
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            let serial = SERIAL_COUNTER.next_serial();
            let time = Event::time_msec(&event);
            let pointer = self.seat.get_pointer().unwrap();
            let focus = self.lock_surface.as_ref().map(|ls| {
                (FocusTarget(ls.wl_surface().clone()), Point::<f64, smithay::utils::Logical>::from((0.0, 0.0)))
            });
            pointer.motion(self, focus, &MotionEvent { location: screen_pos, serial, time });
            pointer.frame(self);
            return;
        }
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let pointer = self.seat.get_pointer().unwrap();

        // Pointer always stays in canvas coords so cursor rendering and grabs
        // work consistently. Layer surface focus locations are adjusted to
        // compensate (see layer_surface_under).

        // Check Overlay and Top layers at screen coords
        if let Some(hit) = self.layer_surface_under(screen_pos, canvas_pos, &[WlrLayer::Overlay, WlrLayer::Top]) {
            self.pointer_over_layer = true;
            pointer.motion(self, Some(hit), &MotionEvent { location: canvas_pos, serial, time });
            pointer.frame(self);
            return;
        }

        // Check canvas-positioned layer surfaces at canvas coords
        if let Some(hit) = self.canvas_layer_under(canvas_pos) {
            self.pointer_over_layer = false;
            pointer.motion(self, Some(hit), &MotionEvent { location: canvas_pos, serial, time });
            pointer.frame(self);
            return;
        }

        // Check canvas windows at canvas coords
        let under = self.surface_under(canvas_pos);
        if under.is_some() {
            self.pointer_over_layer = false;
            pointer.motion(self, under, &MotionEvent { location: canvas_pos, serial, time });
            pointer.frame(self);
            self.update_decoration_cursor(canvas_pos);
            return;
        }

        // Check Bottom and Background layers at screen coords
        if let Some(hit) = self.layer_surface_under(screen_pos, canvas_pos, &[WlrLayer::Bottom, WlrLayer::Background]) {
            self.pointer_over_layer = true;
            pointer.motion(self, Some(hit), &MotionEvent { location: canvas_pos, serial, time });
            pointer.frame(self);
            return;
        }

        // No hit — empty canvas
        self.pointer_over_layer = false;
        pointer.motion(self, None, &MotionEvent { location: canvas_pos, serial, time });
        pointer.frame(self);
        self.update_decoration_cursor(canvas_pos);
    }

    /// Handle relative pointer motion (libinput mice/trackpads).
    /// Converts screen-space delta to canvas-space via zoom, then dispatches
    /// the same layered hit-testing as absolute motion.
    fn on_pointer_motion_relative<I: InputBackend>(
        &mut self,
        event: I::PointerMotionEvent,
    ) {
        // When locked, pointer only targets the lock surface
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            let pointer = self.seat.get_pointer().unwrap();
            let old_pos = pointer.current_location();
            let delta = event.delta();
            let new_pos: Point<f64, smithay::utils::Logical> =
                (old_pos.x + delta.x, old_pos.y + delta.y).into();
            let serial = SERIAL_COUNTER.next_serial();
            let time = Event::time_msec(&event);
            let focus = self.lock_surface.as_ref().map(|ls| {
                (FocusTarget(ls.wl_surface().clone()), Point::<f64, smithay::utils::Logical>::from((0.0, 0.0)))
            });
            pointer.motion(self, focus, &MotionEvent { location: new_pos, serial, time });
            pointer.frame(self);
            return;
        }

        let pointer = self.seat.get_pointer().unwrap();
        let old_canvas = pointer.current_location();
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);

        // Delta is in screen pixels — convert to canvas delta
        let delta = event.delta();
        let canvas_delta: Point<f64, smithay::utils::Logical> =
            (delta.x / self.zoom, delta.y / self.zoom).into();
        let canvas_pos = old_canvas + canvas_delta;

        // Clamp to output bounds in screen space so cursor can't escape
        let output = match self.space.outputs().next() {
            Some(o) => o.clone(),
            None => return,
        };
        let output_size = output
            .current_mode()
            .map(|m| m.size.to_logical(1))
            .unwrap_or((1, 1).into());
        let screen_pos = driftwm::canvas::canvas_to_screen(
            driftwm::canvas::CanvasPos(canvas_pos),
            self.camera,
            self.zoom,
        ).0;
        let clamped_screen: Point<f64, smithay::utils::Logical> = (
            screen_pos.x.clamp(0.0, output_size.w as f64 - 1.0),
            screen_pos.y.clamp(0.0, output_size.h as f64 - 1.0),
        ).into();
        let canvas_pos = driftwm::canvas::screen_to_canvas(
            ScreenPos(clamped_screen),
            self.camera,
            self.zoom,
        ).0;

        // Emit relative motion event for clients that use zwp_relative_pointer
        pointer.relative_motion(
            self,
            self.surface_under(canvas_pos),
            &RelativeMotionEvent {
                delta,
                delta_unaccel: event.delta_unaccel(),
                utime: Event::time(&event),
            },
        );

        // Hit-test layers then canvas (same as absolute motion)
        if let Some(hit) = self.layer_surface_under(clamped_screen, canvas_pos, &[WlrLayer::Overlay, WlrLayer::Top]) {
            self.pointer_over_layer = true;
            pointer.motion(self, Some(hit), &MotionEvent { location: canvas_pos, serial, time });
            pointer.frame(self);
            return;
        }

        if let Some(hit) = self.canvas_layer_under(canvas_pos) {
            self.pointer_over_layer = false;
            pointer.motion(self, Some(hit), &MotionEvent { location: canvas_pos, serial, time });
            pointer.frame(self);
            return;
        }

        let under = self.surface_under(canvas_pos);
        if under.is_some() {
            self.pointer_over_layer = false;
            pointer.motion(self, under, &MotionEvent { location: canvas_pos, serial, time });
            pointer.frame(self);
            self.update_decoration_cursor(canvas_pos);
            return;
        }

        if let Some(hit) = self.layer_surface_under(clamped_screen, canvas_pos, &[WlrLayer::Bottom, WlrLayer::Background]) {
            self.pointer_over_layer = true;
            pointer.motion(self, Some(hit), &MotionEvent { location: canvas_pos, serial, time });
            pointer.frame(self);
            return;
        }

        self.pointer_over_layer = false;
        pointer.motion(self, None, &MotionEvent { location: canvas_pos, serial, time });
        pointer.frame(self);
        self.update_decoration_cursor(canvas_pos);
    }

    /// Find the Wayland surface and local coordinates under the given canvas position.
    /// This is the foundation for all hit-testing — focus, gestures, resize grabs.
    /// Also checks SSD decoration areas (title bar, resize borders), interleaved
    /// with window content in z-order so a higher window's content takes priority
    /// over a lower window's decorations.
    pub fn surface_under(
        &self,
        pos: Point<f64, smithay::utils::Logical>,
    ) -> Option<(FocusTarget, Point<f64, smithay::utils::Logical>)> {
        let bar_height = driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT;
        let border_width = driftwm::config::DecorationConfig::RESIZE_BORDER_WIDTH;

        for window in self.space.elements().rev() {
            let wl_surface = window.toplevel().unwrap().wl_surface();
            if driftwm::config::applied_rule(wl_surface).is_some_and(|r| r.no_focus) {
                continue;
            }

            let Some(loc) = self.space.element_location(window) else { continue };

            // element_location returns the geometry origin, but surface_under
            // expects coords relative to the surface origin (which includes
            // client-side shadows/margins). The offset is geometry().loc.
            let geom_offset = window.geometry().loc;
            let surface_origin = loc - geom_offset;

            // Check window content first (higher priority than decorations)
            if let Some((surface, surface_loc)) = window.surface_under(
                pos - surface_origin.to_f64(),
                WindowSurfaceType::ALL,
            ) {
                return Some((FocusTarget(surface), (surface_loc + surface_origin).to_f64()));
            }

            // Then check SSD decoration areas for this window
            if self.decorations.contains_key(&wl_surface.id()) {
                let size = window.geometry().size;
                if crate::decorations::close_button_contains(pos, loc, size.w, bar_height)
                    || crate::decorations::title_bar_contains(pos, loc, size.w, bar_height)
                    || crate::decorations::resize_edge_at(pos, loc, size, bar_height, border_width).is_some()
                {
                    return Some((FocusTarget(wl_surface.clone()), loc.to_f64()));
                }
            }
        }
        None
    }

    /// Update cursor icon based on what decoration area the pointer is over.
    /// Called after pointer motion to set resize/pointer cursors for SSD areas.
    fn update_decoration_cursor(&mut self, canvas_pos: Point<f64, smithay::utils::Logical>) {
        if self.grab_cursor || self.pointer_over_layer {
            return;
        }
        match self.decoration_under(canvas_pos) {
            Some((ref window, DecorationHit::CloseButton)) => {
                self.decoration_cursor = true;
                self.cursor_status =
                    smithay::input::pointer::CursorImageStatus::Named(
                        smithay::input::pointer::CursorIcon::Pointer,
                    );
                self.set_close_hovered(window, true);
            }
            Some((ref window, DecorationHit::ResizeBorder(edge))) => {
                self.decoration_cursor = true;
                self.cursor_status =
                    smithay::input::pointer::CursorImageStatus::Named(
                        crate::input::pointer::resize_cursor(edge),
                    );
                self.set_close_hovered(window, false);
            }
            Some((ref window, DecorationHit::TitleBar)) => {
                self.decoration_cursor = true;
                self.cursor_status =
                    smithay::input::pointer::CursorImageStatus::default_named();
                self.set_close_hovered(window, false);
            }
            None => {
                if self.decoration_cursor {
                    self.decoration_cursor = false;
                    self.cursor_status =
                        smithay::input::pointer::CursorImageStatus::default_named();
                    self.clear_all_close_hovered();
                }
            }
        }
    }

    /// Set the close button hover state for a specific window's decoration.
    fn set_close_hovered(&mut self, window: &Window, hovered: bool) {
        let wl_surface = window.toplevel().unwrap().wl_surface();
        if let Some(deco) = self.decorations.get_mut(&wl_surface.id())
            && deco.close_hovered != hovered
        {
            deco.close_hovered = hovered;
            deco.title_bar = crate::decorations::render_title_bar(
                deco.width, deco.focused, hovered, &self.config.decorations,
            );
        }
    }

    /// Clear close button hover on all decorations (when leaving decoration areas).
    fn clear_all_close_hovered(&mut self) {
        for deco in self.decorations.values_mut() {
            if deco.close_hovered {
                deco.close_hovered = false;
                deco.title_bar = crate::decorations::render_title_bar(
                    deco.width, deco.focused, false, &self.config.decorations,
                );
            }
        }
    }

    /// Check if a canvas position hits an SSD decoration area.
    /// Returns the window and what part of the decoration was hit.
    pub fn decoration_under(
        &self,
        pos: Point<f64, smithay::utils::Logical>,
    ) -> Option<(Window, DecorationHit)> {
        let bar_height = driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT;
        let border_width = driftwm::config::DecorationConfig::RESIZE_BORDER_WIDTH;

        // Iterate in z-order (topmost first, matching space.elements().rev())
        for window in self.space.elements().rev() {
            let wl_surface = window.toplevel().unwrap().wl_surface();
            if !self.decorations.contains_key(&wl_surface.id()) {
                continue;
            }
            let Some(loc) = self.space.element_location(window) else {
                continue;
            };
            let size = window.geometry().size;

            // Close button (checked before title bar)
            if crate::decorations::close_button_contains(pos, loc, size.w, bar_height) {
                return Some((window.clone(), DecorationHit::CloseButton));
            }

            // Title bar
            if crate::decorations::title_bar_contains(pos, loc, size.w, bar_height) {
                return Some((window.clone(), DecorationHit::TitleBar));
            }

            // Resize borders
            if let Some(edge) =
                crate::decorations::resize_edge_at(pos, loc, size, bar_height, border_width)
            {
                return Some((window.clone(), DecorationHit::ResizeBorder(edge)));
            }
        }
        None
    }

    /// Find a canvas-positioned layer surface under the given canvas position.
    /// These live in canvas coords (like xdg windows), so no coordinate tricks needed.
    pub(crate) fn canvas_layer_under(
        &self,
        canvas_pos: Point<f64, smithay::utils::Logical>,
    ) -> Option<(FocusTarget, Point<f64, smithay::utils::Logical>)> {
        for cl in &self.canvas_layers {
            if driftwm::config::applied_rule(cl.surface.wl_surface()).is_some_and(|r| r.no_focus) {
                continue;
            }
            let Some(pos) = cl.position else { continue; };
            let surface_local = canvas_pos - pos.to_f64();
            if let Some((wl_surface, sub_loc)) =
                cl.surface.surface_under(surface_local, WindowSurfaceType::ALL)
            {
                let loc = (sub_loc + pos).to_f64();
                return Some((FocusTarget(wl_surface), loc));
            }
        }
        None
    }

    /// Find a layer surface under the given screen-space position.
    /// Checks the given layers in order.
    ///
    /// Returns a focus target with a *canvas-adjusted* location: smithay computes
    /// surface-local coords as `pointer_pos - focus_loc`, and the pointer is always
    /// in canvas coords, so we offset the screen-space location by `canvas_pos - screen_pos`
    /// to keep the surface-local math correct.
    pub(crate) fn layer_surface_under(
        &self,
        screen_pos: Point<f64, smithay::utils::Logical>,
        canvas_pos: Point<f64, smithay::utils::Logical>,
        layers: &[WlrLayer],
    ) -> Option<(FocusTarget, Point<f64, smithay::utils::Logical>)> {
        let output = self.space.outputs().next()?;
        let map = layer_map_for_output(output);
        for &layer in layers {
            if let Some(surface) = map.layer_under(layer, screen_pos) {
                let geo = map.layer_geometry(surface).unwrap_or_default();
                let surface_local = screen_pos - geo.loc.to_f64();
                if let Some((wl_surface, sub_loc)) =
                    surface.surface_under(surface_local, WindowSurfaceType::ALL)
                {
                    let screen_loc = (sub_loc + geo.loc).to_f64();
                    // Adjust so: canvas_pos - adjusted = screen_pos - screen_loc
                    let adjusted = screen_loc + (canvas_pos - screen_pos);
                    return Some((FocusTarget(wl_surface), adjusted));
                }
            }
        }
        None
    }
}

use smithay::{
    desktop::Window,
    input::{
        pointer::{
            ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
        },
        SeatHandler,
    },
    reexports::wayland_server::Resource,
    utils::{Logical, Point},
};

use driftwm::canvas::{CanvasPos, canvas_to_screen};
use driftwm::config;
use driftwm::snap::{SnapRect, SnapParams, SnapState, update_axis};
use crate::state::DriftWm;

pub struct MoveSurfaceGrab {
    pub start_data: GrabStartData<DriftWm>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
    pub snap: SnapState,
}


impl MoveSurfaceGrab {
    /// Compute edge-pan velocity based on how deep the cursor is into the edge zone.
    /// Deeper = faster (like a joystick). Returns None when cursor is outside the zone.
    pub(crate) fn edge_pan_velocity(
        screen_pos: Point<f64, Logical>,
        output_w: f64,
        output_h: f64,
        edge_zone: f64,
        pan_min: f64,
        pan_max: f64,
    ) -> Option<Point<f64, Logical>> {
        let dist_left = screen_pos.x;
        let dist_right = output_w - screen_pos.x;
        let dist_top = screen_pos.y;
        let dist_bottom = output_h - screen_pos.y;
        let min_dist = dist_left.min(dist_right).min(dist_top).min(dist_bottom);

        if min_dist >= edge_zone {
            return None;
        }

        // Depth into the zone: 0.0 at boundary, 1.0 at viewport edge
        let t = ((edge_zone - min_dist) / edge_zone).clamp(0.0, 1.0);
        // Quadratic ramp — gentle start, fast finish
        let speed = pan_min + (pan_max - pan_min) * t * t;

        // Direction: push away from the nearest edge(s)
        let mut vx = 0.0;
        let mut vy = 0.0;
        if dist_left < edge_zone { vx -= speed * ((edge_zone - dist_left) / edge_zone); }
        if dist_right < edge_zone { vx += speed * ((edge_zone - dist_right) / edge_zone); }
        if dist_top < edge_zone { vy -= speed * ((edge_zone - dist_top) / edge_zone); }
        if dist_bottom < edge_zone { vy += speed * ((edge_zone - dist_bottom) / edge_zone); }

        // Normalize diagonal so it doesn't go √2 faster
        let len = (vx * vx + vy * vy).sqrt();
        if len > speed {
            vx = vx / len * speed;
            vy = vy / len * speed;
        }

        Some(Point::from((vx, vy)))
    }
}

impl PointerGrab<DriftWm> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // Natural position from unmodified cursor delta
        let delta = event.location - self.start_data.location;
        let natural_x = self.initial_window_location.x as f64 + delta.x;
        let natural_y = self.initial_window_location.y as f64 + delta.y;

        let (final_x, final_y) = if !data.config.snap_enabled {
            (natural_x, natural_y)
        } else {
            let zoom = data.zoom;
            let effective_distance = data.config.snap_distance / zoom;
            let effective_break = data.config.snap_break_force / zoom;
            let gap = data.config.snap_gap;

            // Collect other windows' snap rects (exclude self and widgets)
            let self_surface = self.window.toplevel().unwrap().wl_surface().clone();
            let window_size = self.window.geometry().size;
            let self_bar = if data.decorations.contains_key(&self_surface.id()) {
                config::DecorationConfig::TITLE_BAR_HEIGHT
            } else {
                0
            };
            let extent_x = window_size.w as f64;
            let extent_y = window_size.h as f64 + self_bar as f64;

            let mut others: Vec<SnapRect> = Vec::new();
            for w in data.space.elements() {
                let surface = w.toplevel().unwrap().wl_surface();
                if *surface == self_surface {
                    continue;
                }
                if config::applied_rule(surface).is_some_and(|r| r.widget) {
                    continue;
                }
                let Some(loc) = data.space.element_location(w) else { continue };
                let size = w.geometry().size;
                let bar = if data.decorations.contains_key(&surface.id()) {
                    config::DecorationConfig::TITLE_BAR_HEIGHT
                } else {
                    0
                };
                others.push(SnapRect {
                    x_low: loc.x as f64,
                    x_high: loc.x as f64 + size.w as f64,
                    y_low: loc.y as f64 - bar as f64,
                    y_high: loc.y as f64 + size.h as f64,
                });
            }

            // Use natural (un-snapped) positions for perpendicular ranges
            let visual_y = natural_y - self_bar as f64;

            let params_x = SnapParams {
                extent: extent_x,
                perp_low: visual_y,
                perp_high: visual_y + extent_y,
                horizontal: true,
                others: &others,
                gap,
                threshold: effective_distance,
                break_force: effective_break,
            };
            let final_x = update_axis(
                &mut self.snap.x, &mut self.snap.cooldown_x, natural_x, &params_x,
            );

            // Shift y into visual space (title bar top) for snapping,
            // then convert back to geometry origin.
            let params_y = SnapParams {
                extent: extent_y,
                perp_low: natural_x,
                perp_high: natural_x + extent_x,
                horizontal: false,
                others: &others,
                gap,
                threshold: effective_distance,
                break_force: effective_break,
            };
            let final_visual_y = update_axis(
                &mut self.snap.y, &mut self.snap.cooldown_y, visual_y, &params_y,
            );
            let final_y = final_visual_y + self_bar as f64;

            (final_x, final_y)
        };

        let new_loc = Point::from((final_x as i32, final_y as i32));
        data.space.map_element(self.window.clone(), new_loc, false);
        handle.motion(data, None, event);

        // Edge auto-pan detection
        // single-output assumption: edge detection uses first output size
        let screen_pos = canvas_to_screen(CanvasPos(event.location), data.camera, data.zoom).0;
        let output_size = data.space.outputs().next()
            .and_then(|o| o.current_mode())
            .map(|m| m.size.to_logical(1));

        if let Some(size) = output_size {
            let cfg = &data.config;
            data.edge_pan_velocity = Self::edge_pan_velocity(
                screen_pos,
                size.w as f64,
                size.h as f64,
                cfg.edge_zone,
                cfg.edge_pan_min,
                cfg.edge_pan_max,
            );
        }
    }

    fn button(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            data.edge_pan_velocity = None;
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn unset(&mut self, data: &mut DriftWm) {
        data.edge_pan_velocity = None;
    }

    crate::grabs::forward_pointer_grab_methods!();
}


use smithay::{
    desktop::Window,
    input::{
        pointer::{
            ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
        },
        SeatHandler,
    },
    utils::{Logical, Point},
};

use driftwm::canvas::{CanvasPos, canvas_to_screen};
use driftwm::config;
use crate::state::DriftWm;

/// Per-axis snap state: tracks the snapped coordinate and the natural position
/// at the moment of engagement (used for directional break detection).
struct AxisSnap {
    snapped_pos: f64,
    natural_at_engage: f64,
}

/// Snap state for both axes plus cooldown after breaking a snap.
#[derive(Default)]
pub struct SnapState {
    x: Option<AxisSnap>,
    y: Option<AxisSnap>,
    cooldown_x: Option<f64>,
    cooldown_y: Option<f64>,
}

pub struct MoveSurfaceGrab {
    pub start_data: GrabStartData<DriftWm>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
    pub snap: SnapState,
}

/// Find the best snap candidate along one axis.
///
/// Checks 2 edge pairs per other window (adjacent edges). Distance is measured
/// edge-to-edge (without gap offset), so snapping triggers whether approaching
/// from outside or leaving an overlap.
///
/// Returns `Some((snapped_origin, abs_distance))` for the closest candidate
/// within `threshold`, or `None`.
fn find_snap_candidate(
    natural_edge_low: f64,
    extent: f64,
    others: &[(f64, f64)],
    gap: f64,
    threshold: f64,
) -> Option<(f64, f64)> {
    let natural_edge_high = natural_edge_low + extent;
    let mut best: Option<(f64, f64)> = None;

    for &(other_low, other_high) in others {
        // dragged right edge → other left edge
        let snap_origin = other_low - gap - extent;
        let dist = (natural_edge_high - other_low).abs();
        if dist < threshold && best.is_none_or(|(_, bd)| dist < bd) {
            best = Some((snap_origin, dist));
        }

        // dragged left edge → other right edge
        let snap_origin = other_high + gap;
        let dist = (natural_edge_low - other_high).abs();
        if dist < threshold && best.is_none_or(|(_, bd)| dist < bd) {
            best = Some((snap_origin, dist));
        }
    }

    best
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

            // Collect other windows' edges (exclude self and widgets)
            let self_surface = self.window.toplevel().unwrap().wl_surface().clone();
            let window_size = self.window.geometry().size;
            let extent_x = window_size.w as f64;
            let extent_y = window_size.h as f64;

            let mut others_x: Vec<(f64, f64)> = Vec::new();
            let mut others_y: Vec<(f64, f64)> = Vec::new();
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
                others_x.push((loc.x as f64, loc.x as f64 + size.w as f64));
                others_y.push((loc.y as f64, loc.y as f64 + size.h as f64));
            }

            let params_x = SnapParams {
                extent: extent_x,
                others: &others_x,
                gap,
                threshold: effective_distance,
                break_force: effective_break,
            };
            let final_x = Self::update_axis(
                &mut self.snap.x, &mut self.snap.cooldown_x, natural_x, &params_x,
            );

            let params_y = SnapParams {
                extent: extent_y,
                others: &others_y,
                gap,
                threshold: effective_distance,
                break_force: effective_break,
            };
            let final_y = Self::update_axis(
                &mut self.snap.y, &mut self.snap.cooldown_y, natural_y, &params_y,
            );

            (final_x, final_y)
        };

        let new_loc = Point::from((final_x as i32, final_y as i32));
        data.space.map_element(self.window.clone(), new_loc, false);
        handle.motion(data, None, event);

        // Edge auto-pan detection
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

struct SnapParams<'a> {
    extent: f64,
    others: &'a [(f64, f64)],
    gap: f64,
    threshold: f64,
    break_force: f64,
}

impl MoveSurfaceGrab {
    /// Update snap state for a single axis. Returns the final position for that axis.
    fn update_axis(
        snap: &mut Option<AxisSnap>,
        cooldown: &mut Option<f64>,
        natural_pos: f64,
        p: &SnapParams<'_>,
    ) -> f64 {
        if let Some(ref s) = *snap {
            // Directional break: retreat past engagement point OR overshoot past snap
            let (retreat, overshoot) = if s.snapped_pos > s.natural_at_engage {
                (s.natural_at_engage - natural_pos, natural_pos - s.snapped_pos)
            } else {
                (natural_pos - s.natural_at_engage, s.snapped_pos - natural_pos)
            };
            if retreat >= p.break_force || overshoot >= p.break_force {
                *cooldown = Some(s.snapped_pos);
                *snap = None;
                natural_pos
            } else {
                s.snapped_pos
            }
        } else {
            // Clear cooldown when natural position leaves threshold of cooldown coord
            if let Some(cd) = *cooldown
                && (natural_pos - cd).abs() > p.threshold
            {
                *cooldown = None;
            }

            // Try to find a new snap candidate (skip if on cooldown)
            if cooldown.is_none()
                && let Some((snapped_pos, _)) =
                    find_snap_candidate(natural_pos, p.extent, p.others, p.gap, p.threshold)
            {
                *snap = Some(AxisSnap {
                    snapped_pos,
                    natural_at_engage: natural_pos,
                });
                return snapped_pos;
            }

            natural_pos
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_right_edge_to_left_edge() {
        // Window at x=100, width=200 (right edge at 300)
        // Other window starts at x=310
        // With gap=8, snap should place origin at 310-8-200 = 102
        let others = vec![(310.0, 510.0)];
        let result = find_snap_candidate(100.0, 200.0, &others, 8.0, 16.0);
        assert!(result.is_some());
        let (origin, _dist) = result.unwrap();
        assert!((origin - 102.0).abs() < 0.001);
    }

    #[test]
    fn snap_left_edge_to_right_edge() {
        // Window at x=500, width=200
        // Other window ends at x=492
        // With gap=8, snap should place origin at 492+8 = 500
        let others = vec![(200.0, 492.0)];
        let result = find_snap_candidate(500.0, 200.0, &others, 8.0, 16.0);
        assert!(result.is_some());
        let (origin, _dist) = result.unwrap();
        assert!((origin - 500.0).abs() < 0.001);
    }

    #[test]
    fn no_snap_when_too_far() {
        let others = vec![(500.0, 700.0)];
        let result = find_snap_candidate(100.0, 200.0, &others, 8.0, 16.0);
        assert!(result.is_none());
    }

    #[test]
    fn picks_closest_candidate() {
        // Two other windows — edge-to-edge distance picks the closer one
        // Dragged right edge at 300
        let others = vec![
            (310.0, 510.0), // |300 - 310| = 10
            (305.0, 505.0), // |300 - 305| = 5 ← closer
        ];
        let result = find_snap_candidate(100.0, 200.0, &others, 8.0, 16.0);
        assert!(result.is_some());
        let (origin, _) = result.unwrap();
        // Closer: 305 - 8 - 200 = 97
        assert!((origin - 97.0).abs() < 0.001);
    }

    #[test]
    fn snap_break_and_cooldown() {
        let mut snap: Option<AxisSnap> = None;
        let mut cooldown: Option<f64> = None;
        let others = vec![(308.0, 508.0)];
        let p = SnapParams {
            extent: 200.0,
            others: &others,
            gap: 8.0,
            threshold: 16.0,
            break_force: 32.0,
        };

        // Initial engage
        let pos = MoveSurfaceGrab::update_axis(&mut snap, &mut cooldown, 100.0, &p);
        assert!(snap.is_some());
        assert!((pos - 100.0).abs() < 0.001); // 308 - 8 - 200 = 100

        // Small movement — stays snapped
        let pos = MoveSurfaceGrab::update_axis(&mut snap, &mut cooldown, 110.0, &p);
        assert!(snap.is_some());
        assert!((pos - 100.0).abs() < 0.001);

        // Large movement — breaks snap
        let pos = MoveSurfaceGrab::update_axis(&mut snap, &mut cooldown, 140.0, &p);
        assert!(snap.is_none());
        assert!(cooldown.is_some());
        assert!((pos - 140.0).abs() < 0.001);

        // While on cooldown, same edge doesn't re-engage
        let pos = MoveSurfaceGrab::update_axis(&mut snap, &mut cooldown, 105.0, &p);
        assert!(snap.is_none());
        assert!(cooldown.is_some());
        assert!((pos - 105.0).abs() < 0.001);

        // Move far away — cooldown clears
        let _pos = MoveSurfaceGrab::update_axis(&mut snap, &mut cooldown, 200.0, &p);
        assert!(cooldown.is_none());

        // Coming back — can re-engage now
        let pos = MoveSurfaceGrab::update_axis(&mut snap, &mut cooldown, 100.0, &p);
        assert!(snap.is_some());
        assert!((pos - 100.0).abs() < 0.001);
    }

    #[test]
    fn snap_from_inside_does_not_immediately_break() {
        // Window partially overlapping, left edge near other's right edge from inside.
        // Other: [0, 500], dragged: width=200 at x=480, left edge 20px from other right (500)
        // Snap places window just outside: origin = 500 + 12 = 512
        // At engagement: |natural(480) - snapped(512)| = 32 — old abs logic would break instantly
        let mut snap: Option<AxisSnap> = None;
        let mut cooldown: Option<f64> = None;
        let others = vec![(0.0, 500.0)];
        let p = SnapParams {
            extent: 200.0,
            others: &others,
            gap: 12.0,
            threshold: 24.0,
            break_force: 32.0,
        };

        // Engage from inside (left edge at 480, near other right at 500)
        let pos = MoveSurfaceGrab::update_axis(&mut snap, &mut cooldown, 480.0, &p);
        assert!(snap.is_some(), "should engage");
        assert!((pos - 512.0).abs() < 0.001);

        // Continue moving rightward (toward snap) — stays snapped
        let pos = MoveSurfaceGrab::update_axis(&mut snap, &mut cooldown, 500.0, &p);
        assert!(snap.is_some(), "should stay snapped moving toward snap");
        assert!((pos - 512.0).abs() < 0.001);

        // Retreat back past engagement point — breaks
        let pos = MoveSurfaceGrab::update_axis(&mut snap, &mut cooldown, 440.0, &p);
        assert!(snap.is_none(), "should break on retreat past engage point");
        assert!((pos - 440.0).abs() < 0.001);
    }
}

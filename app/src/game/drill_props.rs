//! Physical drill props mapped to the renderer's independent instance API.

use race_core::drill::{self, DrillState, PropKind};
use race_core::Vec3;
use race_render::{DrillPropInst, DrillPropKind};

pub fn instances(drill: &DrillState) -> Vec<DrillPropInst> {
    drill
        .props()
        .into_iter()
        .map(|prop| {
            let (kind, size) = match prop.kind {
                PropKind::Cone => (
                    DrillPropKind::Cone,
                    Vec3::new(2.0 * drill::CONE_RADIUS, drill::CONE_HEIGHT, 2.0 * drill::CONE_RADIUS),
                ),
                PropKind::Pylon => (DrillPropKind::Pylon, Vec3::new(2.0 * drill::PYLON_RADIUS, 4.0, 2.0 * drill::PYLON_RADIUS)),
                PropKind::Log => (
                    DrillPropKind::Log,
                    Vec3::new(2.0 * drill::LOG_HALF_LENGTH, 2.0 * drill::LOG_RADIUS, 2.0 * drill::LOG_RADIUS),
                ),
                PropKind::RingCone => (
                    DrillPropKind::RingCone,
                    Vec3::new(2.0 * drill::RING_CONE_RADIUS, 1.2, 2.0 * drill::RING_CONE_RADIUS),
                ),
                PropKind::Flag => (DrillPropKind::Flag, Vec3::new(1.4, 3.0, 1.0)),
            };
            DrillPropInst {
                kind,
                pos: prop.pos,
                yaw: prop.yaw,
                knocked: prop.knocked,
                size,
            }
        })
        .collect()
}

//! Convert native degrees and pressure without admitting NaN or invalid ranges.
use openharmony_ability::StylusInputData;
use winit_core::event::{Force, TabletToolData, TabletToolTilt};

pub(crate) fn tablet_data(pen: StylusInputData) -> TabletToolData {
    TabletToolData {
        force: pen
            .pressure
            .is_finite()
            .then(|| Force::Normalized(f64::from(pen.pressure.clamp(0.0, 1.0)))),
        tilt: (pen.tilt_x.is_finite() && pen.tilt_y.is_finite()).then(|| TabletToolTilt {
            x: pen.tilt_x.round().clamp(-90.0, 90.0) as i8,
            y: pen.tilt_y.round().clamp(-90.0, 90.0) as i8,
        }),
        twist: pen
            .rotation
            .filter(|r| r.is_finite())
            .map(|r| r.rem_euclid(360.0).floor() as u16),
        ..Default::default()
    }
}
pub(crate) fn tool_kind(
    tool: openharmony_ability::xcomponent::TouchPointTool,
) -> Option<winit_core::event::TabletToolKind> {
    use openharmony_ability::xcomponent::TouchPointTool as Native;
    use winit_core::event::TabletToolKind as Tool;
    Some(match tool {
        Native::Pen => Tool::Pen,
        Native::Rubber => Tool::Eraser,
        Native::Brush => Tool::Brush,
        Native::Pencil => Tool::Pencil,
        Native::Airbrush => Tool::Airbrush,
        Native::Lens => Tool::Lens,
        Native::Unknown | Native::Finger | Native::Mouse => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_pen_units_and_invalid_samples() {
        let d = tablet_data(StylusInputData {
            pressure: 0.25,
            tilt_x: -32.0,
            tilt_y: 96.0,
            rotation: Some(-45.0),
        });
        assert_eq!(d.force, Some(Force::Normalized(0.25)));
        assert_eq!(d.tilt, Some(TabletToolTilt { x: -32, y: 90 }));
        assert_eq!(d.twist, Some(315));
        let d = tablet_data(StylusInputData {
            pressure: f32::NAN,
            tilt_x: f32::INFINITY,
            tilt_y: 0.0,
            rotation: Some(f64::NAN),
        });
        assert_eq!(d.force, None);
        assert_eq!(d.tilt, None);
        assert_eq!(d.twist, None);
    }
}

//! Splitting a physical output into independent workspace zones.
//!
//! A zoned output is subdivided into rectangular zones, configured in `output "name" { zones {
//! ... } }`. Each zone gets its own [`Output`], with its own `wl_output` global, its own monitor
//! and its own workspaces, so from the layout's point of view a zone is just another monitor.
//!
//! The physical output itself stops being a workspace area. It gets no global, it is not mapped
//! into the global space, and it has no monitor, so clients cannot bind to it and input cannot be
//! routed to it. It stays around only as a render target: the zones' contents are composited into
//! its image.

use niri_config::OutputName;
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::backend::VirtualOutputMarker;
use crate::utils::output_size;

/// Marker inserted into a zone output's `Output::user_data()`.
///
/// Its presence is what makes an output a zone output.
#[derive(Debug)]
pub struct ZoneOutputMarker {
    /// The physical output this zone is composited into.
    pub parent: Output,
    /// Where this zone sits in the parent output, as fractions of the parent's logical size.
    pub rect: ZoneRect,
}

/// A zone's rectangle, as fractions of its output's logical size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl ZoneRect {
    pub fn from_config(zone: &niri_config::output::Zone) -> Self {
        Self {
            x: zone.x.0,
            y: zone.y.0,
            width: zone.width.0,
            height: zone.height.0,
        }
    }

    /// Resolves this rectangle against a parent output's logical size.
    ///
    /// Edges are rounded rather than the size, so that zones sharing an edge tile exactly, with no
    /// gap or overlap between them.
    fn resolve(self, parent_size: Size<f64, Logical>) -> Rectangle<i32, Logical> {
        let left = (self.x * parent_size.w).round() as i32;
        let top = (self.y * parent_size.h).round() as i32;
        let right = ((self.x + self.width) * parent_size.w).round() as i32;
        let bottom = ((self.y + self.height) * parent_size.h).round() as i32;

        // The config validates that zones are non-degenerate, but rounding on a very small output
        // can still collapse one, and an output with a zero-sized mode is not something the rest
        // of niri copes with.
        let width = (right - left).max(1);
        let height = (bottom - top).max(1);

        Rectangle::new(Point::from((left, top)), Size::from((width, height)))
    }
}

/// Returns whether this output is a zone of some physical output.
pub fn is_zone_output(output: &Output) -> bool {
    output.user_data().get::<ZoneOutputMarker>().is_some()
}

/// Returns the physical output that this zone output is composited into.
pub fn parent_of_zone(output: &Output) -> Option<&Output> {
    output
        .user_data()
        .get::<ZoneOutputMarker>()
        .map(|marker| &marker.parent)
}

/// Returns the output that actually gets rendered and presented for this output.
///
/// For a zone output that is its parent physical output, since zones don't have a scanout pipeline
/// of their own. For anything else it's the output itself.
pub fn render_target_of(output: &Output) -> &Output {
    parent_of_zone(output).unwrap_or(output)
}

/// Returns this zone's rectangle within its parent output, in the parent's logical coordinates.
pub fn zone_geometry(output: &Output) -> Option<Rectangle<i32, Logical>> {
    let marker = output.user_data().get::<ZoneOutputMarker>()?;
    Some(marker.rect.resolve(output_size(&marker.parent)))
}

/// The name of the zone output for `zone_name` on `parent_name`.
pub fn zone_output_name(parent_name: &str, zone_name: &str) -> String {
    format!("{parent_name}:{zone_name}")
}

/// Builds the zone outputs for a physical output.
///
/// The parent's scale is inherited as-is. Its transform is not: the parent's logical size already
/// has the transform applied, and zones are laid out in that logical space, so applying it again
/// would rotate each zone within itself.
pub fn build_zone_outputs(parent: &Output, zones: &niri_config::output::Zones) -> Vec<Output> {
    let parent_name = parent.user_data().get::<OutputName>().unwrap();
    let parent_size = output_size(parent);
    let scale = parent.current_scale().fractional_scale();

    let physical_properties = parent.physical_properties();
    let refresh = parent.current_mode().map_or(60_000, |mode| mode.refresh);

    let mut outputs = Vec::with_capacity(zones.zones.len());
    for zone in &zones.zones {
        let rect = ZoneRect::from_config(zone);
        let geo = rect.resolve(parent_size);

        let connector = zone_output_name(&parent_name.connector, &zone.name);
        let make = physical_properties.make.clone();
        let model = physical_properties.model.clone();
        let serial = zone.name.clone();

        let output = Output::new(
            connector.clone(),
            PhysicalProperties {
                // A zone covers part of a physical screen, so there is no meaningful physical size
                // to report. Leaving it at zero also keeps scale guessing from kicking in; the
                // parent's scale is applied explicitly below.
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: make.clone(),
                model: model.clone(),
                serial_number: serial.clone(),
            },
        );

        let mode = Mode {
            size: Size::from((
                (f64::from(geo.size.w) * scale).round() as i32,
                (f64::from(geo.size.h) * scale).round() as i32,
            )),
            refresh,
        };
        output.set_preferred(mode);
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Fractional(scale)),
            None,
        );

        output.user_data().insert_if_missing(|| OutputName {
            connector,
            make: Some(make),
            model: Some(model),
            serial: Some(serial),
        });

        output.user_data().insert_if_missing(|| ZoneOutputMarker {
            parent: parent.clone(),
            rect,
        });

        // Zone outputs have no scanout pipeline of their own, same as headless virtual outputs.
        output
            .user_data()
            .insert_if_missing(VirtualOutputMarker::default);

        outputs.push(output);
    }

    outputs
}

/// Updates a zone output's mode and scale after its parent output changed size or scale.
pub fn update_zone_output_size(output: &Output) {
    let Some(marker) = output.user_data().get::<ZoneOutputMarker>() else {
        return;
    };

    let scale = marker.parent.current_scale().fractional_scale();
    let geo = marker.rect.resolve(output_size(&marker.parent));
    let refresh = marker
        .parent
        .current_mode()
        .map_or(60_000, |mode| mode.refresh);

    let mode = Mode {
        size: Size::from((
            (f64::from(geo.size.w) * scale).round() as i32,
            (f64::from(geo.size.h) * scale).round() as i32,
        )),
        refresh,
    };

    if output.current_mode() == Some(mode) && output.current_scale().fractional_scale() == scale {
        return;
    }

    // Smithay keeps every mode an output ever had, which is right for a real connector, but a zone
    // only ever has the one derived from its parent.
    output.set_preferred(mode);
    output.change_current_state(Some(mode), None, Some(Scale::Fractional(scale)), None);
    for other in output.modes() {
        if other != mode {
            output.delete_mode(other);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> ZoneRect {
        ZoneRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn zones_cover_their_output_exactly() {
        let size = Size::from((1920., 1080.));

        let left = rect(0., 0., 0.5, 1.).resolve(size);
        let right = rect(0.5, 0., 0.5, 1.).resolve(size);

        assert_eq!(left.loc.x, 0);
        assert_eq!(left.size.w, 960);
        assert_eq!(right.loc.x, 960);
        assert_eq!(left.size.w + right.size.w, 1920);
    }

    #[test]
    fn adjacent_zones_tile_an_odd_sized_output() {
        // 1365 doesn't divide into thirds, so rounding has to land somewhere. Rounding the shared
        // edges rather than the widths is what keeps the zones touching.
        let size = Size::from((1365., 768.));

        let a = rect(0., 0., 1. / 3., 1.).resolve(size);
        let b = rect(1. / 3., 0., 1. / 3., 1.).resolve(size);
        let c = rect(2. / 3., 0., 1. / 3., 1.).resolve(size);

        assert_eq!(b.loc.x, a.loc.x + a.size.w);
        assert_eq!(c.loc.x, b.loc.x + b.size.w);
        assert_eq!(a.size.w + b.size.w + c.size.w, 1365);
    }

    #[test]
    fn a_zone_is_never_zero_sized() {
        // Small enough that this zone rounds away entirely.
        let zone = rect(0., 0., 0.001, 0.001).resolve(Size::from((100., 100.)));
        assert_eq!(zone.size.w, 1);
        assert_eq!(zone.size.h, 1);
    }

    #[test]
    fn zones_can_be_stacked_vertically() {
        let size = Size::from((1920., 1080.));

        let top = rect(0., 0., 1., 0.7).resolve(size);
        let bottom = rect(0., 0.7, 1., 0.3).resolve(size);

        assert_eq!(top.size, Size::from((1920, 756)));
        assert_eq!(bottom.loc.y, top.loc.y + top.size.h);
        assert_eq!(top.size.h + bottom.size.h, 1080);
    }
}

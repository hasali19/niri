use niri_config::output::{Zone, Zones};
use niri_config::utils::Percent;
use niri_config::Config;
use smithay::backend::renderer::Color32F;
use smithay::output::Output;

use super::*;

fn zone(name: &str, x: f64, y: f64, width: f64, height: f64) -> Zone {
    Zone {
        name: String::from(name),
        x: Percent(x),
        y: Percent(y),
        width: Percent(width),
        height: Percent(height),
    }
}

/// Config with `headless-1` split into a left and a right half.
fn halves_config() -> Config {
    let mut config = Config::default();
    config.outputs.0.push(niri_config::output::Output {
        name: String::from("headless-1"),
        zones: Some(Zones {
            zones: vec![
                zone("left", 0., 0., 0.5, 1.),
                zone("right", 0.5, 0., 0.5, 1.),
            ],
        }),
        ..Default::default()
    });
    config
}

fn output_named(f: &mut Fixture, name: &str) -> Output {
    f.niri()
        .output_state
        .keys()
        .find(|output| output.name() == name)
        .expect("output not found")
        .clone()
}

#[test]
fn zones_become_outputs_and_the_physical_output_steps_aside() {
    let mut f = Fixture::with_config(halves_config());
    f.add_output(1, (1920, 1080));

    let physical = output_named(&mut f, "headless-1");
    let left = output_named(&mut f, "headless-1:left");
    let right = output_named(&mut f, "headless-1:right");

    // The zones are what the rest of niri sees as screens.
    let in_global_space = |f: &mut Fixture, output: &Output| {
        f.niri().global_space.outputs().any(|other| other == output)
    };
    assert!(in_global_space(&mut f, &left));
    assert!(in_global_space(&mut f, &right));
    assert!(!in_global_space(&mut f, &physical));

    // Each zone gets its own monitor, and the physical output gets none.
    assert!(f.niri().layout.monitor_for_output(&left).is_some());
    assert!(f.niri().layout.monitor_for_output(&right).is_some());
    assert!(f.niri().layout.monitor_for_output(&physical).is_none());

    // Clients can bind to the zones, but not to the output they are composited into.
    assert!(f.niri().output_state[&left].global.is_some());
    assert!(f.niri().output_state[&right].global.is_some());
    assert!(f.niri().output_state[&physical].global.is_none());
}

#[test]
fn zones_tile_their_output() {
    let mut f = Fixture::with_config(halves_config());
    f.add_output(1, (1920, 1080));

    let physical = output_named(&mut f, "headless-1");
    let left = output_named(&mut f, "headless-1:left");
    let right = output_named(&mut f, "headless-1:right");

    let physical_geo = f.niri().output_geometry(&physical).unwrap();
    let left_geo = f.niri().global_space.output_geometry(&left).unwrap();
    let right_geo = f.niri().global_space.output_geometry(&right).unwrap();

    assert_eq!(left_geo.loc, physical_geo.loc);
    assert_eq!(left_geo.size.w, 960);
    assert_eq!(left_geo.size.h, 1080);

    // The right zone starts exactly where the left one ends: no gap, no overlap.
    assert_eq!(right_geo.loc.x, left_geo.loc.x + left_geo.size.w);
    assert_eq!(right_geo.loc.y, physical_geo.loc.y);
    assert_eq!(right_geo.size, left_geo.size);

    // Together they cover the output exactly.
    assert_eq!(left_geo.size.w + right_geo.size.w, physical_geo.size.w);
}

#[test]
fn input_never_lands_on_a_zoned_output() {
    let mut f = Fixture::with_config(halves_config());
    f.add_output(1, (1920, 1080));

    let physical = output_named(&mut f, "headless-1");
    let left = output_named(&mut f, "headless-1:left");
    let right = output_named(&mut f, "headless-1:right");

    // Every point of the screen resolves to one of the zones.
    for (x, expected) in [(10., &left), (950., &left), (970., &right), (1910., &right)] {
        let (output, _) = f.niri().output_under((x, 500.).into()).unwrap();
        assert_eq!(output, expected, "at x={x}");
        assert_ne!(*output, physical, "at x={x}");
    }
}

#[test]
fn zones_follow_their_output_across_a_resize() {
    let mut f = Fixture::with_config(halves_config());
    f.add_output(1, (1920, 1080));

    let physical = output_named(&mut f, "headless-1");
    let left = output_named(&mut f, "headless-1:left");
    let right = output_named(&mut f, "headless-1:right");

    let mode = smithay::output::Mode {
        size: (3840, 2160).into(),
        refresh: 60_000,
    };
    physical.change_current_state(Some(mode), None, None, None);
    f.niri().output_resized(&physical);

    let left_geo = f.niri().global_space.output_geometry(&left).unwrap();
    let right_geo = f.niri().global_space.output_geometry(&right).unwrap();

    assert_eq!(left_geo.size.w, 1920);
    assert_eq!(left_geo.size.h, 2160);
    assert_eq!(right_geo.loc.x, left_geo.loc.x + left_geo.size.w);
    assert_eq!(right_geo.size, left_geo.size);
}

#[test]
fn zones_inherit_their_output_scale() {
    let mut config = halves_config();
    config.outputs.0[0].scale = Some(niri_config::FloatOrInt(2.));

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    let left = output_named(&mut f, "headless-1:left");

    // A zone reports no physical size, so if it went through the usual scale guessing it would
    // come out as 1 and the zone would be drawn at the wrong size.
    assert_eq!(left.current_scale().fractional_scale(), 2.);

    // Half of the output's 960 logical points, at scale 2.
    let geo = f.niri().global_space.output_geometry(&left).unwrap();
    assert_eq!(geo.size.w, 480);
    assert_eq!(left.current_mode().unwrap().size.w, 960);
}

#[test]
fn zones_inherit_their_output_config() {
    let mut config = halves_config();
    let backdrop = niri_config::Color::new_unpremul(1., 0., 0., 1.);
    config.outputs.0[0].backdrop_color = Some(backdrop);

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    // Settings written for a display apply to the zones it is split into, rather than silently
    // doing nothing because the zones have config sections of their own.
    let left = output_named(&mut f, "headless-1:left");
    let right = output_named(&mut f, "headless-1:right");
    let expected = Color32F::from({
        let mut c = backdrop.to_array_unpremul();
        c[3] = 1.;
        c
    });

    assert_eq!(
        f.niri().output_state[&left].backdrop_buffer.color(),
        expected
    );
    assert_eq!(
        f.niri().output_state[&right].backdrop_buffer.color(),
        expected
    );
}

#[test]
fn ipc_reports_zones_and_their_output() {
    let mut f = Fixture::with_config(halves_config());
    f.add_output(1, (1920, 1080));
    f.niri_state().refresh_ipc_outputs();

    let state = f.niri_state();
    let ipc_outputs = state.backend.ipc_outputs();
    let ipc_outputs = ipc_outputs.lock().unwrap();
    let by_name = |name: &str| {
        ipc_outputs
            .values()
            .find(|output| output.name == name)
            .unwrap_or_else(|| panic!("{name} missing from IPC outputs"))
            .clone()
    };

    // The output is still listed — it's the user's hardware — but as something split into zones
    // rather than as a place windows can go.
    let physical = by_name("headless-1");
    assert_eq!(
        physical.zones,
        vec!["headless-1:left", "headless-1:right"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    );
    assert_eq!(physical.zone_of, None);
    assert!(physical.logical.is_none());

    // The zones are listed as outputs of their own, pointing back at it.
    let left = by_name("headless-1:left");
    assert_eq!(left.zone_of.as_deref(), Some("headless-1"));
    assert!(left.zones.is_empty());
    let logical = left.logical.expect("a zone has a logical output");
    assert_eq!(logical.width, 960);
    assert_eq!(logical.height, 1080);
}

#[test]
fn focus_and_frame_callbacks_go_to_zones() {
    let mut f = Fixture::with_config(halves_config());
    f.add_output(1, (1920, 1080));

    let physical = output_named(&mut f, "headless-1");
    let left = output_named(&mut f, "headless-1:left");
    let right = output_named(&mut f, "headless-1:right");

    // Everything that picks an output to focus or step through works off this list, so it has to
    // hold the zones rather than the output they are composited into.
    assert_eq!(f.niri().sorted_outputs, vec![left.clone(), right.clone()]);

    // Startup focus picks the first of those, and warps the cursor onto it.
    f.niri_state().focus_default_monitor();
    assert_eq!(f.niri().layout.active_output(), Some(&left));

    // Backends notify the output they scan out, which for a zoned output is not where any of the
    // surfaces live.
    f.niri().send_frame_callbacks(&physical);
    f.niri().send_frame_callbacks_for_virtual_output(&physical);
}

#[test]
fn removing_an_output_removes_its_zones() {
    let mut f = Fixture::with_config(halves_config());
    f.add_output(1, (1920, 1080));

    let physical = output_named(&mut f, "headless-1");
    f.niri().remove_output(&physical);

    assert!(f.niri().output_state.is_empty());
    assert!(f.niri().zoned_outputs.is_empty());
    assert_eq!(f.niri().global_space.outputs().count(), 0);
}

#[test]
fn config_reload_can_add_and_remove_zones() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Starts out as an ordinary, unzoned output.
    let physical = output_named(&mut f, "headless-1");
    assert!(f.niri().layout.monitor_for_output(&physical).is_some());
    assert!(f.niri().zoned_outputs.is_empty());

    // Splitting it into zones takes its monitor away and gives one to each zone.
    f.niri_state().reload_config(Ok(halves_config()));

    let physical = output_named(&mut f, "headless-1");
    let left = output_named(&mut f, "headless-1:left");
    assert!(f.niri().layout.monitor_for_output(&physical).is_none());
    assert!(f.niri().layout.monitor_for_output(&left).is_some());
    assert_eq!(f.niri().zoned_outputs.len(), 1);

    // Taking the zones away again gives the output its monitor back.
    f.niri_state().reload_config(Ok(Config::default()));

    let physical = output_named(&mut f, "headless-1");
    assert!(f.niri().layout.monitor_for_output(&physical).is_some());
    assert!(f.niri().zoned_outputs.is_empty());
    assert!(f
        .niri()
        .output_state
        .keys()
        .all(|output| !output.name().contains(':')));
}

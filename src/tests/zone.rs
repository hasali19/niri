use niri_config::output::{Zone, Zones};
use niri_config::utils::Percent;
use niri_config::Config;
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

use sift_wm::*;
use std::collections::BTreeMap;

#[test]
fn binding_sequence_waits_for_missing_target_before_acting_on_a_window() {
    let mut e = engine();
    let other = window(2, "other.app", "Other", 650.);
    e.add_window(other.clone()).expect("valid fixture");
    execute(&mut e, "focus --window-id 2");
    let sequence = Command::Sequence(vec![
        parse("focus --target terminal").expect("valid fixture"),
        parse("snap left").expect("valid fixture"),
    ]);
    let effects = e.execute(sequence).expect("valid fixture");
    assert_eq!(e.windows[&2].frame, other.frame);
    let [Effect::LaunchTarget { command, .. }] = effects.as_slice() else {
        panic!("expected a pending launch");
    };
    e.add_window(window(50, "com.apple.Terminal", "project reopened", 100.))
        .expect("valid fixture");
    let effects = e.execute((**command).clone()).expect("valid fixture");
    assert!(effects.contains(&Effect::Focus(50)));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Frame(50, _)))
    );
    assert_eq!(e.windows[&2].frame, other.frame);
}

fn monitor(id: u32, x: f64) -> Monitor {
    Monitor {
        id,
        name: format!("Display {id}"),
        frame: Rect::new(x, 0., 1200., 900.),
        usable: Rect::new(x, 24., 1200., 850.),
        main: id == 1,
    }
}
fn config() -> Config {
    Config {
        enabled: true,
        targets: BTreeMap::from([(
            "terminal".into(),
            WindowTarget {
                app_id: Some("com.apple.Terminal".into()),
                title: Some("^project".into()),
                ..WindowTarget::default()
            },
        )]),
        ..Config::default()
    }
}
fn engine() -> Engine {
    Engine::new(config(), vec![monitor(1, 0.), monitor(2, 1200.)]).expect("valid fixture")
}
fn window(id: u32, app_id: &str, title: &str, x: f64) -> Window {
    let mut w = Window::new(id, 100, Rect::new(x, 100., 500., 400.));
    w.app_id = app_id.into();
    w.app_name = "Terminal".into();
    w.title = title.into();
    w
}
fn execute(engine: &mut Engine, command: &str) -> Vec<Effect> {
    engine
        .execute(parse(command).expect("valid fixture"))
        .expect("valid fixture")
}

#[test]
fn manual_mode_never_reflows_parks_routes_or_constrains_observed_windows() {
    let mut config = config();
    config.rules.push(Rule {
        workspace: Some("hidden".into()),
        floating: Some(false),
        ..Rule::default()
    });
    let mut e = Engine::new(config, vec![monitor(1, 0.)]).expect("valid fixture");
    let first = window(1, "com.apple.Terminal", "project", -40.);
    e.add_window(first.clone()).expect("valid fixture");
    e.add_window(window(2, "other.app", "Other", 650.))
        .expect("valid fixture");
    e.observed_focus(1).expect("valid fixture");
    assert_eq!(e.windows[&1].frame, first.frame);
    assert_ne!(e.windows[&1].workspace, "hidden");
    let mut moved = first.clone();
    moved.frame.x = -150.;
    e.update_window(moved.clone());
    e.set_monitors(vec![monitor(1, 0.), monitor(2, 1200.)])
        .expect("valid fixture");
    e.remove_window(2);
    assert!(e.reflow().is_empty());
    assert!(e.hidden().is_empty());
    assert_eq!(e.windows[&1].frame, moved.frame);
    assert!(
        e.execute(parse("workspace 2").expect("valid fixture"))
            .is_err()
    );
    assert!(e.reflow().is_empty());
    e.verify().expect("valid fixture");
}

#[test]
fn saved_target_survives_window_recreation_pid_changes_and_title_updates() {
    let mut e = engine();
    e.add_window(window(1, "com.apple.Terminal", "project one", 100.))
        .expect("valid fixture");
    e.add_window(window(2, "other.app", "project unrelated", 600.))
        .expect("valid fixture");
    assert_eq!(
        execute(&mut e, "focus --target terminal"),
        [Effect::Focus(1)]
    );
    e.remove_window(1);
    let mut reopened = window(40, "com.apple.Terminal", "project reopened", 100.);
    reopened.pid = 900;
    e.add_window(reopened.clone()).expect("valid fixture");
    assert_eq!(
        execute(&mut e, "focus --target terminal"),
        [Effect::Focus(40)]
    );
    reopened.title = "different document".into();
    e.update_window(reopened);
    assert_eq!(e.resolve_target("terminal").expect("valid fixture"), None);
    assert_eq!(e.windows[&2].frame.x, 600.);
}

#[test]
fn target_ties_prefer_focused_then_recently_focused_without_arbitrary_app_matches() {
    let mut e = engine();
    for id in 1..=3 {
        e.add_window(window(
            id,
            "com.apple.Terminal",
            "project",
            100. * id as f64,
        ))
        .expect("valid fixture");
    }
    assert_eq!(
        e.resolve_target("terminal").expect("valid fixture"),
        Some(1)
    );
    execute(&mut e, "focus --window-id 2");
    execute(&mut e, "focus --window-id 3");
    assert_eq!(
        e.resolve_target("terminal").expect("valid fixture"),
        Some(3)
    );
    e.remove_window(3);
    assert_eq!(
        e.resolve_target("terminal").expect("valid fixture"),
        Some(2)
    );
}

#[test]
fn missing_target_requests_launch_but_never_launches_for_close_or_minimize() {
    let mut e = engine();
    let effects = execute(&mut e, "focus --target terminal");
    assert!(
        matches!(&effects[..], [Effect::LaunchTarget { name, app_id, command }] if name == "terminal" && app_id == "com.apple.Terminal" && **command == Command::FocusTarget("terminal".into()))
    );
    assert!(matches!(
        &execute(&mut e, "snap left --target terminal")[..],
        [Effect::LaunchTarget { .. }]
    ));
    assert!(
        e.execute(parse("close --target terminal").expect("valid fixture"))
            .is_err()
    );
    assert!(
        e.execute(parse("macos-native-minimize --target terminal").expect("valid fixture"))
            .is_err()
    );
    let mut config = config();
    config
        .targets
        .get_mut("terminal")
        .expect("valid fixture")
        .launch = false;
    e.reconfigure(config).expect("valid fixture");
    assert!(
        e.execute(parse("focus --target terminal").expect("valid fixture"))
            .is_err()
    );
}

#[test]
fn manual_snap_resize_and_monitor_move_only_change_the_selected_window() {
    let mut e = engine();
    let first = window(1, "com.apple.Terminal", "project", 100.);
    let other = window(2, "other.app", "Other", 650.);
    e.add_window(first.clone()).expect("valid fixture");
    e.add_window(other.clone()).expect("valid fixture");
    execute(&mut e, "focus --window-id 2");
    let effects = execute(&mut e, "snap left --target terminal");
    assert_eq!(effects, [Effect::Frame(1, Rect::new(0., 24., 600., 850.))]);
    assert_eq!(e.focused, Some(2));
    assert_eq!(e.windows[&2].frame, other.frame);
    execute(&mut e, "resize width 450 --target terminal");
    assert_eq!(e.windows[&1].frame.width, 450.);
    execute(&mut e, "snap restore --target terminal");
    assert_eq!(e.windows[&1].frame, first.frame);
    execute(&mut e, "move-node-to-monitor next --target terminal");
    assert_eq!(e.windows[&1].frame.x, 1300.);
    assert_eq!(e.windows[&2].frame, other.frame);
    assert!(e.reflow().is_empty());
}

#[test]
fn manual_fullscreen_restores_pre_fullscreen_geometry_and_pause_does_not_move_windows() {
    let mut e = engine();
    e.add_window(window(1, "com.apple.Terminal", "project", 100.))
        .expect("valid fixture");
    execute(&mut e, "focus --target terminal");
    execute(&mut e, "snap left");
    let snapped = e.windows[&1].frame;
    execute(&mut e, "fullscreen");
    assert_eq!(e.windows[&1].frame, e.monitors[&1].usable);
    execute(&mut e, "fullscreen");
    assert_eq!(e.windows[&1].frame, snapped);
    assert!(execute(&mut e, "enable off").is_empty());
    assert_eq!(e.windows[&1].frame, snapped);
    assert!(
        e.execute(parse("focus --target terminal").expect("valid fixture"))
            .is_err()
    );
}

#[test]
fn only_explicit_tiling_opt_in_reflows_and_manual_positions_become_restore_positions() {
    let mut e = engine();
    let first = window(1, "com.apple.Terminal", "project", 100.);
    e.add_window(first).expect("valid fixture");
    execute(&mut e, "focus --target terminal");
    execute(&mut e, "snap right");
    let manual = e.windows[&1].frame;
    let mut config = config();
    config.automatic_tiling = true;
    e.reconfigure(config.clone()).expect("valid fixture");
    assert_eq!(e.windows[&1].original_frame, manual);
    assert!(!e.reflow().is_empty());
    config.automatic_tiling = false;
    e.reconfigure(config).expect("valid fixture");
    assert_eq!(e.windows[&1].frame, manual);
    assert!(e.reflow().is_empty());
}

#[test]
fn manual_bindings_are_independent_and_configuration_rejects_bad_targets_and_shell_wrapping() {
    let config = config();
    assert!(!config.automatic_tiling);
    let manual = config.bindings_for_mode("main").expect("valid fixture");
    assert!(manual.contains_key("ctrl-alt-h"));
    assert!(manual.contains_key("ctrl-alt-left"));
    assert!(!manual.contains_key("ctrl-alt-1"));
    assert!(!manual.contains_key("ctrl-alt-slash"));
    let mut bad = config.clone();
    bad.targets
        .get_mut("terminal")
        .expect("valid fixture")
        .title = Some("[".into());
    assert!(bad.validate().is_err());
    bad = config.clone();
    bad.bindings
        .get_mut("main")
        .expect("valid fixture")
        .insert("ctrl-alt-t".into(), vec!["focus --target missing".into()]);
    assert!(bad.validate().is_err());
    assert!(parse("focus --target terminal --window-id 4").is_err());
    assert!(
        protocol_command(&[
            "exec-and-forget".into(),
            "echo no".into(),
            "--target".into(),
            "terminal".into()
        ])
        .is_err()
    );
    assert!(config.validate().is_ok());
}

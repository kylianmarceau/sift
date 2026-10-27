use sift_wm::*;

fn monitor(id: u32, x: f64) -> Monitor {
    Monitor {
        id,
        name: format!("Display {id}"),
        frame: Rect::new(x, 0., 1200., 900.),
        usable: Rect::new(x, 24., 1200., 850.),
        main: id == 1,
    }
}
fn tiling_config() -> Config {
    Config {
        automatic_tiling: true,
        ..Config::default()
    }
}
fn engine() -> Engine {
    Engine::new(tiling_config(), vec![monitor(1, 0.)]).expect("valid fixture")
}
fn add(e: &mut Engine, id: u32) {
    let mut w = Window::new(id, 100, Rect::new(10., 40., 500., 400.));
    w.app_id = "test.app".into();
    w.app_name = "Test".into();
    w.title = format!("Window {id}");
    e.add_window(w).expect("valid fixture");
    e.observed_focus(id).expect("valid fixture");
    e.reflow();
}

#[test]
fn workspace_shortcut_focuses_existing_assigned_window_and_launches_missing_app() {
    let mut config = tiling_config();
    config.rules.push(Rule {
        app_id: Some(r"^company\.thebrowser\.dia$".into()),
        workspace: Some("1".into()),
        ..Rule::default()
    });
    let mut e = Engine::new(config, vec![monitor(1, 0.)]).unwrap();
    let effects = e.execute(Command::Workspace("1".into())).unwrap();
    assert!(effects.iter().any(|effect| matches!(effect, Effect::LaunchWorkspaceApp { workspace, app_id } if workspace == "1" && app_id == "company.thebrowser.dia")));
    let mut dia = Window::new(17, 20, Rect::new(20., 40., 600., 400.));
    dia.app_id = "company.thebrowser.dia".into();
    dia.app_name = "Dia".into();
    e.add_window(dia).unwrap();
    // An unrelated, more recently focused window must not steal the shortcut.
    add(&mut e, 18);
    e.execute(Command::Workspace("2".into())).unwrap();
    let effects = e.execute(Command::Workspace("1".into())).unwrap();
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Focus(17)))
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::LaunchWorkspaceApp { .. }))
    );
    // Repeating the shortcut still focuses the existing window.
    assert!(
        e.execute(Command::Workspace("1".into()))
            .unwrap()
            .iter()
            .any(|effect| matches!(effect, Effect::Focus(17)))
    );
    e.remove_window(17);
    assert!(
        e.execute(Command::Workspace("1".into()))
            .unwrap()
            .iter()
            .any(|effect| matches!(effect, Effect::LaunchWorkspaceApp { .. }))
    );
}

#[test]
fn workspace_shortcut_does_not_launch_general_regex_or_excluded_apps() {
    let mut config = tiling_config();
    config.rules.push(Rule {
        app_id: Some(".*".into()),
        workspace: Some("1".into()),
        ..Rule::default()
    });
    config.rules.push(Rule {
        app_id: Some(r"^com\.example\.blocked$".into()),
        workspace: Some("1".into()),
        ..Rule::default()
    });
    config.exclude_apps.push(r"^com\.example\.blocked$".into());
    let mut e = Engine::new(config, vec![monitor(1, 0.)]).unwrap();
    assert!(
        !e.execute(Command::Workspace("1".into()))
            .unwrap()
            .iter()
            .any(|effect| matches!(effect, Effect::LaunchWorkspaceApp { .. }))
    );
}

#[test]
fn workspace_launch_respects_first_matching_app_rule() {
    let mut config = tiling_config();
    for workspace in ["2", "1"] {
        config.rules.push(Rule {
            app_id: Some(r"^com\.example\.app$".into()),
            workspace: Some(workspace.into()),
            ..Rule::default()
        });
    }
    let mut e = Engine::new(config, vec![monitor(1, 0.)]).unwrap();
    assert!(
        !e.execute(Command::Workspace("1".into()))
            .unwrap()
            .iter()
            .any(|effect| matches!(effect, Effect::LaunchWorkspaceApp { .. }))
    );
    assert!(
        e.execute(Command::Workspace("2".into()))
            .unwrap()
            .iter()
            .any(|effect| matches!(effect, Effect::LaunchWorkspaceApp { .. }))
    );
}
fn command(e: &mut Engine, text: &str) {
    e.execute(parse(text).expect("valid fixture"))
        .expect("valid fixture");
    e.verify().expect("valid fixture");
}

#[test]
fn tiling_respects_work_area_gaps_and_rebalances_after_removal() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    let a = e.windows[&1].frame;
    let b = e.windows[&2].frame;
    assert_eq!(a.x, 8.);
    assert_eq!(a.y, 32.);
    assert_eq!(a.height, 834.);
    assert_eq!(a.right() + 8., b.x);
    assert_eq!(b.right(), 1192.);
    e.remove_window(1);
    e.reflow();
    e.verify().expect("valid fixture");
    assert_eq!(e.windows[&2].frame.width, 1184.);
}

#[test]
fn nested_splits_survive_window_lifecycle() {
    let mut e = engine();
    add(&mut e, 1);
    command(&mut e, "split vertical");
    add(&mut e, 2);
    assert_eq!(e.windows[&1].frame.width, e.windows[&2].frame.width);
    assert!(e.windows[&1].frame.y < e.windows[&2].frame.y);
    e.remove_window(1);
    e.reflow();
    add(&mut e, 3);
    e.verify().expect("valid fixture");
    assert!(e.windows[&2].frame.valid());
    assert!(e.windows[&3].frame.valid());
}

#[test]
fn workspace_switching_hides_without_minimizing_and_keeps_focus_history() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    command(&mut e, "move-node-to-workspace 2");
    assert_eq!(e.hidden(), [2]);
    assert_eq!(e.focused, Some(1));
    assert!(!e.windows[&2].minimized);
    command(&mut e, "workspace 2");
    assert_eq!(e.focused, Some(2));
    assert_eq!(e.hidden(), [1]);
    command(&mut e, "workspace-back-and-forth");
    assert_eq!(e.focused, Some(1));
    command(&mut e, "workspace-back-and-forth");
    assert_eq!(e.focused, Some(2));
}

#[test]
fn focus_by_id_reveals_another_workspace() {
    let mut e = engine();
    add(&mut e, 1);
    command(&mut e, "workspace 2");
    add(&mut e, 2);
    command(&mut e, "focus --window-id 1");
    assert_eq!(e.visible[&1], "1");
    assert_eq!(e.focused, Some(1));
}

#[test]
fn floating_minimized_and_fullscreen_windows_leave_and_rejoin_the_tree() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    command(&mut e, "layout floating");
    assert!(e.windows[&2].floating);
    assert_eq!(e.windows[&1].frame.width, 1184.);
    command(&mut e, "layout tiling");
    assert!(!e.windows[&2].floating);
    let mut w = e.windows[&2].clone();
    w.minimized = true;
    e.update_window(w);
    e.reflow();
    e.verify().expect("valid fixture");
    assert_eq!(e.windows[&1].frame.width, 1184.);
    let mut w = e.windows[&2].clone();
    w.minimized = false;
    e.update_window(w);
    e.reflow();
    e.verify().expect("valid fixture");
    assert!(e.windows[&1].frame.width < 1184.);
}

#[test]
fn direction_navigation_and_swap_use_layout_geometry() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    add(&mut e, 3);
    command(&mut e, "focus left");
    assert_eq!(e.focused, Some(2));
    let old = e.windows[&2].frame;
    command(&mut e, "move left");
    assert!(e.windows[&2].frame.x < old.x);
    command(&mut e, "focus-back-and-forth");
    assert_eq!(e.focused, Some(3));
}

#[test]
fn fullscreen_produces_one_final_frame_and_restores_tiles() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    let effects = e
        .execute(parse("fullscreen on").expect("valid fixture"))
        .expect("valid fixture");
    assert_eq!(
        effects
            .iter()
            .filter(|f| matches!(f, Effect::Frame(2, _)))
            .count(),
        1
    );
    assert_eq!(e.windows[&2].frame.width, 1184.);
    command(&mut e, "fullscreen off");
    assert!(e.windows[&2].frame.width < 1184.);
}

#[test]
fn accordion_focus_exposes_selected_window() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    add(&mut e, 3);
    command(&mut e, "layout accordion");
    assert_eq!(e.windows[&3].frame.width, 1184. - 60.);
    command(&mut e, "focus --window-id 1");
    assert_eq!(e.windows[&1].frame.x, 8.);
    assert!(e.windows[&2].frame.x > e.windows[&1].frame.x);
}

#[test]
fn resize_and_balance_preserve_total_space() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    let width = e.windows[&2].frame.width;
    command(&mut e, "resize width +100");
    assert!(e.windows[&2].frame.width > width);
    command(&mut e, "balance-sizes");
    assert!((e.windows[&1].frame.width - e.windows[&2].frame.width).abs() < 1.);
}

#[test]
fn monitor_unplug_rehomes_workspaces_and_floating_windows() {
    let mut e = Engine::new(tiling_config(), vec![monitor(1, 0.), monitor(2, 1200.)])
        .expect("valid fixture");
    add(&mut e, 1);
    command(&mut e, "move-node-to-monitor next --focus-follows-window");
    assert_eq!(e.active_monitor, 2);
    command(&mut e, "layout floating");
    e.set_monitors(vec![monitor(1, 0.)]).expect("valid fixture");
    e.reflow();
    e.verify().expect("valid fixture");
    assert_eq!(e.active_monitor, 1);
    assert!(e.windows[&1].frame.right() <= 1200.);
}

#[test]
fn regex_rules_route_windows_once_and_respect_check_further() {
    let config = Config {
        rules: vec![
            Rule {
                app_id: Some("^test\\.app$".into()),
                workspace: Some("code".into()),
                check_further: true,
                ..Rule::default()
            },
            Rule {
                title: Some("Window 1".into()),
                floating: Some(true),
                ..Rule::default()
            },
        ],
        ..tiling_config()
    };
    let mut e = Engine::new(config, vec![monitor(1, 0.)]).expect("valid fixture");
    add(&mut e, 1);
    assert_eq!(e.windows[&1].workspace, "code");
    assert!(e.windows[&1].floating);
    command(&mut e, "move-node-to-workspace 3");
    let w = e.windows[&1].clone();
    e.update_window(w);
    assert_eq!(e.windows[&1].workspace, "3");
}

#[test]
fn paused_engine_keeps_model_but_produces_no_layout_work() {
    let mut e = engine();
    add(&mut e, 1);
    command(&mut e, "enable off");
    assert!(e.reflow().is_empty());
    assert!(e.hidden().is_empty());
    assert!(
        e.execute(parse("workspace 2").expect("valid fixture"))
            .is_err()
    );
    command(&mut e, "enable on");
    assert!(!e.reflow().is_empty());
}

#[test]
fn command_and_config_validation_reject_bad_input() {
    for s in [
        "",
        "focus diagonal",
        "resize width NaN",
        "resize width inf",
        "resize width 1e300",
        "workspace",
        "workspace 'unclosed",
        "fullscreen maybe",
        "layout unknown",
    ] {
        assert!(parse(s).is_err(), "{s}");
    }
    let mut config = tiling_config();
    config.rules.push(Rule {
        title: Some("[".into()),
        ..Rule::default()
    });
    assert!(config.validate().is_err());
    let mut config = tiling_config();
    config.gaps.inner = f64::NAN;
    assert!(config.validate().is_err());
    assert!(protocol_command(&["exec-and-forget".into(), "echo hi".into()]).is_err());
}

#[test]
fn notification_reflow_never_raises_or_focuses_windows() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    for _ in 0..10 {
        assert!(e.reflow().iter().all(|e| matches!(e, Effect::Frame(_, _))));
    }
}

#[test]
fn random_lifecycle_and_command_sequences_preserve_invariants() {
    for seed in 0..8_u64 {
        let mut rng = seed + 1;
        let mut e = engine();
        let mut next_id = 1;
        for step in 0..2000 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let ids: Vec<_> = e.windows.keys().copied().collect();
            match rng % 17 {
                0..=2 if ids.len() < 100 => {
                    add(&mut e, next_id);
                    next_id += 1;
                }
                3 if !ids.is_empty() => {
                    e.remove_window(ids[(rng as usize / 16) % ids.len()]);
                }
                4 => {
                    let _ = e.execute(Command::Workspace((rng % 9 + 1).to_string()));
                }
                5 => {
                    let _ = e.execute(Command::MoveToWorkspace(
                        (rng % 9 + 1).to_string(),
                        rng & 16 != 0,
                    ));
                }
                6 => {
                    let _ = e.execute(Command::Layout(vec![LayoutRequest::ToggleFloating], false));
                }
                7 => {
                    let _ = e.execute(Command::Split(if rng & 16 == 0 {
                        Axis::Horizontal
                    } else {
                        Axis::Vertical
                    }));
                }
                8 => {
                    let _ = e.execute(Command::ResizeSmart((rng % 200) as f64 - 100.));
                }
                9 => {
                    let _ = e.execute(Command::Fullscreen(None));
                }
                10 => {
                    let _ = e.execute(Command::Flatten);
                }
                11 => {
                    let _ = e.execute(Command::Move(Direction::Left));
                }
                12 => {
                    let _ = e.execute(Command::Move(Direction::Down));
                }
                13 => {
                    let _ = e.execute(Command::Join(Direction::Right));
                }
                14 => {
                    let _ = e.execute(Command::Swap(Direction::Up));
                }
                15 if !ids.is_empty() => {
                    let id = ids[(rng as usize / 16) % ids.len()];
                    let before = e.windows[&id].frame;
                    let mut after = before;
                    after.x += 30.;
                    after.y += 30.;
                    let _ = e.commit_drag(
                        id,
                        before,
                        after,
                        (rng % 1200) as f64,
                        ((rng / 1200) % 850 + 24) as f64,
                    );
                }
                _ => {
                    let _ = e.execute(Command::Balance);
                }
            }
            e.reflow();
            e.verify().unwrap_or_else(|error| {
                panic!("seed {seed}, step {step}, action {}: {error}", rng % 17)
            });
            for w in e.windows.values() {
                assert!(w.frame.valid());
            }
        }
    }
}

#[test]
fn protocol_rejects_oversized_frames_without_allocating_the_payload() {
    let bytes = ((protocol::MAX_FRAME + 1) as u32).to_le_bytes();
    assert!(protocol::read_frame::<protocol::Request>(&mut &bytes[..]).is_err());
    let mut bytes = Vec::new();
    protocol::write_frame(
        &mut bytes,
        &protocol::Request {
            version: 1,
            args: vec!["status".into()],
        },
    )
    .expect("valid fixture");
    let request: protocol::Request =
        protocol::read_frame(&mut bytes.as_slice()).expect("valid fixture");
    assert_eq!(request.args, ["status"]);
}

#[test]
fn move_escapes_a_nested_container_while_swap_preserves_its_shape() {
    let mut e = engine();
    add(&mut e, 1);
    command(&mut e, "split vertical");
    add(&mut e, 2);
    command(&mut e, "move right");
    assert_eq!(e.windows[&1].frame.y, e.windows[&2].frame.y);
    assert!(e.windows[&2].frame.x > e.windows[&1].frame.x);
}

#[test]
fn joining_side_by_side_windows_creates_the_opposite_orientation() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    command(&mut e, "join-with left");
    assert_eq!(e.windows[&1].frame.x, e.windows[&2].frame.x);
    assert!(e.windows[&1].frame.y < e.windows[&2].frame.y);
}

#[test]
fn layout_command_chooses_first_unmatched_description() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    command(&mut e, "layout accordion");
    command(&mut e, "layout tiles horizontal vertical");
    assert_eq!(e.windows[&1].frame.y, e.windows[&2].frame.y);
    command(&mut e, "layout tiles horizontal vertical");
    assert_eq!(e.windows[&1].frame.x, e.windows[&2].frame.x);
}

#[test]
fn empty_workspace_layout_and_absolute_resize_work() {
    let mut e = engine();
    command(&mut e, "layout --root vertical");
    add(&mut e, 1);
    add(&mut e, 2);
    assert_eq!(e.windows[&1].frame.x, e.windows[&2].frame.x);
    command(&mut e, "layout floating");
    command(&mut e, "resize width 400");
    assert_eq!(e.windows[&2].frame.width, 400.);
}

#[test]
fn targeted_commands_do_not_steal_focus() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    command(&mut e, "move-node-to-workspace 2 --window-id 1");
    assert_eq!(e.focused, Some(2));
    assert_eq!(e.windows[&1].workspace, "2");
    assert!(
        protocol_command(&[
            "exec-and-forget".into(),
            "--window-id".into(),
            "1".into(),
            "true".into()
        ])
        .is_err()
    );
}

#[test]
fn window_parking_uses_a_free_corner_of_its_own_monitor() {
    let mut right_below = monitor(2, 1200.);
    right_below.frame.y = 800.;
    right_below.usable.y = 824.;
    let mut e = Engine::new(tiling_config(), vec![monitor(1, 0.), right_below]).expect("engine");
    add(&mut e, 1);
    command(&mut e, "move-node-to-workspace 2");
    let parked = e.parking_frame(1).expect("frame");
    assert!(parked.x < 0.);
    assert_eq!(parked.y, 899.);
}

#[test]
fn allowlists_and_exclusions_are_compiled_and_applied() {
    let config = Config {
        include_apps: vec!["^com\\.example\\.".into()],
        exclude_apps: vec!["popup$".into()],
        ..tiling_config()
    };
    let e = Engine::new(config, vec![monitor(1, 0.)]).expect("engine");
    assert!(e.manages_app("com.example.editor"));
    assert!(!e.manages_app("com.example.popup"));
    assert!(!e.manages_app("another.editor"));
}

#[test]
fn floating_windows_translate_when_moved_to_another_monitor() {
    let mut e =
        Engine::new(tiling_config(), vec![monitor(1, 0.), monitor(2, 1200.)]).expect("engine");
    add(&mut e, 1);
    command(&mut e, "layout floating");
    command(&mut e, "move-node-to-monitor next --focus-follows-window");
    assert!(e.windows[&1].frame.x >= 1200.);
}

#[test]
fn reloading_monitor_assignments_rehomes_workspaces() {
    let mut e =
        Engine::new(tiling_config(), vec![monitor(1, 0.), monitor(2, 1200.)]).expect("engine");
    let mut config = tiling_config();
    config
        .workspace_monitors
        .insert("2".into(), "secondary".into());
    e.reconfigure(config).expect("reload");
    e.verify().expect("consistent");
    assert_eq!(e.workspaces["2"].monitor, 2);
}

#[test]
fn focus_back_and_forth_crosses_workspaces() {
    let mut e = engine();
    add(&mut e, 1);
    command(&mut e, "workspace 2");
    add(&mut e, 2);
    command(&mut e, "focus --window-id 1");
    command(&mut e, "focus-back-and-forth");
    assert_eq!(e.focused, Some(2));
    assert_eq!(e.visible[&1], "2");
}

#[test]
fn hiding_an_application_releases_tiles_and_unhiding_rejoins_them() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    let mut window = e.windows[&2].clone();
    window.hidden = true;
    e.update_window(window);
    e.reflow();
    e.verify().expect("consistent");
    assert_eq!(e.windows[&1].frame.width, 1184.);
    let mut window = e.windows[&2].clone();
    window.hidden = false;
    e.update_window(window);
    e.reflow();
    e.verify().expect("consistent");
    assert!(e.windows[&1].frame.width < 1184.);
}

#[test]
fn layout_and_mode_changes_do_not_reactivate_the_focused_application() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    for text in [
        "resize width +10",
        "layout accordion",
        "mode main",
        "balance-sizes",
    ] {
        let effects = e.execute(parse(text).expect("command")).expect("execute");
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Focus(_))),
            "{text}"
        );
    }
}

#[test]
fn nested_absolute_resize_uses_the_container_span_and_gaps() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    command(&mut e, "split vertical");
    add(&mut e, 3);
    command(&mut e, "split horizontal");
    add(&mut e, 4);
    command(&mut e, "resize width 200");
    assert!((e.windows[&4].frame.width - 200.).abs() < 0.01);
    command(&mut e, "resize height +50");
    let height = e.windows[&4].frame.height;
    command(&mut e, "resize height -50");
    assert!((e.windows[&4].frame.height - (height - 50.)).abs() < 0.01);
}

#[test]
fn repeating_pending_split_changes_orientation_without_deepening_the_tree() {
    let mut e = engine();
    add(&mut e, 1);
    for _ in 0..2000 {
        command(&mut e, "split horizontal");
        command(&mut e, "split vertical");
    }
    add(&mut e, 2);
    assert_eq!(e.windows[&1].frame.x, e.windows[&2].frame.x);
    assert!(e.windows[&1].frame.y < e.windows[&2].frame.y);
}

#[test]
fn monitor_gap_overrides_inherit_global_values_and_specific_ids_take_precedence() {
    let mut config = tiling_config();
    config.gaps.inner = 12.;
    config.monitor_gaps.insert(
        "secondary".into(),
        GapsOverride {
            left: Some(40.),
            right: Some(20.),
            inner: Some(16.),
            ..GapsOverride::default()
        },
    );
    config.monitor_gaps.insert(
        "2".into(),
        GapsOverride {
            inner: Some(24.),
            ..GapsOverride::default()
        },
    );
    let mut e = Engine::new(config, vec![monitor(1, 0.), monitor(2, 1200.)]).expect("engine");
    add(&mut e, 1);
    assert_eq!(e.windows[&1].frame.x, 8.);
    command(&mut e, "move-node-to-monitor next --focus-follows-window");
    add(&mut e, 2);
    command(&mut e, "move-node-to-monitor 2 --focus-follows-window");
    assert_eq!(e.windows[&1].frame.x, 1240.);
    assert_eq!(e.windows[&1].frame.width, 558.);
    assert_eq!(e.windows[&2].frame.x - e.windows[&1].frame.right(), 24.);
    assert_eq!(e.windows[&1].frame.y, 32.);
    let mut invalid = e.config.clone();
    invalid.monitor_gaps.get_mut("2").expect("override").top = Some(f64::NAN);
    assert!(e.reconfigure(invalid).is_err());
    e.verify().expect("valid previous configuration");
}

#[test]
fn native_drag_reorders_tiles_and_clicks_do_not_rearrange_them() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    let initial = e.snapshot();
    let before = e.windows[&1].frame;
    let target = e.windows[&2].frame;
    assert!(
        e.commit_drag(1, before, before, target.right() - 2., target.center().1)
            .expect("click")
            .is_empty()
    );
    assert_eq!(e.snapshot(), initial);
    let mut moved = before;
    moved.x += 100.;
    e.commit_drag(1, before, moved, target.right() - 2., target.center().1)
        .expect("drag");
    e.verify().expect("valid tree");
    assert!(e.windows[&1].frame.x > e.windows[&2].frame.x);
    assert_eq!(e.focused, Some(1));
}

#[test]
fn native_drag_resize_updates_weights_and_cross_monitor_drop_moves_the_workspace() {
    let mut e =
        Engine::new(tiling_config(), vec![monitor(1, 0.), monitor(2, 1200.)]).expect("engine");
    add(&mut e, 1);
    add(&mut e, 2);
    let before = e.windows[&1].frame;
    let mut resized = before;
    resized.width += 100.;
    e.commit_drag(1, before, resized, 100., 100.)
        .expect("resize");
    assert!((e.windows[&1].frame.width - resized.width).abs() < 0.01);
    let before = e.windows[&1].frame;
    let mut moved = before;
    moved.x += 1200.;
    e.commit_drag(1, before, moved, 1500., 200.).expect("drop");
    e.verify().expect("valid tree");
    assert_eq!(e.active_monitor, 2);
    assert_eq!(e.windows[&1].workspace, e.visible[&2]);
    assert!(e.windows[&1].frame.x >= 1200.);
}

#[test]
fn cursor_commands_resolve_the_window_or_monitor_without_changing_focus() {
    let mut e = engine();
    add(&mut e, 1);
    let expected = e.windows[&1].frame;
    let effects = e
        .execute(parse("move-mouse window-lazy-center").expect("command"))
        .expect("execute");
    assert!(effects.contains(&Effect::Cursor(expected, true)));
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Focus(_)))
    );
    command(&mut e, "workspace 2");
    assert!(
        e.execute(parse("move-mouse window-center").expect("command"))
            .is_err()
    );
    let effects = e
        .execute(parse("move-mouse monitor-center").expect("command"))
        .expect("execute");
    assert!(effects.contains(&Effect::Cursor(e.monitors[&1].frame, false)));
}

#[test]
fn swapping_tiles_never_inserts_a_floating_window_into_the_tree() {
    let mut e = engine();
    add(&mut e, 1);
    add(&mut e, 2);
    command(&mut e, "layout floating");
    command(&mut e, "focus --window-id 1");
    command(&mut e, "swap right");
    assert!(e.windows[&2].floating);
    add(&mut e, 3);
    command(&mut e, "focus --window-id 1");
    command(&mut e, "swap right");
    assert!(e.windows[&1].frame.x > e.windows[&3].frame.x);
    assert!(e.windows[&2].floating);
}

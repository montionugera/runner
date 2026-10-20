use std::sync::Arc;
use std::time::Duration;

use gpui::{px, size, TestAppContext, VisualTestContext};

use crate::render_counts::RenderCounts;
use crate::{AppRoute, NativeRoot, PaneLayout, PaneNode, SplitOrientation};

#[test]
fn terminal_output_reuses_split_siblings_and_title_changes_update_sidebar() {
    let _theme = crate::theme_snapshot::ThemeGuard::new();
    let temp = tempfile::tempdir().unwrap();
    let mut cx = TestAppContext::single();
    let store = crate::app_store::test_lifecycle_store(&mut cx, temp.path());
    let (node_id, idle_pane, core, terminal) = store.update(&mut cx, |store, cx| {
        crate::app_store::seed_mixed_working_sessions(store);
        for status in store.session_statuses.values_mut() {
            status.observation.activity = runner_core::protocol::status::Activity::Idle;
        }
        let core = store.test_core.clone();
        let events: Arc<dyn runner_daemon::session::manager::SessionEvents> =
            Arc::new(core.session_events());
        for id in ["busy-shell", "direct-agent"] {
            core.sessions
                .prepare_unlisted_terminal(id, (80, 24), &core.db, &events)
                .unwrap();
            store.bridge.attach(id).unwrap();
        }
        let mut layout = PaneLayout::single(Some("busy-shell"), &[]);
        let idle_pane = layout.split("p1", SplitOrientation::Row).unwrap();
        layout.assign_session(&idle_pane, "direct-agent").unwrap();
        let conn = core.db.get().unwrap();
        let node =
            runner_daemon::repo::node::create_tab(&conn, None, "", 0, &layout.serialize().unwrap())
                .unwrap();
        store.replace_nodes(runner_daemon::repo::node::list(&conn).unwrap(), cx);
        let terminal = store.bridge.session("busy-shell").unwrap();
        (node.id, idle_pane, core, terminal)
    });
    let host = cx.add_window(|window, cx| {
        let mut root = NativeRoot::new(
            "redraw-test".into(),
            temp.path().join("logs"),
            None,
            None,
            store.clone(),
            window,
            cx,
        );
        root.apply_tab_rows(cx);
        assert!(root.tabs.activate_session("direct-agent"));
        root.route = AppRoute::Chat;
        root.sync_active_chat_detail(cx);
        root.ensure_active_tab_attached(window, cx).unwrap();
        root
    });
    let mut visual = VisualTestContext::from_window(host.into(), &cx);
    visual.simulate_resize(size(px(1440.), px(900.)));
    terminal.test_feed(1, b"ready");
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(50));
    cx.run_until_parked();
    assert!(visual.debug_bounds("CHAT_SIDE_PANEL").is_some());
    assert!(visual.debug_bounds("APP_SIDEBAR").is_some());
    assert_eq!(
        host.read_with(&cx, |root, _| root.route.clone()).unwrap(),
        AppRoute::Chat
    );

    let counts = RenderCounts::default();
    cx.update(|cx| cx.set_global(counts.clone()));
    // Seed the counters with one real frame so a missing surface cannot pass as zero renders.
    host.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
    cx.run_until_parked();
    let initial = counts.0.borrow().clone();
    for key in [
        "sidebar",
        "tab-bar",
        "side-panel",
        "pane:p1",
        &format!("pane:{idle_pane}"),
        "terminal:busy-shell",
        "terminal:direct-agent",
    ] {
        assert!(
            initial.get(key).copied().unwrap_or_default() > 0,
            "missing render counter: {key}: {initial:?}"
        );
    }
    counts.0.borrow_mut().clear();
    for seq in 2..=25 {
        terminal.test_feed(seq, b"\r|");
    }
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(4));
    cx.run_until_parked();
    let rendered = counts.0.borrow().clone();
    assert_eq!(
        rendered.get("terminal:busy-shell"),
        Some(&1),
        "one output burst should render once: {rendered:?}"
    );
    for key in [
        "sidebar",
        "tab-bar",
        "side-panel",
        &format!("pane:{idle_pane}"),
        "terminal:direct-agent",
    ] {
        assert_eq!(
            rendered.get(key).copied().unwrap_or_default(),
            0,
            "unchanged view re-rendered: {key}: {rendered:?}"
        );
    }

    assert!(terminal.viewer_count() > 0, "busy terminal has no viewer");
    let wake_before = store.read_with(&cx, |store, _| store.revisions.terminal_wake);
    counts.0.borrow_mut().clear();
    let model = core.sessions.terminal_model("busy-shell").unwrap();
    model.feed_output(26, b"\x1b]0;Updated shell title\x07");
    model.flush_events();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while model.title() != "Updated shell title" && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(model.title(), "Updated shell title");
    terminal.refresh_metadata();
    assert_eq!(terminal.title(), "Updated shell title");
    terminal.test_feed(26, b"\x1b]0;Updated shell title\x07");
    cx.run_until_parked();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        cx.executor().advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
        if counts
            .0
            .borrow()
            .get("sidebar")
            .copied()
            .unwrap_or_default()
            > 0
            && store.read_with(&cx, |store, _| store.revisions.terminal_wake) != wake_before
        {
            break;
        }
        std::thread::yield_now();
    }
    let wake_after = store.read_with(&cx, |store, _| store.revisions.terminal_wake);
    assert_ne!(
        wake_after, wake_before,
        "live title must update the store revision"
    );
    let rendered = counts.0.borrow();
    assert!(
        rendered.get("sidebar").copied().unwrap_or_default() > 0,
        "title wake must invalidate the sidebar: {rendered:?}, wake {wake_before}->{wake_after}, viewers {}, title {:?}",
        terminal.viewer_count(), terminal.title()
    );
    assert!(
        rendered
            .get(&format!("sidebar-row:{node_id}"))
            .copied()
            .unwrap_or_default()
            > 0,
        "live title must update its row: {rendered:?}"
    );
    drop(rendered);

    let agent_model = core.sessions.terminal_model("direct-agent").unwrap();
    let agent = store.read_with(&cx, |store, _| {
        store.bridge.session("direct-agent").unwrap()
    });
    agent_model.feed_output(1, b"\x1b]0;Stable topic\x07");
    agent_model.flush_events();
    assert_eq!(agent_model.title(), "Stable topic");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(10));
        if store.read_with(&cx, |store, _| {
            store
                .sessions
                .iter()
                .find(|session| session.session_id == "direct-agent")
                .and_then(|session| session.live_title.as_deref())
                == Some("Stable topic")
        }) {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(
        store.read_with(&cx, |store, _| store
            .sessions
            .iter()
            .find(|session| session.session_id == "direct-agent")
            .unwrap()
            .live_title
            .clone()),
        Some("Stable topic".into())
    );
    agent.refresh_metadata();
    agent.test_feed(1, b"\x1b]0;Stable topic\x07");
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(50));
    cx.run_until_parked();
    let revisions_before = store.read_with(&cx, |store, _| store.revisions);
    counts.0.borrow_mut().clear();
    for (seq, glyph) in (2..).zip(['/', '-', '\\', '|', '/', '-', '\\']) {
        let output = format!("\x1b]0;{glyph} Stable topic\x07");
        agent_model.feed_output(seq, output.as_bytes());
        agent.test_feed(seq, output.as_bytes());
    }
    agent_model.flush_events();
    agent.refresh_metadata();
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(4));
    cx.run_until_parked();
    let rendered = counts.0.borrow().clone();
    assert_eq!(
        rendered.get("terminal:direct-agent"),
        Some(&1),
        "title spinner burst must paint once: {rendered:?}"
    );
    for key in [
        "sidebar",
        "tab-bar",
        "side-panel",
        "pane:p1",
        "terminal:busy-shell",
    ] {
        assert_eq!(
            rendered.get(key).copied().unwrap_or_default(),
            0,
            "title spinner invalidated an unchanged view: {key}: {rendered:?}"
        );
    }

    assert_eq!(agent.title(), "Stable topic");
    assert_eq!(
        store.read_with(&cx, |store, _| store.revisions),
        revisions_before,
        "spinner-only titles must not refresh shared metadata"
    );

    host.update(&mut cx, |root, window, cx| {
        let layout = root.tabs.active_mut().unwrap();
        layout.add_drawer_shell("busy-shell".into());
        layout.set_drawer_open(true);
        root.ensure_active_tab_attached(window, cx).unwrap();
        cx.notify();
    })
    .unwrap();
    let shared = store.read_with(&cx, |store, _| store.bridge.session("busy-shell").unwrap());
    shared.test_feed(100, b"ready");
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(50));
    cx.run_until_parked();
    counts.0.borrow_mut().clear();
    shared.test_feed(101, b"\r|");
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(4));
    cx.run_until_parked();
    let rendered = counts.0.borrow();
    assert_eq!(
        rendered.get("terminal:busy-shell"),
        Some(&2),
        "both appearances must paint once: {rendered:?}"
    );
    assert_eq!(rendered.get("pane:p1"), Some(&1));
    assert_eq!(
        rendered.get(&format!("pane:{idle_pane}")),
        None,
        "idle pane must stay cached beside a busy drawer: {rendered:?}"
    );
}

#[test]
fn sidebar_status_change_only_rerenders_affected_sidebar_views() {
    let _theme = crate::theme_snapshot::ThemeGuard::new();
    let temp = tempfile::tempdir().unwrap();
    let mut cx = TestAppContext::single();
    let store = crate::app_store::test_lifecycle_store(&mut cx, temp.path());
    let (node_id, idle_pane, _core, _terminal) = store.update(&mut cx, |store, cx| {
        crate::app_store::seed_mixed_working_sessions(store);
        for status in store.session_statuses.values_mut() {
            status.observation.activity = runner_core::protocol::status::Activity::Idle;
        }
        let core = store.test_core.clone();
        let events: Arc<dyn runner_daemon::session::manager::SessionEvents> =
            Arc::new(core.session_events());
        for id in ["busy-shell", "direct-agent"] {
            core.sessions
                .prepare_unlisted_terminal(id, (80, 24), &core.db, &events)
                .unwrap();
            store.bridge.attach(id).unwrap();
        }
        let mut layout = PaneLayout::single(Some("busy-shell"), &[]);
        let idle_pane = layout.split("p1", SplitOrientation::Row).unwrap();
        layout.assign_session(&idle_pane, "direct-agent").unwrap();
        let conn = core.db.get().unwrap();
        let node =
            runner_daemon::repo::node::create_tab(&conn, None, "", 0, &layout.serialize().unwrap())
                .unwrap();
        store.replace_nodes(runner_daemon::repo::node::list(&conn).unwrap(), cx);
        let terminal = store.bridge.session("busy-shell").unwrap();
        (node.id, idle_pane, core, terminal)
    });
    let host = cx.add_window(|window, cx| {
        let mut root = NativeRoot::new(
            "status-change-test".into(),
            temp.path().join("logs"),
            None,
            None,
            store.clone(),
            window,
            cx,
        );
        root.apply_tab_rows(cx);
        assert!(root.tabs.activate_session("direct-agent"));
        root.route = AppRoute::Chat;
        root.sync_active_chat_detail(cx);
        root.ensure_active_tab_attached(window, cx).unwrap();
        root
    });
    let _visual = VisualTestContext::from_window(host.into(), &cx);
    _visual.simulate_resize(size(px(1440.), px(900.)));
    cx.run_until_parked();

    let counts = RenderCounts::default();
    cx.update(|cx| cx.set_global(counts.clone()));
    host.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
    cx.run_until_parked();

    // Clear render counts before status update.
    counts.0.borrow_mut().clear();

    // Update status in the store: direct-agent becomes Working.
    store.update(&mut cx, |store, cx| {
        if let Some(status) = store.session_statuses.get_mut("direct-agent") {
            status.observation.activity = runner_core::protocol::status::Activity::Working;
        }
        store.revisions.activity = store.revisions.activity.wrapping_add(1);
        cx.notify();
    });
    cx.run_until_parked();

    let rendered = counts.0.borrow().clone();
    assert!(
        rendered.get("sidebar").copied().unwrap_or_default() > 0,
        "sidebar must re-render when a status changes: {rendered:?}"
    );
    assert!(
        rendered
            .get(&format!("sidebar-row:{node_id}"))
            .copied()
            .unwrap_or_default()
            > 0,
        "affected sidebar row must re-render: {rendered:?}"
    );

    // Unchanged views must not re-render.
    for key in [
        "tab-bar",
        "side-panel",
        "pane:p1",
        &format!("pane:{idle_pane}"),
        "terminal:busy-shell",
        "terminal:direct-agent",
    ] {
        assert_eq!(
            rendered.get(key).copied().unwrap_or_default(),
            0,
            "unchanged view re-rendered on sidebar status change: {key}: {rendered:?}"
        );
    }
}

#[test]
fn tab_switch_only_rerenders_new_tab_views() {
    let _theme = crate::theme_snapshot::ThemeGuard::new();
    let temp = tempfile::tempdir().unwrap();
    let mut cx = TestAppContext::single();
    let store = crate::app_store::test_lifecycle_store(&mut cx, temp.path());
    let (node1_id, node2_id, _core) = store.update(&mut cx, |store, cx| {
        crate::app_store::seed_mixed_working_sessions(store);
        let core = store.test_core.clone();
        let events: Arc<dyn runner_daemon::session::manager::SessionEvents> =
            Arc::new(core.session_events());
        for id in ["busy-shell", "direct-agent"] {
            core.sessions
                .prepare_unlisted_terminal(id, (80, 24), &core.db, &events)
                .unwrap();
            store.bridge.attach(id).unwrap();
        }
        let layout1 = PaneLayout::single(Some("busy-shell"), &[]);
        let layout2 = PaneLayout::single(Some("direct-agent"), &[]);
        let conn = core.db.get().unwrap();
        let node1 = runner_daemon::repo::node::create_tab(
            &conn,
            None,
            "",
            0,
            &layout1.serialize().unwrap(),
        )
        .unwrap();
        let node2 = runner_daemon::repo::node::create_tab(
            &conn,
            None,
            "",
            1,
            &layout2.serialize().unwrap(),
        )
        .unwrap();
        store.replace_nodes(runner_daemon::repo::node::list(&conn).unwrap(), cx);
        (node1.id, node2.id, core)
    });
    let host = cx.add_window(|window, cx| {
        let mut root = NativeRoot::new(
            "tab-switch-test".into(),
            temp.path().join("logs"),
            None,
            None,
            store.clone(),
            window,
            cx,
        );
        root.apply_tab_rows(cx);
        assert!(root.tabs.activate(&node1_id));
        root.route = AppRoute::Chat;
        root.sync_active_chat_detail(cx);
        root.ensure_active_tab_attached(window, cx).unwrap();
        root
    });
    let _visual = VisualTestContext::from_window(host.into(), &cx);
    _visual.simulate_resize(size(px(1440.), px(900.)));
    cx.run_until_parked();

    let counts = RenderCounts::default();
    cx.update(|cx| cx.set_global(counts.clone()));
    host.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
    cx.run_until_parked();

    let initial = counts.0.borrow().clone();
    assert!(
        initial
            .get("terminal:busy-shell")
            .copied()
            .unwrap_or_default()
            > 0
    );
    assert_eq!(
        initial
            .get("terminal:direct-agent")
            .copied()
            .unwrap_or_default(),
        0,
        "direct-agent terminal must not render before switch"
    );

    // Clear render counts before switching tabs
    counts.0.borrow_mut().clear();

    // Switch to Tab 2
    host.update(&mut cx, |root, window, cx| {
        root.activate_tab(&node2_id, window, cx);
    })
    .unwrap();
    cx.run_until_parked();

    let rendered = counts.0.borrow().clone();
    assert!(
        rendered.get("tab-bar").copied().unwrap_or_default() > 0,
        "tab-bar must re-render on tab switch: {rendered:?}"
    );
    assert!(
        rendered.get("sidebar").copied().unwrap_or_default() > 0,
        "sidebar must re-render on tab switch: {rendered:?}"
    );
    assert!(
        rendered
            .get(&format!("sidebar-row:{node2_id}"))
            .copied()
            .unwrap_or_default()
            > 0,
        "newly active sidebar row must re-render: {rendered:?}"
    );
    assert!(
        rendered
            .get("terminal:direct-agent")
            .copied()
            .unwrap_or_default()
            > 0,
        "newly active tab terminal must render: {rendered:?}"
    );

    // Hidden views from Tab 1 must NOT re-render:
    assert_eq!(
        rendered
            .get("terminal:busy-shell")
            .copied()
            .unwrap_or_default(),
        0,
        "hidden tab terminal must not re-render: {rendered:?}"
    );
}

#[test]
fn frames_drawn_for_terminal_output_burst_at_most_one_per_frame() {
    let _theme = crate::theme_snapshot::ThemeGuard::new();
    let temp = tempfile::tempdir().unwrap();
    let mut cx = TestAppContext::single();
    let store = crate::app_store::test_lifecycle_store(&mut cx, temp.path());
    let (_node_id, terminal) = store.update(&mut cx, |store, cx| {
        crate::app_store::seed_mixed_working_sessions(store);
        let core = store.test_core.clone();
        let events: Arc<dyn runner_daemon::session::manager::SessionEvents> =
            Arc::new(core.session_events());
        core.sessions
            .prepare_unlisted_terminal("busy-shell", (80, 24), &core.db, &events)
            .unwrap();
        store.bridge.attach("busy-shell").unwrap();
        let layout = PaneLayout::single(Some("busy-shell"), &[]);
        let conn = core.db.get().unwrap();
        let node =
            runner_daemon::repo::node::create_tab(&conn, None, "", 0, &layout.serialize().unwrap())
                .unwrap();
        store.replace_nodes(runner_daemon::repo::node::list(&conn).unwrap(), cx);
        let terminal = store.bridge.session("busy-shell").unwrap();
        (node.id, terminal)
    });
    let host = cx.add_window(|window, cx| {
        let mut root = NativeRoot::new(
            "burst-test".into(),
            temp.path().join("logs"),
            None,
            None,
            store.clone(),
            window,
            cx,
        );
        root.apply_tab_rows(cx);
        assert!(root.tabs.activate_session("busy-shell"));
        root.route = AppRoute::Chat;
        root.sync_active_chat_detail(cx);
        root.ensure_active_tab_attached(window, cx).unwrap();
        root
    });
    let _visual = VisualTestContext::from_window(host.into(), &cx);
    _visual.simulate_resize(size(px(1440.), px(900.)));
    terminal.test_feed(1, b"init");
    cx.run_until_parked();

    let counts = RenderCounts::default();
    cx.update(|cx| cx.set_global(counts.clone()));
    host.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
    cx.run_until_parked();

    // Clear render counts before burst.
    counts.0.borrow_mut().clear();

    // Feed a rapid burst of 50 chunks within a single frame interval.
    for seq in 2..=51 {
        terminal.test_feed(seq, b"\r[chunk]");
    }
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(4));
    cx.run_until_parked();

    let rendered = counts.0.borrow().clone();
    assert_eq!(
        rendered.get("terminal:busy-shell"),
        Some(&1),
        "burst of 50 outputs must draw at most one frame: {rendered:?}"
    );

    // Advancing clock without new output must NOT draw extra frames.
    counts.0.borrow_mut().clear();
    cx.executor().advance_clock(Duration::from_millis(16));
    cx.run_until_parked();
    assert_eq!(
        counts
            .0
            .borrow()
            .get("terminal:busy-shell")
            .copied()
            .unwrap_or_default(),
        0,
        "idle tick must draw zero frames"
    );
}

#[test]
fn layout_work_for_fixed_window_stays_within_budget() {
    let _theme = crate::theme_snapshot::ThemeGuard::new();
    let temp = tempfile::tempdir().unwrap();
    let mut cx = TestAppContext::single();
    let store = crate::app_store::test_lifecycle_store(&mut cx, temp.path());
    let (node1_id, node2_id, idle_pane, split_id) = store.update(&mut cx, |store, cx| {
        crate::app_store::seed_mixed_working_sessions(store);
        let core = store.test_core.clone();
        let events: Arc<dyn runner_daemon::session::manager::SessionEvents> =
            Arc::new(core.session_events());
        for id in ["busy-shell", "direct-agent"] {
            core.sessions
                .prepare_unlisted_terminal(id, (80, 24), &core.db, &events)
                .unwrap();
            store.bridge.attach(id).unwrap();
        }
        let mut layout1 = PaneLayout::single(Some("busy-shell"), &[]);
        let idle_pane = layout1.split("p1", SplitOrientation::Row).unwrap();
        layout1.assign_session(&idle_pane, "direct-agent").unwrap();
        let split_id = match &layout1.root {
            PaneNode::Split(split) => split.id.clone(),
            _ => panic!("expected split root"),
        };
        let layout2 = PaneLayout::single(Some("direct-agent"), &[]);
        let conn = core.db.get().unwrap();
        let node1 = runner_daemon::repo::node::create_tab(
            &conn,
            None,
            "",
            0,
            &layout1.serialize().unwrap(),
        )
        .unwrap();
        let node2 = runner_daemon::repo::node::create_tab(
            &conn,
            None,
            "",
            1,
            &layout2.serialize().unwrap(),
        )
        .unwrap();
        store.replace_nodes(runner_daemon::repo::node::list(&conn).unwrap(), cx);
        (node1.id, node2.id, idle_pane, split_id)
    });
    let host = cx.add_window(|window, cx| {
        let mut root = NativeRoot::new(
            "budget-test".into(),
            temp.path().join("logs"),
            None,
            None,
            store.clone(),
            window,
            cx,
        );
        root.apply_tab_rows(cx);
        assert!(root.tabs.activate(&node1_id));
        root.route = AppRoute::Chat;
        root.sync_active_chat_detail(cx);
        root.ensure_active_tab_attached(window, cx).unwrap();
        root
    });
    let _visual = VisualTestContext::from_window(host.into(), &cx);
    _visual.simulate_resize(size(px(1440.), px(900.)));
    cx.run_until_parked();

    let counts = RenderCounts::default();
    cx.update(|cx| cx.set_global(counts.clone()));

    // Trigger a single layout and paint pass on the fixed window.
    counts.0.borrow_mut().clear();
    host.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
    cx.run_until_parked();

    let rendered = counts.0.borrow().clone();

    // 1. Sidebar rows budget: each row laid out at most once per pass.
    let row1_count = rendered
        .get(&format!("sidebar-row:{node1_id}"))
        .copied()
        .unwrap_or_default();
    let row2_count = rendered
        .get(&format!("sidebar-row:{node2_id}"))
        .copied()
        .unwrap_or_default();
    assert_eq!(
        row1_count, 1,
        "sidebar row 1 must lay out exactly once: {rendered:?}"
    );
    assert_eq!(
        row2_count, 1,
        "sidebar row 2 must lay out exactly once: {rendered:?}"
    );

    // 2. Panes budget: each visible pane laid out at most once per pass.
    let p1_count = rendered.get("pane:p1").copied().unwrap_or_default();
    let idle_count = rendered
        .get(&format!("pane:{idle_pane}"))
        .copied()
        .unwrap_or_default();
    assert_eq!(
        p1_count, 1,
        "pane p1 must lay out exactly once: {rendered:?}"
    );
    assert_eq!(
        idle_count, 1,
        "idle pane must lay out exactly once: {rendered:?}"
    );

    // 3. Splits budget: each visible split laid out at most once per pass.
    let split_count = rendered
        .get(&format!("split:{split_id}"))
        .copied()
        .unwrap_or_default();
    assert_eq!(
        split_count, 1,
        "split must lay out exactly once: {rendered:?}"
    );

    // 4. Total layout work for (sidebar rows, panes, splits) must not exceed budget of 5.
    let total_layout_work = row1_count + row2_count + p1_count + idle_count + split_count;
    const BUDGET: usize = 5;
    assert!(
        total_layout_work <= BUDGET,
        "fixed window layout work {total_layout_work} exceeded budget {BUDGET}: {rendered:?}"
    );
}

//! Portable desktop types, `PlatformAdapter` contract tests, and cross-platform CLI checks.

use std::process::Command;

use agent_workstation::platform::RawNode;
use agent_workstation::{
    AdapterError, Button, DesktopAction, DesktopRect, DesktopTree, Frame, MockPlatformAdapter,
    NodeRef, PlatformAdapter, PlatformCapabilities, TreeOptions, native_adapter,
};
#[cfg(not(target_os = "linux"))]
use agent_workstation::{BrowserConfig, DesktopSession, Error, Profile, SessionConfig};

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn unsupported_start_has_no_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("profile");
    let runtime = dir.path().join("runtime");
    let config = SessionConfig::new(
        Profile::new("portable", &profile),
        BrowserConfig::new("missing-browser"),
        &runtime,
    );
    let result = DesktopSession::start(config).await;
    assert!(matches!(result, Err(Error::Unsupported(_))), "{result:?}");
    assert!(!profile.exists());
    assert!(!runtime.exists());
}

#[test]
fn portable_desktop_types() {
    let frame = Frame {
        width: 1,
        height: 1,
        rgba: vec![255, 0, 0, 255],
    };
    let bytes = frame.to_png().unwrap();
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().unwrap();
    let mut rgba = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut rgba).unwrap();
    assert_eq!((info.width, info.height), (1, 1));
    assert_eq!(rgba, frame.rgba);
    assert_eq!(serde_json::to_string(&Button::Left).unwrap(), "\"left\"");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn unsupported_lock_inspection_preserves_profile() {
    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join("SingletonLock");
    std::fs::write(&lock, b"preserve").unwrap();
    let result = agent_workstation::profile::clear_stale_singleton(dir.path());
    assert!(matches!(result, Err(Error::Unsupported(_))));
    assert_eq!(std::fs::read(lock).unwrap(), b"preserve");
}

#[test]
fn native_adapter_reports_expected_compile_time_platform() {
    let adapter = native_adapter();
    let caps = adapter.capabilities();
    #[cfg(target_os = "linux")]
    {
        assert_eq!(caps.platform, "linux");
        assert_eq!(caps.backend, "linux-atspi2+wayland");
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(caps.platform, "macos");
        assert_eq!(caps.backend, "macos-ax");
    }
    #[cfg(target_os = "windows")]
    {
        assert_eq!(caps.platform, "windows");
        assert_eq!(caps.backend, "windows-uia");
    }
}

#[test]
fn cli_desktop_host_capabilities_outputs_valid_json_on_all_platforms() {
    let out = Command::new(env!("CARGO_BIN_EXE_agent-workstation"))
        .args(["desktop", "--host", "capabilities", "--json"])
        .output()
        .expect("spawn agent-workstation");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let caps: PlatformCapabilities =
        serde_json::from_slice(&out.stdout).expect("valid PlatformCapabilities JSON");
    assert_eq!(caps, native_adapter().capabilities());
}

fn fixture_tree() -> Vec<RawNode<String>> {
    vec![RawNode {
        handle: "app-window".into(),
        role: "window".into(),
        name: "Document Editor".into(),
        value: None,
        app_id: Some("editor".into()),
        pid: Some(4200),
        enabled: true,
        focused: true,
        bounds: Some(DesktopRect {
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
        }),
        actions: vec!["focus".into()],
        children: vec![RawNode {
            handle: "toolbar".into(),
            role: "toolbar".into(),
            name: "Main".into(),
            value: None,
            app_id: Some("editor".into()),
            pid: Some(4200),
            enabled: true,
            focused: false,
            bounds: Some(DesktopRect {
                x: 0,
                y: 0,
                width: 1280,
                height: 48,
            }),
            actions: vec![],
            children: vec![
                RawNode {
                    handle: "field-title".into(),
                    role: "text_field".into(),
                    name: "Title".into(),
                    value: Some("Untitled".into()),
                    app_id: Some("editor".into()),
                    pid: Some(4200),
                    enabled: true,
                    focused: false,
                    bounds: Some(DesktopRect {
                        x: 16,
                        y: 8,
                        width: 240,
                        height: 32,
                    }),
                    actions: vec!["set_value".into(), "focus".into()],
                    children: vec![],
                },
                RawNode {
                    handle: "btn-publish".into(),
                    role: "button".into(),
                    name: "Publish".into(),
                    value: None,
                    app_id: Some("editor".into()),
                    pid: Some(4200),
                    enabled: true,
                    focused: false,
                    bounds: Some(DesktopRect {
                        x: 272,
                        y: 8,
                        width: 96,
                        height: 32,
                    }),
                    actions: vec!["press".into()],
                    children: vec![],
                },
            ],
        }],
    }]
}

#[tokio::test]
async fn platform_adapter_contract_snapshot_json_refs_rotation_bounds_and_permissions() {
    let adapter = MockPlatformAdapter::with_roots(fixture_tree());
    let dyn_adapter: &dyn PlatformAdapter = &adapter;

    // 1. Snapshot produces JSON-serializable DesktopTree with deterministic @p1:eN refs.
    let tree = dyn_adapter.snapshot(TreeOptions::default()).await.unwrap();
    let json = serde_json::to_string(&tree).unwrap();
    let roundtrip: DesktopTree = serde_json::from_str(&json).unwrap();
    assert_eq!(roundtrip, tree);
    assert_eq!(tree.snapshot_id, "p1");
    assert_eq!(tree.total_nodes, 4);
    assert!(!tree.truncated);
    assert_eq!(tree.find_node("@p1:e3").unwrap().name, "Title");
    assert_eq!(tree.find_node("@p1:e4").unwrap().name, "Publish");

    // 2. Action dispatch covers all DesktopAction variants.
    dyn_adapter
        .act(DesktopAction::SetValue {
            target: NodeRef::parse("@p1:e3").unwrap(),
            value: "Quarterly Plan".into(),
        })
        .await
        .unwrap();
    dyn_adapter
        .act(DesktopAction::Click {
            target: NodeRef::parse("@p1:e4").unwrap(),
            button: Some(Button::Left),
        })
        .await
        .unwrap();
    dyn_adapter
        .act(DesktopAction::DoubleClick {
            target: NodeRef::parse("@p1:e4").unwrap(),
        })
        .await
        .unwrap();
    dyn_adapter
        .act(DesktopAction::Focus {
            target: NodeRef::parse("@p1:e1").unwrap(),
        })
        .await
        .unwrap();
    dyn_adapter
        .act(DesktopAction::TypeText {
            text: "Hello".into(),
        })
        .await
        .unwrap();
    dyn_adapter
        .act(DesktopAction::PressKey {
            chord: "ctrl+s".into(),
        })
        .await
        .unwrap();
    dyn_adapter
        .act(DesktopAction::Scroll {
            target: Some(NodeRef::parse("@p1:e1").unwrap()),
            dx: 0,
            dy: 4,
        })
        .await
        .unwrap();

    // 3. Tree rotation invalidates @p1 refs fail-closed with StaleRef.
    let rotated = dyn_adapter.snapshot(TreeOptions::default()).await.unwrap();
    assert_eq!(rotated.snapshot_id, "p2");
    assert_eq!(
        rotated.find_node("@p2:e3").unwrap().value.as_deref(),
        Some("Quarterly Plan")
    );
    let stale = dyn_adapter
        .act(DesktopAction::Click {
            target: NodeRef::parse("@p1:e4").unwrap(),
            button: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(stale, AdapterError::StaleRef(_)));

    // 4. Depth and node caps truncate cleanly without panicking.
    let depth_capped = dyn_adapter
        .snapshot(TreeOptions {
            max_depth: 2,
            max_nodes: 100,
            pid: None,
        })
        .await
        .unwrap();
    assert!(depth_capped.truncated);
    assert_eq!(depth_capped.total_nodes, 2);

    let node_capped = dyn_adapter
        .snapshot(TreeOptions {
            max_depth: 10,
            max_nodes: 3,
            pid: None,
        })
        .await
        .unwrap();
    assert!(node_capped.truncated);
    assert_eq!(node_capped.total_nodes, 3);

    // 5. Missing node fails closed with NodeNotFound.
    let missing = dyn_adapter
        .act(DesktopAction::Focus {
            target: NodeRef::parse("@e99").unwrap(),
        })
        .await
        .unwrap_err();
    assert!(matches!(missing, AdapterError::NodeNotFound(_)));

    // 6. Permission denied fails closed on all operations.
    adapter.set_permission_denied(Some("host accessibility permission denied".into()));
    assert!(matches!(
        dyn_adapter.snapshot(TreeOptions::default()).await,
        Err(AdapterError::PermissionDenied(_))
    ));
    assert!(matches!(
        dyn_adapter
            .act(DesktopAction::PressKey {
                chord: "Return".into()
            })
            .await,
        Err(AdapterError::PermissionDenied(_))
    ));
    assert!(matches!(
        dyn_adapter.list_windows().await,
        Err(AdapterError::PermissionDenied(_))
    ));
}

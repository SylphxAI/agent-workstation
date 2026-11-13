//! Shared desktop types work everywhere; non-Linux sessions fail closed.

#[cfg(not(target_os = "linux"))]
use agent_workstation::{BrowserConfig, DesktopSession, Error, Profile, SessionConfig};
use agent_workstation::{Button, Frame};

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

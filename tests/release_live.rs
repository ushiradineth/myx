//! MAC6 source harness. These tests are deliberately never part of ordinary CI.
//! See support/mac_live.rs for the public fixture and owner-receipt contracts.
#[cfg(target_os = "macos")]
#[path = "support/mac_live.rs"]
mod mac_live;

#[test]
#[ignore = "owner-only MAC6 live account writes; read the fixture contract first"]
fn playlist_library_workflows() {
    allowance();
    #[cfg(target_os = "macos")]
    mac_live::run(mac_live::Scenario::Library).expect("MAC6 library gate failed");
    #[cfg(not(target_os = "macos"))]
    panic!("GateMissing: MAC6 requires native macOS");
}

#[test]
#[ignore = "owner-only >=300s processed playback and >=60s paused/idle resource gate"]
fn native_playback_resource_gate() {
    allowance();
    #[cfg(target_os = "macos")]
    mac_live::run(mac_live::Scenario::Resources).expect("MAC6 resource gate failed");
    #[cfg(not(target_os = "macos"))]
    panic!("GateMissing: MAC6 requires native macOS");
}

#[test]
#[ignore = "owner-only real terminal detach/reattach and physical media controls"]
fn daemon_detach_media_controls() {
    allowance();
    #[cfg(target_os = "macos")]
    mac_live::run(mac_live::Scenario::Media).expect("MAC6 terminal/media gate failed");
    #[cfg(not(target_os = "macos"))]
    panic!("GateMissing: MAC6 requires native macOS");
}

fn allowance() {
    // This must precede even fixture/config metadata access on every platform.
    assert_eq!(
        std::env::var("MYX_LIVE_ALLOW").as_deref(),
        Ok("1"),
        "GateMissing: explicitly set MYX_LIVE_ALLOW=1 for an owned fixture"
    );
}

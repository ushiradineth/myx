use myx::cli::Cli;
use myx::domain::{PlaybackStatus, ServiceState, Snapshot};
use myx::player::{controller::verify_owned_device, reducer};
use myx::soloist::protocol::PlayerEvent;
use serde_json::json;

#[test]
fn readiness_requires_both_services_and_verified_device_not_a_friendly_name() {
    let mut snapshot = Snapshot::starting("/private/test/config".into(), "Same Name".into());
    snapshot.web_api_ready = true;
    snapshot.native_media_ready = true;
    reducer::readiness(&mut snapshot);
    assert_ne!(snapshot.readiness, ServiceState::Ready);
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Authentication {
            logged_in: true,
            is_active: false,
        },
        false,
    );
    assert_ne!(snapshot.readiness, ServiceState::Ready);
    let devices =
        json!({"devices":[{"id":"other","name":"Same Name"},{"id":"owned","name":"Same Name"}]});
    assert!(!verify_owned_device(&devices, "Same Name"));
    assert!(verify_owned_device(&devices, "owned"));
    snapshot.owned_device_verified = true;
    reducer::readiness(&mut snapshot);
    assert_eq!(snapshot.readiness, ServiceState::Ready);
    assert_ne!(snapshot.playback, PlaybackStatus::Playing);
    snapshot.web_api_ready = false;
    reducer::readiness(&mut snapshot);
    assert_ne!(snapshot.readiness, ServiceState::Ready);
}

#[test]
fn duplicate_owned_id_is_ambiguous_and_missing_services_never_claim_playing() {
    assert!(!verify_owned_device(
        &json!({"devices":[{"id":"owned"},{"id":"owned"}]}),
        "owned"
    ));
    let mut snapshot = Snapshot::starting("/config".into(), "Player".into());
    reducer::observe(
        &mut snapshot,
        PlayerEvent::TrackChanged {
            uri: "spotify:track:pending".into(),
        },
        false,
    );
    reducer::observe(
        &mut snapshot,
        PlayerEvent::Playing {
            uri: "spotify:track:pending".into(),
            position_ms: 0,
        },
        false,
    );
    assert_ne!(snapshot.playback, PlaybackStatus::Playing);
    assert!(snapshot.track.unwrap().title.is_none());
}

#[test]
fn removed_modes_are_rejected_without_reflecting_input() {
    for input in [
        ["--backend", "PRIVATE-SENTINEL"],
        ["--api", "PRIVATE-SENTINEL"],
        ["--no-web-api", "PRIVATE-SENTINEL"],
    ] {
        let error = Cli::parse_from(["myx", input[0], input[1]])
            .unwrap_err()
            .to_string();
        assert!(!error.contains("PRIVATE-SENTINEL"));
    }
}

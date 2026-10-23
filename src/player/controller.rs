use crate::domain::{PlayerCommand, ServiceState, Snapshot};
use crate::soloist::RuntimePlayer;
use anyhow::{ensure, Result};

pub fn dispatch(
    player: &RuntimePlayer,
    snapshot: &mut Snapshot,
    command: PlayerCommand,
) -> Result<()> {
    if let PlayerCommand::Equalizer { settings } = command {
        snapshot.equalizer = settings.normalized();
        player.set_equalizer(snapshot.equalizer);
        return Ok(());
    }
    if matches!(command, PlayerCommand::Stop) {
        player.stop();
        return Ok(());
    }
    ensure!(
        matches!(
            snapshot.readiness,
            ServiceState::Ready | ServiceState::Degraded
        ) && snapshot.soloist_ready
            && snapshot.owned_device_verified,
        "Required services or owned-device verification need setup"
    );
    match command {
        PlayerCommand::PlayContext {
            uri,
            track,
            position_ms,
            shuffle,
        } => player.play_context_at(uri, track, position_ms, shuffle)?,
        PlayerCommand::PlayTracks {
            uris,
            start,
            position_ms,
            shuffle,
        } => player.play_tracks(uris, start, position_ms, shuffle)?,
        PlayerCommand::Play => player.play()?,
        PlayerCommand::Pause => player.pause()?,
        PlayerCommand::Toggle => player.toggle()?,
        PlayerCommand::Next => player.next()?,
        PlayerCommand::Previous => player.prev()?,
        PlayerCommand::Seek { position_ms } => player.seek(position_ms)?,
        PlayerCommand::Volume { percent } => {
            ensure!(percent <= 100, "Volume must be from 0 to 100");
            player.set_volume(((u32::from(percent) * 65535) / 100) as u16)?;
            snapshot.volume = percent;
        }
        PlayerCommand::Shuffle { enabled } => {
            player.shuffle(enabled)?;
            snapshot.shuffle = enabled;
        }
        PlayerCommand::Repeat { mode } => {
            player.set_repeat(mode)?;
            snapshot.repeat = mode;
        }
        PlayerCommand::Queue => player.request_queue()?,
        PlayerCommand::Add { uri } => player.add_to_queue(uri)?,
        PlayerCommand::Equalizer { .. } | PlayerCommand::Stop => unreachable!(),
    }
    Ok(())
}

pub fn verify_owned_device(devices: &serde_json::Value, owned_id: &str) -> bool {
    !owned_id.is_empty()
        && devices["devices"].as_array().is_some_and(|devices| {
            devices
                .iter()
                .filter(|device| device["id"].as_str() == Some(owned_id))
                .count()
                == 1
        })
}

/// Only a new explicit Play/Toggle may apply this paused cold-start restoration.
pub fn restored_command(last: Option<PlayerCommand>, snapshot: &Snapshot) -> PlayerCommand {
    let position_ms = snapshot.position.position_ms.min(u64::from(u32::MAX)) as u32;
    let track = snapshot.track.as_ref().map(|track| track.uri.clone());
    match last {
        Some(PlayerCommand::PlayContext {
            uri,
            track: previous,
            ..
        }) => PlayerCommand::PlayContext {
            uri,
            track: track.or(previous),
            position_ms,
            shuffle: snapshot.shuffle,
        },
        Some(PlayerCommand::PlayTracks { uris, start, .. }) => PlayerCommand::PlayTracks {
            start: track.filter(|uri| uris.contains(uri)).or(start),
            uris,
            position_ms,
            shuffle: snapshot.shuffle,
        },
        _ => track.map_or(PlayerCommand::Play, |uri| PlayerCommand::PlayContext {
            uri,
            track: None,
            position_ms,
            shuffle: snapshot.shuffle,
        }),
    }
}

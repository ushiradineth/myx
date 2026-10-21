//! Side-effect-free player argument parsing. `parse_from` takes full argv,
//! including the executable name. Environment overrides belong to the caller.

use std::time::Duration;

use anyhow::{bail, ensure, Result};

use crate::config::BackendKind;

#[derive(Debug, Default)]
pub struct Cli {
    pub backend: Option<BackendKind>,
    pub uri: Option<String>,
    pub headless: bool,
    pub duration: Option<Duration>,
    pub no_web_api: bool,
    pub non_interactive: bool,
    pub volume: Option<u8>,
    pub help: bool,
}

impl Cli {
    /// Parse full argv, with no network, environment, filesystem or TTY access.
    /// Headless playback requires an explicit bounded duration.
    pub fn parse_from<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut args = args.into_iter();
        let _program = args.next();
        let mut cli = Self::default();
        let mut positional_only = false;
        while let Some(arg) = args.next() {
            let arg = arg.as_ref();
            if !positional_only && arg == "--" {
                positional_only = true;
                continue;
            }
            if !positional_only && arg.starts_with('-') {
                let (flag, inline) = arg
                    .split_once('=')
                    .map_or((arg, None), |(k, v)| (k, Some(v)));
                match flag {
                    "--backend" | "--api" | "--duration" | "--volume" => {
                        let next;
                        let value = match inline {
                            Some(value) => value,
                            None => {
                                next = args
                                    .next()
                                    .ok_or_else(|| anyhow::anyhow!("option requires a value"))?;
                                next.as_ref()
                            }
                        };
                        ensure!(
                            !value.is_empty() && !value.starts_with('-'),
                            "option requires a value"
                        );
                        match flag {
                            "--backend" | "--api" => {
                                ensure!(cli.backend.is_none(), "backend specified more than once");
                                cli.backend = Some(value.parse()?);
                            }
                            "--duration" => {
                                ensure!(
                                    cli.duration.is_none(),
                                    "duration specified more than once"
                                );
                                cli.duration = Some(Duration::from_secs(bounded_number(
                                    value,
                                    1,
                                    86400,
                                    "duration must be an integer from 1 to 86400 seconds",
                                )?));
                            }
                            "--volume" => {
                                ensure!(cli.volume.is_none(), "volume specified more than once");
                                cli.volume = Some(bounded_number(
                                    value,
                                    0,
                                    100,
                                    "volume must be an integer from 0 to 100",
                                )? as u8);
                            }
                            _ => unreachable!(),
                        }
                    }
                    "--headless" | "--no-web-api" | "--non-interactive" | "--help" | "-h" => {
                        ensure!(inline.is_none(), "flag does not accept a value");
                        match flag {
                            "--headless" => cli.headless = true,
                            "--no-web-api" => cli.no_web_api = true,
                            "--non-interactive" => cli.non_interactive = true,
                            _ => cli.help = true,
                        }
                    }
                    _ => bail!("unknown option; use --help for usage"),
                }
            } else {
                ensure!(cli.uri.is_none(), "only one Spotify URI may be supplied");
                cli.uri = Some(normalize_uri(arg)?);
            }
        }
        ensure!(
            cli.headless || cli.duration.is_none(),
            "--duration requires --headless"
        );
        ensure!(
            !cli.headless || cli.duration.is_some() || cli.help,
            "--headless requires --duration (1 to 86400 seconds)"
        );
        Ok(cli)
    }
}

fn bounded_number(value: &str, min: u64, max: u64, error: &'static str) -> Result<u64> {
    ensure!(
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        error
    );
    let number = value.parse::<u64>().map_err(|_| anyhow::anyhow!(error))?;
    ensure!((min..=max).contains(&number), error);
    Ok(number)
}

fn normalize_uri(value: &str) -> Result<String> {
    let invalid = || {
        anyhow::anyhow!("expected a Spotify track, album, playlist, artist or episode URI or HTTPS share URL with a 22-character ID")
    };
    let (kind, id) = if let Some(rest) = value.strip_prefix("spotify:") {
        rest.split_once(':').ok_or_else(invalid)?
    } else if let Some(rest) = value.strip_prefix("https://open.spotify.com/") {
        let path = rest.split(['?', '#']).next().ok_or_else(invalid)?;
        // Spotify's localized share links use an optional /intl-xx prefix.
        let path = if let Some((locale, localized_path)) = path.split_once('/') {
            if locale
                .strip_prefix("intl-")
                .is_some_and(|s| s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic()))
            {
                localized_path
            } else {
                path
            }
        } else {
            path
        };
        path.split_once('/').ok_or_else(invalid)?
    } else {
        return Err(invalid());
    };
    ensure!(
        matches!(kind, "track" | "album" | "playlist" | "artist" | "episode")
            && id.len() == 22
            && id.bytes().all(|b| b.is_ascii_alphanumeric()),
        invalid()
    );
    Ok(format!("spotify:{kind}:{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdefghijkl";

    #[test]
    fn defaults_and_full_headless_options() {
        let cli = Cli::parse_from(["myx"]).unwrap();
        assert!(cli.backend.is_none() && cli.uri.is_none() && cli.duration.is_none());
        assert!(!cli.headless && !cli.no_web_api && !cli.non_interactive && !cli.help);
        assert!(cli.volume.is_none());
        let cli = Cli::parse_from([
            "myx",
            "--api",
            "soloist",
            "--headless",
            "--duration=86400",
            "--no-web-api",
            "--non-interactive",
            "--volume",
            "0",
        ])
        .unwrap();
        assert_eq!(cli.backend, Some(BackendKind::Soloist));
        assert_eq!(cli.duration, Some(Duration::from_secs(86400)));
        assert!(cli.headless && cli.no_web_api && cli.non_interactive);
        assert_eq!(cli.volume, Some(0));
        assert_eq!(
            Cli::parse_from(["myx", "--backend=normal", "--volume=100"])
                .unwrap()
                .backend,
            Some(BackendKind::Normal)
        );
        assert!(Cli::parse_from(["myx", "-h"]).unwrap().help);
        assert!(Cli::parse_from(["myx", "--help"]).unwrap().help);
        assert_eq!(
            Cli::parse_from(["myx", "--duration", "1", "--headless"])
                .unwrap()
                .duration,
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn invalid_options_and_unsafe_automation_are_rejected_without_echoing_input() {
        for args in [
            vec!["--backend", "secret-value"],
            vec!["--backend"],
            vec!["--backend", "normal", "--api", "soloist"],
            vec!["--headless"],
            vec!["--duration", "1"],
            vec!["--headless", "--duration", "0"],
            vec!["--headless", "--duration", "86401"],
            vec!["--headless", "--duration", "1.5"],
            vec!["--headless", "--duration", "18446744073709551616"],
            vec!["--volume", "101"],
            vec!["--volume", "-1"],
            vec!["--volume", "+1"],
            vec!["--volume", "secret-value"],
            vec!["--headless=secret-value"],
            vec!["--secret-value"],
            vec!["secret-value"],
        ] {
            let error = Cli::parse_from(std::iter::once("myx").chain(args))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret-value"));
        }
    }

    #[test]
    fn canonical_uris_and_share_urls_normalize_without_network_access() {
        for kind in ["track", "album", "playlist", "artist", "episode"] {
            let canonical = format!("spotify:{kind}:{ID}");
            for input in [
                canonical.clone(),
                format!("https://open.spotify.com/{kind}/{ID}?si=secret-value"),
                format!("https://open.spotify.com/intl-en/{kind}/{ID}?si=secret-value#fragment"),
            ] {
                assert_eq!(
                    Cli::parse_from(["myx", &input]).unwrap().uri.as_deref(),
                    Some(canonical.as_str())
                );
            }
        }
        for input in [
            format!("spotify:show:{ID}"),
            format!("spotify:track:{ID}?si=secret-value"),
            format!("http://open.spotify.com/track/{ID}"),
            format!("https://open.spotify.com.evil/track/{ID}"),
            format!("https://open.spotify.com/track/{ID}/extra"),
            "spotify:track:short".into(),
            format!("spotify:track:{}!", &ID[..21]),
        ] {
            assert!(Cli::parse_from(["myx", &input]).is_err());
        }
        let uri = format!("spotify:track:{ID}");
        assert!(Cli::parse_from(["myx", &uri, &uri]).is_err());
        assert_eq!(
            Cli::parse_from(["myx", "--", &uri]).unwrap().uri.as_deref(),
            Some(uri.as_str())
        );
    }
}

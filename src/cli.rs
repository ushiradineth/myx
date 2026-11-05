//! Side-effect-free full-argv parsing. Environment and filesystem access belong
//! to the caller, including selection of the configuration before its first use.

use std::{path::PathBuf, time::Duration};

use anyhow::{bail, ensure, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Tui,
    Daemon(DaemonCommand),
    Service(ServiceCommand),
    Config(ConfigCommand),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigCommand {
    Init,
    Check,
    Migrate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonCommand {
    Run,
    Start,
    Stop,
    Restart,
    Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceCommand {
    Install,
    Uninstall,
    Status,
}

#[derive(Debug, Default)]
pub struct Cli {
    pub mode: Mode,
    pub uri: Option<String>,
    pub headless: bool,
    pub duration: Option<Duration>,
    pub non_interactive: bool,
    pub volume: Option<u8>,
    pub help: bool,
    pub version: bool,
    pub config: Option<PathBuf>,
    pub json: bool,
    pub output: Option<PathBuf>,
}

impl Cli {
    /// Parse full argv without environment, filesystem, network or TTY access.
    pub fn parse_from<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut args = args.into_iter();
        let _program = args.next();
        let mut cli = Self::default();
        let mut positional_only = false;
        let mut command_group = None;
        let mut command_seen = false;
        while let Some(arg) = args.next() {
            let arg = arg.as_ref();
            if !positional_only && arg == "--" {
                positional_only = true;
                continue;
            }
            if !positional_only && arg.starts_with('-') {
                let (flag, inline) = arg
                    .split_once('=')
                    .map_or((arg, None), |(key, value)| (key, Some(value)));
                match flag {
                    "--backend" | "--api" | "--no-web-api" | "--theme" => {
                        bail!("removed mode or option; use --help for supported commands")
                    }
                    "--duration" | "--volume" | "--config" | "--output" => {
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
                            "--output" => {
                                ensure!(cli.output.is_none(), "output specified more than once");
                                cli.output = Some(PathBuf::from(value));
                            }
                            _ => {
                                ensure!(cli.config.is_none(), "config specified more than once");
                                cli.config = Some(PathBuf::from(value));
                            }
                        }
                    }
                    "--headless" | "--non-interactive" | "--json" | "--help" | "-h"
                    | "--version" | "-V" => {
                        ensure!(inline.is_none(), "flag does not accept a value");
                        match flag {
                            "--headless" => cli.headless = true,
                            "--non-interactive" => cli.non_interactive = true,
                            "--json" => cli.json = true,
                            "--version" | "-V" => cli.version = true,
                            _ => cli.help = true,
                        }
                    }
                    _ => bail!("unknown option; use --help for usage"),
                }
            } else if !positional_only && arg == "theme" {
                bail!("removed mode or option; use --help for supported commands");
            } else if command_group.is_some() && !command_seen {
                cli.mode = match (command_group, arg) {
                    (Some("daemon"), "run") => Mode::Daemon(DaemonCommand::Run),
                    (Some("daemon"), "start") => Mode::Daemon(DaemonCommand::Start),
                    (Some("daemon"), "stop") => Mode::Daemon(DaemonCommand::Stop),
                    (Some("daemon"), "restart") => Mode::Daemon(DaemonCommand::Restart),
                    (Some("daemon"), "status") => Mode::Daemon(DaemonCommand::Status),
                    (Some("service"), "install") => Mode::Service(ServiceCommand::Install),
                    (Some("service"), "uninstall") => Mode::Service(ServiceCommand::Uninstall),
                    (Some("service"), "status") => Mode::Service(ServiceCommand::Status),
                    (Some("config"), "init") => Mode::Config(ConfigCommand::Init),
                    (Some("config"), "check") => Mode::Config(ConfigCommand::Check),
                    (Some("config"), "migrate") => Mode::Config(ConfigCommand::Migrate),
                    _ => bail!("unknown management command; use --help for usage"),
                };
                command_seen = true;
            } else if !positional_only && matches!(arg, "daemon" | "service" | "config") {
                ensure!(
                    command_group.is_none() && cli.uri.is_none(),
                    "unexpected command"
                );
                command_group = Some(match arg {
                    "daemon" => "daemon",
                    "service" => "service",
                    _ => "config",
                });
            } else {
                ensure!(
                    command_group.is_none(),
                    "management commands do not accept a URI"
                );
                ensure!(cli.uri.is_none(), "only one Spotify URI may be supplied");
                cli.uri = Some(normalize_uri(arg)?);
            }
        }
        ensure!(
            command_group.is_none() || command_seen || cli.help || cli.version,
            "management command required; use --help for usage"
        );
        if command_group.is_some() {
            ensure!(
                !cli.headless && cli.duration.is_none() && cli.volume.is_none(),
                "playback options cannot be used with management commands"
            );
        }
        ensure!(
            cli.output.is_none()
                || matches!(
                    cli.mode,
                    Mode::Config(ConfigCommand::Init | ConfigCommand::Migrate)
                ),
            "--output requires config init or config migrate"
        );
        ensure!(
            cli.mode != Mode::Config(ConfigCommand::Migrate)
                || cli.output.is_some()
                || cli.help
                || cli.version,
            "config migrate requires an explicit new --output candidate"
        );
        ensure!(
            !cli.json
                || matches!(
                    cli.mode,
                    Mode::Daemon(DaemonCommand::Status) | Mode::Service(ServiceCommand::Status)
                ),
            "--json requires a status command"
        );
        ensure!(
            cli.headless || cli.duration.is_none(),
            "--duration requires --headless"
        );
        ensure!(
            !cli.headless || cli.duration.is_some() || cli.help || cli.version,
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

pub fn normalize_uri(value: &str) -> Result<String> {
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
    fn defaults_and_bounded_playback_options() {
        let cli = Cli::parse_from(["myx"]).unwrap();
        assert_eq!(cli.mode, Mode::Tui);
        assert!(cli.uri.is_none() && cli.duration.is_none() && cli.config.is_none());
        assert!(!cli.help && !cli.version && !cli.json && !cli.headless);
        for (seconds, volume) in [("1", "0"), ("86400", "100")] {
            let cli = Cli::parse_from([
                "myx",
                "--headless",
                "--duration",
                seconds,
                "--volume",
                volume,
                "--non-interactive",
                "--config=/private/myx.toml",
            ])
            .unwrap();
            assert_eq!(cli.duration.unwrap().as_secs().to_string(), seconds);
            assert_eq!(cli.volume.unwrap().to_string(), volume);
            assert!(cli.non_interactive);
            assert_eq!(cli.config, Some(PathBuf::from("/private/myx.toml")));
        }
        assert!(Cli::parse_from(["myx", "-h"]).unwrap().help);
        assert!(Cli::parse_from(["myx", "--help"]).unwrap().help);
        assert!(Cli::parse_from(["myx", "-V"]).unwrap().version);
        assert!(Cli::parse_from(["myx", "--version"]).unwrap().version);
    }

    #[test]
    fn management_commands_and_global_flags() {
        for (group, command, expected) in [
            ("daemon", "run", Mode::Daemon(DaemonCommand::Run)),
            ("daemon", "start", Mode::Daemon(DaemonCommand::Start)),
            ("daemon", "stop", Mode::Daemon(DaemonCommand::Stop)),
            ("daemon", "restart", Mode::Daemon(DaemonCommand::Restart)),
            ("daemon", "status", Mode::Daemon(DaemonCommand::Status)),
            ("service", "install", Mode::Service(ServiceCommand::Install)),
            (
                "service",
                "uninstall",
                Mode::Service(ServiceCommand::Uninstall),
            ),
            ("service", "status", Mode::Service(ServiceCommand::Status)),
        ] {
            let cli = Cli::parse_from([
                "myx",
                "--config",
                "/private/test.toml",
                group,
                "--non-interactive",
                command,
            ])
            .unwrap();
            assert_eq!(cli.mode, expected);
            assert!(cli.non_interactive);
            if command == "status" {
                assert!(
                    Cli::parse_from(["myx", group, command, "--json"])
                        .unwrap()
                        .json
                );
            }
        }
        assert!(Cli::parse_from(["myx", "daemon", "--help"]).unwrap().help);
        assert!(Cli::parse_from(["myx", "service", "--help"]).unwrap().help);
    }

    #[test]
    fn removed_and_invalid_inputs_never_echo_supplied_secrets() {
        for args in [
            vec!["--backend=PRIVATE-SENTINEL"],
            vec!["--api", "PRIVATE-SENTINEL"],
            vec!["--no-web-api"],
            vec!["theme", "PRIVATE-SENTINEL"],
            vec!["--theme=PRIVATE-SENTINEL"],
            vec!["--headless"],
            vec!["--duration", "1"],
            vec!["--headless", "--duration", "0"],
            vec!["--headless", "--duration", "86401"],
            vec!["--headless", "--duration", "18446744073709551616"],
            vec!["--volume", "101"],
            vec!["--volume", "-1"],
            vec!["--volume", "+1"],
            vec!["--volume=PRIVATE-SENTINEL"],
            vec!["--help=PRIVATE-SENTINEL"],
            vec!["--PRIVATE-SENTINEL"],
            vec!["PRIVATE-SENTINEL"],
            vec!["daemon"],
            vec!["service"],
            vec!["daemon", "PRIVATE-SENTINEL"],
            vec!["service", "run"],
            vec!["daemon", "status", "--volume=1"],
            vec!["--json"],
            vec!["daemon", "start", "--json"],
            vec!["--config"],
            vec!["--config="],
            vec!["--config=a", "--config=b"],
            vec!["--volume=1", "--volume=2"],
            vec!["--headless", "--duration=1", "--duration=2"],
        ] {
            let error = Cli::parse_from(std::iter::once("myx").chain(args))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("PRIVATE-SENTINEL"));
        }
        for option in ["--backend", "--api", "--no-web-api", "theme"] {
            assert!(Cli::parse_from(["myx", option])
                .unwrap_err()
                .to_string()
                .contains("removed"));
        }
    }

    #[test]
    fn canonical_uris_and_share_urls_remain_supported() {
        for kind in ["track", "album", "playlist", "artist", "episode"] {
            let canonical = format!("spotify:{kind}:{ID}");
            for input in [
                canonical.clone(),
                format!("https://open.spotify.com/{kind}/{ID}?si=PRIVATE-SENTINEL"),
                format!(
                    "https://open.spotify.com/intl-en/{kind}/{ID}?si=PRIVATE-SENTINEL#fragment"
                ),
            ] {
                assert_eq!(
                    Cli::parse_from(["myx", &input]).unwrap().uri.as_deref(),
                    Some(canonical.as_str())
                );
            }
        }
        for input in [
            format!("spotify:show:{ID}"),
            format!("spotify:track:{ID}?si=PRIVATE-SENTINEL"),
            format!("http://open.spotify.com/track/{ID}"),
            format!("https://open.spotify.com.evil/track/{ID}"),
            format!("https://open.spotify.com/track/{ID}/extra"),
            "spotify:track:short".into(),
        ] {
            assert!(Cli::parse_from(["myx", &input]).is_err());
        }
        let uri = format!("spotify:track:{ID}");
        assert!(Cli::parse_from(["myx", &uri, &uri]).is_err());
        assert!(Cli::parse_from(["myx", "daemon", "run", &uri]).is_err());
        assert_eq!(
            Cli::parse_from(["myx", "--", &uri]).unwrap().uri.as_deref(),
            Some(uri.as_str())
        );
    }
}

//! X-server-style launch handling: argv parsing, display resolution,
//! the `/tmp/.X<N>-lock` protocol, socket binding, and the lightdm
//! readiness handshake. See
//! `docs/superpowers/specs/2026-06-12-lightdm-launch-design.md`.

use std::{os::fd::RawFd, path::PathBuf};

/// Display yserver uses when neither an explicit display nor `-displayfd`
/// is given. 7 avoids clashing with a real Xorg on `:0` (existing
/// convention).
pub const DEFAULT_DISPLAY: u16 = 7;

/// Parsed X-server-style command line. Fields the issue's items 1-2 act
/// on; `vt`/`seat` are parsed + logged but otherwise ignored (logind owns
/// the seat/VT), `auth_file` is stashed for the deferred item 4.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LaunchOptions {
    /// `:N` or bare `N` → explicit display; `None` → resolved in `run()`.
    pub display: Option<u16>,
    /// `-displayfd N`.
    pub displayfd: Option<RawFd>,
    /// `vtN` — logged, otherwise ignored.
    pub vt: Option<u32>,
    /// `-seat NAME` — logged, otherwise ignored.
    pub seat: Option<String>,
    /// `-auth FILE` — stashed for item 4, unused now.
    pub auth_file: Option<PathBuf>,
}

fn next_value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} requires a value"))
}

/// Parse X-server-style argv. Tolerates unknown flags (warn + skip);
/// hard-errors only on malformed *explicit* requests and missing values
/// for known value-taking flags.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<LaunchOptions, String> {
    let mut o = LaunchOptions::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if let Some(rest) = arg.strip_prefix(':') {
            o.display = Some(
                rest.parse::<u16>()
                    .map_err(|_| format!("invalid display argument: {arg}"))?,
            );
        } else if let Some(rest) = arg.strip_prefix("vt") {
            o.vt = Some(
                rest.parse::<u32>()
                    .map_err(|_| format!("invalid vt argument: {arg}"))?,
            );
        } else if arg == "-seat" {
            o.seat = Some(next_value(&mut it, "-seat")?);
        } else if arg == "-auth" {
            o.auth_file = Some(PathBuf::from(next_value(&mut it, "-auth")?));
        } else if arg == "-displayfd" {
            let v = next_value(&mut it, "-displayfd")?;
            o.displayfd = Some(
                v.parse::<RawFd>()
                    .map_err(|_| format!("invalid -displayfd argument: {v}"))?,
            );
        } else if matches!(
            arg.as_str(),
            "-nolisten" | "-config" | "-layout" | "-background"
        ) {
            // Known value-taking no-ops. Consume + ignore the value; a
            // missing value is tolerated (these don't affect us).
            if it.next().is_none() {
                log::warn!("yserver: {arg} given without a value; ignoring");
            }
        } else if arg == "-novtswitch" {
            // Known no-arg no-op (lightdm passes it).
        } else if let Ok(n) = arg.parse::<u16>() {
            // Bare number → display. Keeps `yserver 7` (Justfile) working.
            o.display = Some(n);
        } else {
            log::warn!("yserver: ignoring unrecognized argument: {arg}");
        }
    }
    Ok(o)
}

/// How `run()` should obtain the display + whether to take the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Use this exact display. `lock` is true only when `-displayfd` is
    /// absent (Xorg sets `nolock = TRUE` whenever `-displayfd` is parsed).
    Explicit { display: u16, lock: bool },
    /// Scan for the lowest free display (gdm-style `-displayfd`); no lock.
    AutoPick,
}

/// The display-resolution table from the spec. Lock iff `-displayfd` is
/// absent.
#[must_use]
pub fn resolve(opts: &LaunchOptions) -> Resolution {
    match (opts.display, opts.displayfd) {
        (Some(display), None) => Resolution::Explicit {
            display,
            lock: true,
        },
        (Some(display), Some(_)) => Resolution::Explicit {
            display,
            lock: false,
        },
        (None, Some(_)) => Resolution::AutoPick,
        (None, None) => Resolution::Explicit {
            display: DEFAULT_DISPLAY,
            lock: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<LaunchOptions, String> {
        parse_args(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn lightdm_default_argv_parses_clean() {
        let o = parse(&[
            ":0",
            "-seat",
            "seat0",
            "-auth",
            "/var/run/lightdm/root/:0",
            "-nolisten",
            "tcp",
            "vt7",
            "-novtswitch",
        ])
        .unwrap();
        assert_eq!(o.display, Some(0));
        assert_eq!(o.displayfd, None);
        assert_eq!(o.vt, Some(7));
        assert_eq!(o.seat.as_deref(), Some("seat0"));
        assert_eq!(o.auth_file, Some(PathBuf::from("/var/run/lightdm/root/:0")));
    }

    #[test]
    fn gdm_style_displayfd_without_explicit_display() {
        let o = parse(&["-displayfd", "12"]).unwrap();
        assert_eq!(o.displayfd, Some(12));
        assert_eq!(o.display, None);
    }

    #[test]
    fn bare_number_is_back_compat_display() {
        assert_eq!(parse(&["7"]).unwrap().display, Some(7));
        assert_eq!(parse(&[]).unwrap().display, None);
    }

    #[test]
    fn explicit_colon_display() {
        assert_eq!(parse(&[":42"]).unwrap().display, Some(42));
    }

    #[test]
    fn unknown_flags_are_tolerated() {
        let o = parse(&["-bogus", "--whatever", ":1"]).unwrap();
        assert_eq!(o.display, Some(1));
    }

    #[test]
    fn malformed_explicit_requests_error() {
        assert!(parse(&[":foo"]).is_err());
        assert!(parse(&["vtbad"]).is_err());
        assert!(parse(&["-displayfd", "notanumber"]).is_err());
    }

    #[test]
    fn missing_required_value_errors() {
        assert!(parse(&["-seat"]).is_err());
        assert!(parse(&["-auth"]).is_err());
        assert!(parse(&["-displayfd"]).is_err());
    }

    #[test]
    fn resolution_table() {
        let mk = |d: Option<u16>, fd: Option<RawFd>| LaunchOptions {
            display: d,
            displayfd: fd,
            ..Default::default()
        };
        assert_eq!(
            resolve(&mk(Some(0), None)),
            Resolution::Explicit {
                display: 0,
                lock: true
            }
        );
        assert_eq!(
            resolve(&mk(Some(0), Some(9))),
            Resolution::Explicit {
                display: 0,
                lock: false
            }
        );
        assert_eq!(resolve(&mk(None, Some(9))), Resolution::AutoPick);
        assert_eq!(
            resolve(&mk(None, None)),
            Resolution::Explicit {
                display: DEFAULT_DISPLAY,
                lock: true
            }
        );
    }
}

//! `xauth list` for ssh's X11 forwarding on Windows, where there is no xauth.
//!
//! ssh.exe runs `xauth list unix:N` (`-Y`) to swap the real cookie in for the
//! fake one it forwards; `-X` runs `generate ... untrusted`, which needs the
//! SECURITY extension yserver lacks, so `generate` fails and ssh says so.

use std::{env, fmt::Write as _, fs, path::PathBuf, process::ExitCode};

use yserver_core::xauth::{XauthRecord, parse_records};

const FAMILY_INTERNET: u16 = 0;
const FAMILY_LOCAL: u16 = 256;
const FAMILY_WILD: u16 = 0xffff;

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("xauth: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut file = None;
    let mut args = args.into_iter();
    let command = loop {
        match args.next().as_deref() {
            Some("-f") => file = Some(PathBuf::from(args.next().ok_or("-f needs a file")?)),
            Some("-q" | "-i" | "-b" | "-n" | "-v") => {}
            Some(cmd) => break cmd.to_owned(),
            None => return Err("only `list [display ...]` is supported".into()),
        }
    };
    let displays: Vec<String> = args.collect();
    match command.as_str() {
        "list" => {
            let path = file
                .or_else(default_file)
                .ok_or("no XAUTHORITY or LOCALAPPDATA")?;
            let bytes =
                fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let host = env::var("COMPUTERNAME")
                .or_else(|_| env::var("HOSTNAME"))
                .ok();
            print!(
                "{}",
                list(&parse_records(&bytes), &displays, host.as_deref())?
            );
            Ok(())
        }
        "generate" => Err(
            "yserver has no SECURITY extension, so there is no untrusted cookie for \
             `ssh -X`; use `ssh -Y`"
                .into(),
        ),
        other => Err(format!("`{other}` is not supported, only `list`")),
    }
}

/// XAUTHORITY, YSERVER_AUTH, then the file start-yserver.cmd creates.
fn default_file() -> Option<PathBuf> {
    env::var_os("XAUTHORITY")
        .or_else(|| env::var_os("YSERVER_AUTH"))
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(r"yserver\Xauthority"))
        })
}

/// Real xauth's `list` output for the records matching any of `displays` (all if none).
fn list(
    records: &[XauthRecord],
    displays: &[String],
    this_host: Option<&str>,
) -> Result<String, String> {
    let wanted = displays
        .iter()
        .map(|d| parse_display(d).ok_or_else(|| format!("bad display name \"{d}\"")))
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = String::new();
    for r in records {
        if wanted.is_empty()
            || wanted
                .iter()
                .any(|(host, num)| matches(r, host, num, this_host))
        {
            let _ = writeln!(
                out,
                "{}:{}  {}  {}",
                entry_name(r),
                String::from_utf8_lossy(&r.number),
                String::from_utf8_lossy(&r.name),
                hex(&r.data)
            );
        }
    }
    Ok(out)
}

/// `host:N[.S]` to (host, "N").
fn parse_display(display: &str) -> Option<(&str, &str)> {
    let (host, rest) = display.rsplit_once(':')?;
    let num = rest.split('.').next()?;
    (!num.is_empty() && num.bytes().all(|b| b.is_ascii_digit())).then_some((host, num))
}

/// Wild matches any host, Local only this machine, Internet only its own address.
fn matches(r: &XauthRecord, host: &str, num: &str, this_host: Option<&str>) -> bool {
    let local_display = matches!(host, "" | "unix" | "localhost" | "127.0.0.1")
        || this_host.is_some_and(|h| h.eq_ignore_ascii_case(host));
    r.number == num.as_bytes()
        && match r.family {
            FAMILY_WILD => true,
            FAMILY_LOCAL => {
                local_display
                    && this_host.is_some_and(|h| h.as_bytes().eq_ignore_ascii_case(&r.address))
            }
            FAMILY_INTERNET => dotted(&r.address).is_some_and(|a| a == host),
            _ => false,
        }
}

/// The address column as xauth -n prints it.
fn entry_name(r: &XauthRecord) -> String {
    match r.family {
        FAMILY_LOCAL => format!("{}/unix", String::from_utf8_lossy(&r.address)),
        FAMILY_INTERNET if dotted(&r.address).is_some() => dotted(&r.address).unwrap_or_default(),
        family => format!("#{family:04x}#{}#", hex(&r.address)),
    }
}

fn dotted(address: &[u8]) -> Option<String> {
    let [a, b, c, d] = address else { return None };
    Some(format!("{a}.{b}.{c}.{d}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cookie file start-yserver.cmd wrote on Windows: one FamilyWild record, display 0.
    const START_YSERVER_FILE: [u8; 45] = [
        0xff, 0xff, 0x00, 0x00, 0x00, 0x01, 0x30, 0x00, 0x12, 0x4d, 0x49, 0x54, 0x2d, 0x4d, 0x41,
        0x47, 0x49, 0x43, 0x2d, 0x43, 0x4f, 0x4f, 0x4b, 0x49, 0x45, 0x2d, 0x31, 0x00, 0x10, 0xfc,
        0xfe, 0xee, 0xde, 0xb2, 0x70, 0x38, 0x4f, 0xa4, 0x86, 0x71, 0xf1, 0xab, 0x7e, 0x22, 0x22,
    ];
    /// What xauth 1.1 prints for it, for `list` and for every display numbered 0.
    const START_YSERVER_LIST: &str =
        "#ffff##:0  MIT-MAGIC-COOKIE-1  fcfeeedeb270384fa48671f1ab7e2222\n";

    /// Written by xauth 1.1: `add winbox/unix:7 ...` then `add 192.168.5.9:7 ...`.
    const XAUTH_FILE: [u8; 100] = [
        0x01, 0x00, 0x00, 0x06, 0x77, 0x69, 0x6e, 0x62, 0x6f, 0x78, 0x00, 0x01, 0x37, 0x00, 0x12,
        0x4d, 0x49, 0x54, 0x2d, 0x4d, 0x41, 0x47, 0x49, 0x43, 0x2d, 0x43, 0x4f, 0x4f, 0x4b, 0x49,
        0x45, 0x2d, 0x31, 0x00, 0x10, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99,
        0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x00, 0x00, 0x04, 0xc0, 0xa8, 0x05, 0x09, 0x00,
        0x01, 0x37, 0x00, 0x12, 0x4d, 0x49, 0x54, 0x2d, 0x4d, 0x41, 0x47, 0x49, 0x43, 0x2d, 0x43,
        0x4f, 0x4f, 0x4b, 0x49, 0x45, 0x2d, 0x31, 0x00, 0x10, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
        0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10,
    ];

    fn list_on(host: &str, bytes: &[u8], displays: &[&str]) -> Result<String, String> {
        let displays: Vec<String> = displays.iter().map(|d| (*d).to_owned()).collect();
        list(&parse_records(bytes), &displays, Some(host))
    }

    fn list_of(bytes: &[u8], displays: &[&str]) -> Result<String, String> {
        list_on("otherhost", bytes, displays)
    }

    #[test]
    fn wild_record_matches_every_spelling_of_display_0() {
        for d in [
            "unix:0",
            ":0",
            "127.0.0.1:0",
            "localhost:0",
            "unix:0.0",
            "localhost:0.0",
        ] {
            assert_eq!(
                list_of(&START_YSERVER_FILE, &[d]).unwrap(),
                START_YSERVER_LIST,
                "{d}"
            );
        }
        assert_eq!(
            list_of(&START_YSERVER_FILE, &[]).unwrap(),
            START_YSERVER_LIST
        );
    }

    #[test]
    fn other_display_number_lists_nothing() {
        assert_eq!(list_of(&START_YSERVER_FILE, &["unix:1"]).unwrap(), "");
    }

    #[test]
    fn local_and_internet_records_print_as_xauth_does() {
        assert_eq!(
            list_of(&XAUTH_FILE, &[]).unwrap(),
            "winbox/unix:7  MIT-MAGIC-COOKIE-1  00112233445566778899aabbccddeeff\n\
             192.168.5.9:7  MIT-MAGIC-COOKIE-1  0102030405060708090a0b0c0d0e0f10\n"
        );
    }

    #[test]
    fn internet_record_matches_its_own_address() {
        assert_eq!(
            list_of(&XAUTH_FILE, &["192.168.5.9:7"]).unwrap(),
            "192.168.5.9:7  MIT-MAGIC-COOKIE-1  0102030405060708090a0b0c0d0e0f10\n"
        );
        assert_eq!(list_of(&XAUTH_FILE, &["unix:8"]).unwrap(), "");
    }

    #[test]
    fn local_record_matches_only_on_its_own_host() {
        let winbox = "winbox/unix:7  MIT-MAGIC-COOKIE-1  00112233445566778899aabbccddeeff\n";
        assert_eq!(list_on("WINBOX", &XAUTH_FILE, &["unix:7"]).unwrap(), winbox);
        assert_eq!(
            list_on("winbox", &XAUTH_FILE, &["localhost:7.0"]).unwrap(),
            winbox
        );
        assert_eq!(list_of(&XAUTH_FILE, &["unix:7"]).unwrap(), "");
    }

    #[test]
    fn display_without_number_is_an_error() {
        assert!(list_of(&START_YSERVER_FILE, &["foo"]).is_err());
    }

    #[test]
    fn generate_fails_so_ssh_x_reports_it() {
        let args = [
            "-f",
            "x",
            "generate",
            "unix:0",
            "MIT-MAGIC-COOKIE-1",
            "untrusted",
        ];
        assert!(run(args.iter().map(|a| (*a).to_owned()).collect()).is_err());
    }
}

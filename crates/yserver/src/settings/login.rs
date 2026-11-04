//! Start at sign-in on Windows: a value under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` holding the command
//! line that started this run. The quoting is `CommandLineToArgvW`'s.

use std::path::Path;

/// The command line running `exe` with `args`: the program always quoted
/// (its own rule: up to the next quote, no escapes), each argument quoted
/// only when it must be.
#[must_use]
pub fn command_line(exe: &Path, args: &[String]) -> String {
    let mut line = format!("\"{}\"", exe.display());
    for arg in args {
        line.push(' ');
        quote_into(&mut line, arg);
    }
    line
}

/// Append `arg` so that `CommandLineToArgvW` gives it back unchanged.
fn quote_into(line: &mut String, arg: &str) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        line.push_str(arg);
        return;
    }
    line.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are escaped, and so is it.
                line.extend(std::iter::repeat_n('\\', 2 * backslashes + 1));
                line.push('"');
                backslashes = 0;
            }
            _ => {
                line.extend(std::iter::repeat_n('\\', backslashes));
                line.push(c);
                backslashes = 0;
            }
        }
    }
    // Before the closing quote, backslashes are escaped too.
    line.extend(std::iter::repeat_n('\\', 2 * backslashes));
    line.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(|&a| a.to_owned()).collect();
        command_line(Path::new(r"C:\Program Files\yserver\yserver.exe"), &args)
    }

    #[test]
    fn the_program_is_quoted_and_plain_arguments_are_not() {
        assert_eq!(line(&[]), r#""C:\Program Files\yserver\yserver.exe""#);
        assert_eq!(
            line(&[
                ":0",
                "-auth",
                r"C:\Users\jos\AppData\Local\yserver\Xauthority"
            ]),
            r#""C:\Program Files\yserver\yserver.exe" :0 -auth C:\Users\jos\AppData\Local\yserver\Xauthority"#
        );
    }

    #[test]
    fn arguments_with_spaces_or_quotes_are_quoted() {
        assert_eq!(quoted(""), r#""""#);
        assert_eq!(quoted("a b"), r#""a b""#);
        assert_eq!(quoted("tab\there"), "\"tab\there\"");
        assert_eq!(quoted(r#"a"b"#), r#""a\"b""#);
        // Backslashes are literal unless a quote follows them.
        assert_eq!(quoted(r"C:\a b\c"), r#""C:\a b\c""#);
        assert_eq!(quoted(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(quoted(r#"a\\"b"#), r#""a\\\\\"b""#);
    }

    fn quoted(arg: &str) -> String {
        let mut out = String::new();
        quote_into(&mut out, arg);
        out
    }
}

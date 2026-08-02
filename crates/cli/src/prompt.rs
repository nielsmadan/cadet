use std::io::{BufRead, IsTerminal, Write};

pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal()
}

pub fn ask(label: &str, default: Option<&str>) -> std::io::Result<String> {
    let stdin = std::io::stdin();
    let mut r = stdin.lock();
    let mut w = std::io::stderr();
    ask_with(&mut r, &mut w, label, default)
}

// Prompts go to stderr: stdout is the command's output, and a prompt in a
// pipe would corrupt it.
pub fn ask_with<R: BufRead, W: Write>(
    r: &mut R,
    w: &mut W,
    label: &str,
    default: Option<&str>,
) -> std::io::Result<String> {
    match default {
        Some(d) => write!(w, "  {label}  [{d}] › ")?,
        None => write!(w, "  {label} › ")?,
    }
    w.flush()?;
    let mut line = String::new();
    r.read_line(&mut line)?;
    let typed = line.trim();
    Ok(if typed.is_empty() {
        default.unwrap_or("").to_string()
    } else {
        typed.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn run(input: &str, label: &str, default: Option<&str>) -> (String, String) {
        let mut r = Cursor::new(input.as_bytes().to_vec());
        let mut w: Vec<u8> = vec![];
        let got = ask_with(&mut r, &mut w, label, default).unwrap();
        (got, String::from_utf8(w).unwrap())
    }

    #[test]
    fn empty_input_takes_the_default() {
        let (got, _) = run("\n", "path", Some("/tmp/x"));
        assert_eq!(got, "/tmp/x");
    }

    #[test]
    fn typed_input_wins_over_the_default() {
        let (got, _) = run("/other\n", "path", Some("/tmp/x"));
        assert_eq!(got, "/other");
    }

    #[test]
    fn input_is_trimmed() {
        let (got, _) = run("  /spaced  \n", "path", None);
        assert_eq!(got, "/spaced");
    }

    #[test]
    fn the_default_is_shown_in_the_label() {
        let (_, shown) = run("\n", "prefix", Some("JUG"));
        assert!(shown.contains("prefix"), "{shown}");
        assert!(shown.contains("JUG"), "{shown}");
    }

    #[test]
    fn no_default_and_no_input_yields_an_empty_string() {
        let (got, _) = run("\n", "path", None);
        assert_eq!(got, "");
    }

    #[test]
    fn eof_with_a_default_takes_the_default() {
        let (got, _) = run("", "path", Some("/tmp/x"));
        assert_eq!(got, "/tmp/x");
    }
}

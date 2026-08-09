use std::io::{BufRead, IsTerminal, Write};

pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptInput {
    DefaultAccepted,
    Value(String),
    Eof,
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

pub fn ask_cancelable_with<R: BufRead, W: Write>(
    r: &mut R,
    w: &mut W,
    label: &str,
    default: Option<&str>,
) -> std::io::Result<PromptInput> {
    match default {
        Some(d) => write!(w, "  {label}  [{d}] › ")?,
        None => write!(w, "  {label} › ")?,
    }
    w.flush()?;
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(PromptInput::Eof);
    }
    let typed = line.trim();
    Ok(if typed.is_empty() {
        PromptInput::DefaultAccepted
    } else {
        PromptInput::Value(typed.to_string())
    })
}

pub fn confirm(question: &str) -> std::io::Result<bool> {
    let stdin = std::io::stdin();
    let mut r = stdin.lock();
    let mut w = std::io::stderr();
    confirm_with(&mut r, &mut w, question)
}

/// Anything but an explicit yes is a no, EOF included — this gate exists to
/// stop an irreversible-feeling action, so silence must not consent.
pub fn confirm_with<R: BufRead, W: Write>(
    r: &mut R,
    w: &mut W,
    question: &str,
) -> std::io::Result<bool> {
    write!(w, "  {question} [y/N] ")?;
    w.flush()?;
    let mut line = String::new();
    r.read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn confirmed(input: &str) -> bool {
        let mut r = Cursor::new(input.as_bytes().to_vec());
        let mut w: Vec<u8> = vec![];
        confirm_with(&mut r, &mut w, "go ahead?").unwrap()
    }

    #[test]
    fn confirm_accepts_y_and_yes() {
        assert!(confirmed("y\n"));
        assert!(confirmed("YES\n"));
    }

    #[test]
    fn confirm_defaults_to_no() {
        assert!(!confirmed("\n"));
        assert!(!confirmed(""));
        assert!(!confirmed("maybe\n"));
    }

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

    #[test]
    fn cancelable_prompt_distinguishes_enter_value_and_eof() {
        let mut output = Vec::new();
        assert_eq!(
            ask_cancelable_with(
                &mut Cursor::new("\n".as_bytes()),
                &mut output,
                "title",
                Some("default")
            )
            .unwrap(),
            PromptInput::DefaultAccepted
        );
        assert_eq!(
            ask_cancelable_with(
                &mut Cursor::new("typed\n".as_bytes()),
                &mut output,
                "title",
                Some("default")
            )
            .unwrap(),
            PromptInput::Value("typed".into())
        );
        assert_eq!(
            ask_cancelable_with(
                &mut Cursor::new(Vec::<u8>::new()),
                &mut output,
                "title",
                Some("default")
            )
            .unwrap(),
            PromptInput::Eof
        );
    }
}

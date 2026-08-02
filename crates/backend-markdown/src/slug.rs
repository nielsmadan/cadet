/// Title → filename stem. ASCII-only and lowercased so macOS and Linux agree.
///
/// A non-ASCII letter or digit (e.g. an accented character) is dropped
/// silently rather than turned into a separator — `"déjà"` becomes `"dj"`,
/// not `"d-j"`. Anything else non-alphanumeric (whitespace, ASCII
/// punctuation, and non-ASCII punctuation such as an em dash) is treated
/// as a word separator and collapsed to a single hyphen.
pub fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if ch.is_alphanumeric() {
            continue;
        } else {
            if last_dash {
                continue;
            }
            out.push('-');
            last_dash = true;
        }
        if out.len() >= 40 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_and_hyphenates() {
        assert_eq!(slugify("Buy Oat Milk"), "buy-oat-milk");
    }

    #[test]
    fn strips_non_ascii_and_punctuation() {
        assert_eq!(slugify("Café — déjà vu!"), "caf-dj-vu");
    }

    #[test]
    fn collapses_and_trims_separators() {
        assert_eq!(slugify("  a   b  "), "a-b");
    }

    #[test]
    fn truncates_to_forty_characters() {
        assert_eq!(slugify(&"x".repeat(80)).len(), 40);
    }

    #[test]
    fn empty_input_yields_untitled() {
        assert_eq!(slugify("!!!"), "untitled");
    }
}

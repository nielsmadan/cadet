use std::collections::BTreeMap;

/// A frontmatter field's parsed shape.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    /// A flat scalar, e.g. `state: todo` or `tags: [home, errands]`.
    Scalar { raw: String, decoded: String },
    /// A block list, e.g. `tags:` followed by indented `- item` lines.
    List(Vec<String>),
    /// Anything else indented under a key (a nested map, or an
    /// unrecognised block shape). Preserved verbatim, never parsed or
    /// written by Cadet.
    Nested,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frontmatter {
    values: BTreeMap<String, Value>,
    pub body: String,
}

impl Frontmatter {
    /// Returns the scalar text for `key`. `None` if the key is absent, or
    /// if it is a block value (list or nested map) rather than a flat
    /// scalar — callers must use [`Frontmatter::list`] for those.
    pub fn get(&self, key: &str) -> Option<&str> {
        match self.values.get(key)? {
            Value::Scalar { decoded, .. } => Some(decoded.as_str()),
            Value::List(_) | Value::Nested => None,
        }
    }

    /// Returns `key` as a list of strings: the items of a block list, or
    /// the parsed elements of an inline `[a, b]` / comma-separated
    /// scalar. Empty if the key is absent, an empty scalar, or a nested
    /// map.
    ///
    /// A scalar item wrapped in double quotes is decoded as a YAML string.
    /// Splitting itself ignores commas inside a quoted item.
    pub fn list(&self, key: &str) -> Vec<String> {
        match self.values.get(key) {
            Some(Value::List(items)) => items.clone(),
            Some(Value::Scalar { raw, .. }) => {
                let trimmed = raw.trim();
                let inner = trimmed
                    .strip_prefix('[')
                    .and_then(|s| s.strip_suffix(']'))
                    .unwrap_or(trimmed);
                split_respecting_quotes(inner)
                    .iter()
                    .map(|s| unquote_list_item(s))
                    .filter(|s| !s.is_empty())
                    .collect()
            }
            Some(Value::Nested) | None => Vec::new(),
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }
}

/// Splits `s` on commas that are not inside a double-quoted span, so a
/// quoted item's own commas cannot be mistaken for separators.
fn split_respecting_quotes(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            '\\' if in_quotes => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ',' if !in_quotes => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    out.push(current);
    out
}

/// Undoes [`quote_list_item`]: strips a wrapping pair of double quotes and
/// unescapes `\"` back to `"`. An unquoted token is trimmed and, for
/// backward compatibility with hand-written single-quoted items, has any
/// leading/trailing `'` stripped too.
fn unquote_list_item(token: &str) -> String {
    let trimmed = token.trim();
    decode_quoted_string(trimmed).unwrap_or_else(|| trimmed.trim_matches(['"', '\'']).to_string())
}

fn decode_hex(chars: &mut std::str::Chars<'_>, width: usize) -> Option<char> {
    let digits: String = chars.by_ref().take(width).collect();
    if digits.len() != width {
        return None;
    }
    char::from_u32(u32::from_str_radix(&digits, 16).ok()?)
}

fn decode_quoted_string(raw: &str) -> Option<String> {
    if raw.len() < 2 {
        return None;
    }
    if let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            out.push(match chars.next()? {
                '0' => '\0',
                'a' => '\u{7}',
                'b' => '\u{8}',
                't' => '\t',
                'n' => '\n',
                'v' => '\u{b}',
                'f' => '\u{c}',
                'r' => '\r',
                'e' => '\u{1b}',
                ' ' => ' ',
                '"' => '"',
                '/' => '/',
                '\\' => '\\',
                'N' => '\u{85}',
                '_' => '\u{a0}',
                'L' => '\u{2028}',
                'P' => '\u{2029}',
                'x' => decode_hex(&mut chars, 2)?,
                'u' => decode_hex(&mut chars, 4)?,
                'U' => decode_hex(&mut chars, 8)?,
                _ => return None,
            });
        }
        return Some(out);
    }

    let inner = raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\''))?;
    let mut out = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            out.push(c);
        } else if chars.next_if_eq(&'\'').is_some() {
            out.push('\'');
        } else {
            return None;
        }
    }
    Some(out)
}

pub fn render_string(value: &str) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => write!(&mut out, "\\u{:04X}", c as u32).unwrap(),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn render_list(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|i| render_string(i))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Splits `s` into `(content, terminator)` pairs, where `terminator` is
/// `"\r\n"`, `"\n"`, or `""` for a final line with no trailing newline.
/// Unlike `str::lines`, the terminator is preserved so untouched lines can
/// be re-emitted byte-identical regardless of the document's line ending.
pub(crate) fn split_lines(s: &str) -> Vec<(&str, &str)> {
    let mut result = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(idx) = rest.find('\n') {
            let raw = &rest[..idx];
            if let Some(content) = raw.strip_suffix('\r') {
                result.push((content, "\r\n"));
            } else {
                result.push((raw, "\n"));
            }
            rest = &rest[idx + 1..];
        } else {
            result.push((rest, ""));
            rest = "";
        }
    }
    result
}

fn join_lines(lines: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (c, t) in lines {
        out.push_str(c);
        out.push_str(t);
    }
    out
}

/// Locates the opening and closing fence lines by content, independent of
/// line-ending style. Returns `(open_idx, close_idx)`.
pub(crate) fn find_fences(lines: &[(&str, &str)]) -> Option<(usize, usize)> {
    if lines.is_empty() || lines[0].0 != "---" {
        return None;
    }
    (1..lines.len())
        .find(|&i| lines[i].0 == "---")
        .map(|i| (0, i))
}

fn split_line(line: &str) -> Option<(&str, &str)> {
    if line.trim_start().starts_with('#') || line.trim().is_empty() {
        return None;
    }
    if line.starts_with(char::is_whitespace) {
        return None; // nested / continuation — preserved, never edited
    }
    let (k, v) = line.split_once(':')?;
    if k.trim().is_empty() {
        return None;
    }
    Some((k.trim(), v.trim()))
}

/// A keyed frontmatter entry's line span within the frontmatter block:
/// the header line at `header_idx`, plus any indented continuation lines,
/// running up to (but excluding) `end_idx`.
struct Entry {
    key: String,
    header_value: String,
    header_idx: usize,
    end_idx: usize,
}

/// Walks the frontmatter block and groups each header line with the
/// indented continuation lines that follow it, so a block list or nested
/// map is never split from its header.
fn parse_entries<S: AsRef<str>>(block: &[(S, S)]) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut i = 0;
    while i < block.len() {
        let content = block[i].0.as_ref();
        if let Some((k, v)) = split_line(content) {
            let mut end = i + 1;
            while end < block.len() && block[end].0.as_ref().starts_with(char::is_whitespace) {
                end += 1;
            }
            entries.push(Entry {
                key: k.to_string(),
                header_value: v.to_string(),
                header_idx: i,
                end_idx: end,
            });
            i = end;
        } else {
            i += 1;
        }
    }
    entries
}

/// Classifies a header's value: a non-empty header value is always a
/// scalar; an empty one is a block list if every continuation line is a
/// `- item` entry, a nested/unrecognised block if there are continuation
/// lines that aren't, or an empty scalar if there are none.
fn compute_value(header_value: &str, continuation: &[(&str, &str)]) -> Value {
    if !header_value.is_empty() {
        return Value::Scalar {
            raw: header_value.to_string(),
            decoded: decode_quoted_string(header_value).unwrap_or_else(|| header_value.to_string()),
        };
    }
    if continuation.is_empty() {
        return Value::Scalar {
            raw: String::new(),
            decoded: String::new(),
        };
    }
    let all_list_items = continuation
        .iter()
        .all(|(c, _)| c.trim_start().starts_with("- "));
    if all_list_items {
        let items = continuation
            .iter()
            .map(|(c, _)| c.trim_start().trim_start_matches("- ").trim().to_string())
            .collect();
        Value::List(items)
    } else {
        Value::Nested
    }
}

pub fn parse_frontmatter(src: &str) -> Option<Frontmatter> {
    let all_lines = split_lines(src);
    let (open_idx, close_idx) = find_fences(&all_lines)?;
    let block = &all_lines[open_idx + 1..close_idx];
    let entries = parse_entries(block);
    let mut values = BTreeMap::new();
    for e in &entries {
        let continuation = &block[e.header_idx + 1..e.end_idx];
        let value = compute_value(&e.header_value, continuation);
        values.insert(e.key.clone(), value);
    }
    let body = join_lines(&all_lines[close_idx + 1..]);
    Some(Frontmatter { values, body })
}

pub fn replace_body(src: &str, body: &str) -> Option<String> {
    let all_lines = split_lines(src);
    let (_, close_idx) = find_fences(&all_lines)?;
    let mut out = join_lines(&all_lines[..=close_idx]);
    out.push_str(body);
    Some(out)
}

/// Replace, insert or remove frontmatter fields, touching no other byte.
/// `None` removes the field. Spec §4.
///
/// Line-ending agnostic: `---\r\n` and `---\n` are both recognised as
/// fences, every existing line (including a block list or nested map's
/// continuation lines) keeps its original terminator, and a newly
/// inserted line — or a brand-new frontmatter header for a document that
/// had none — uses the terminator already found in the document,
/// defaulting to `\n` when there isn't one.
///
/// Splicing a key that currently spans a block (a block list, or a
/// nested map that Cadet does not itself write) replaces the header line
/// and consumes its continuation lines, so nothing is left orphaned.
pub fn splice(src: &str, edits: &[(&str, Option<String>)]) -> String {
    let all_lines = split_lines(src);
    let Some((open_idx, close_idx)) = find_fences(&all_lines) else {
        let eol = "\n";
        let mut header = format!("---{eol}");
        for (k, v) in edits {
            if let Some(v) = v {
                header.push_str(&format!("{k}: {v}{eol}"));
            }
        }
        header.push_str(&format!("---{eol}"));
        header.push_str(src);
        return header;
    };

    let eol = match all_lines[open_idx].1 {
        "" => "\n",
        t => t,
    };

    let mut block: Vec<(String, String)> = all_lines[open_idx + 1..close_idx]
        .iter()
        .map(|(c, t)| (c.to_string(), t.to_string()))
        .collect();

    for (key, value) in edits {
        let entries = parse_entries(&block);
        let found = entries.into_iter().find(|e| e.key == *key);
        match (found, value) {
            (Some(e), Some(v)) => {
                let term = block[e.header_idx].1.clone();
                block.splice(
                    e.header_idx..e.end_idx,
                    std::iter::once((format!("{key}: {v}"), term)),
                );
            }
            (Some(e), None) => {
                block.splice(e.header_idx..e.end_idx, std::iter::empty());
            }
            (None, Some(v)) => {
                block.push((format!("{key}: {v}"), eol.to_string()));
            }
            (None, None) => {}
        }
    }

    let mut out = String::with_capacity(src.len() + 64);
    for (c, t) in &all_lines[..=open_idx] {
        out.push_str(c);
        out.push_str(t);
    }
    for (c, t) in &block {
        out.push_str(c);
        out.push_str(t);
    }
    for (c, t) in &all_lines[close_idx..] {
        out.push_str(c);
        out.push_str(t);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\n\
title: Buy milk\n\
# a comment we must not lose\n\
state: todo\n\
tags: [home, errands]\n\
custom:   spaced out value\n\
---\n\
\n\
Body text here.\n";

    #[test]
    fn parses_flat_scalars_and_inline_arrays() {
        let fm = parse_frontmatter(DOC).unwrap();
        assert_eq!(fm.get("title").unwrap(), "Buy milk");
        assert_eq!(fm.get("state").unwrap(), "todo");
        assert_eq!(fm.get("custom").unwrap(), "spaced out value");
        assert_eq!(
            fm.list("tags"),
            vec!["home".to_string(), "errands".to_string()]
        );
        assert_eq!(fm.body, "\nBody text here.\n");
    }

    #[test]
    fn yaml_quoted_strings_round_trip() {
        let title = r#"bug: press "global" at C:\hotkeys"#;
        let rendered = render_string(title);
        assert_eq!(rendered, r#""bug: press \"global\" at C:\\hotkeys""#);
        let doc = format!("---\ntitle: {rendered}\nowner: 'Niels''s desk'\n---\n");
        let fm = parse_frontmatter(&doc).unwrap();
        assert_eq!(fm.get("title"), Some(title));
        assert_eq!(fm.get("owner"), Some("Niels's desk"));
    }

    #[test]
    fn yaml_list_strings_are_always_quoted() {
        let items = vec![
            "plain".to_string(),
            "bug: urgent".to_string(),
            "[nested]".to_string(),
            "#hash".to_string(),
            "true".to_string(),
            "line\nbreak".to_string(),
        ];
        let rendered = render_list(&items);
        assert_eq!(
            rendered,
            r##"["plain", "bug: urgent", "[nested]", "#hash", "true", "line\nbreak"]"##
        );
        let fm = parse_frontmatter(&format!("---\ntags: {rendered}\n---\n")).unwrap();
        assert_eq!(fm.list("tags"), items);
    }

    #[test]
    fn returns_none_without_frontmatter() {
        assert!(parse_frontmatter("just a note\n").is_none());
    }

    /// Spec §9.5, the golden-file round trip: *change one field, assert only
    /// that field's bytes differ*. Byte-for-byte against the whole expected
    /// document — comparing `.lines()` with the changed key filtered out of
    /// both sides cannot see a line ending change, a lost trailing newline,
    /// or a second `state:` line appearing out of nowhere.
    #[test]
    fn splicing_one_field_changes_only_that_field_byte_for_byte() {
        let out = splice(DOC, &[("state", Some("doing".into()))]);
        let expected = DOC.replace("state: todo\n", "state: doing\n");
        assert_eq!(out, expected, "only the `state:` bytes may differ");
        // Guards the expectation itself: `expected` must really be one byte
        // sequence away from `DOC`, not accidentally identical to it.
        assert_ne!(out, DOC);
        assert_eq!(
            out.matches("state:").count(),
            1,
            "splicing must replace the field, not add a second one"
        );
    }

    #[test]
    fn replacing_the_body_preserves_the_frontmatter_bytes() {
        let out = replace_body(DOC, "\nNew body.\n").unwrap();
        let frontmatter_end = DOC.find("\n---\n").unwrap() + "\n---\n".len();
        assert_eq!(&out[..frontmatter_end], &DOC[..frontmatter_end]);
        assert_eq!(&out[frontmatter_end..], "\nNew body.\n");
    }

    /// The same rule for a CRLF document: the splice must not normalise the
    /// line endings of the lines it did not touch.
    #[test]
    fn splicing_preserves_crlf_byte_for_byte() {
        let doc = DOC.replace('\n', "\r\n");
        let out = splice(&doc, &[("state", Some("doing".into()))]);
        assert_eq!(out, doc.replace("state: todo\r\n", "state: doing\r\n"));
    }

    /// A document with no trailing newline must not gain one.
    #[test]
    fn splicing_does_not_invent_a_trailing_newline() {
        let doc = DOC.trim_end_matches('\n');
        let out = splice(doc, &[("state", Some("doing".into()))]);
        assert_eq!(out, doc.replace("state: todo\n", "state: doing\n"));
    }

    #[test]
    fn splicing_an_absent_field_appends_before_the_closing_fence() {
        let out = splice(DOC, &[("uid", Some("01ARZ3".into()))]);
        let fm_end = out.find("\n---\n").unwrap();
        assert!(out[..fm_end].contains("uid: 01ARZ3"));
        assert!(out.contains("Body text here."));
    }

    #[test]
    fn splicing_none_removes_the_field() {
        let out = splice(DOC, &[("custom", None)]);
        assert!(!out.contains("custom:"));
        assert!(out.contains("state: todo"));
    }

    #[test]
    fn splicing_is_idempotent() {
        let once = splice(DOC, &[("state", Some("doing".into()))]);
        let twice = splice(&once, &[("state", Some("doing".into()))]);
        assert_eq!(once, twice);
    }

    #[test]
    fn a_document_without_frontmatter_gains_one() {
        let out = splice("Body only.\n", &[("state", Some("todo".into()))]);
        assert!(out.starts_with("---\nstate: todo\n---\n"));
        assert!(out.ends_with("Body only.\n"));
    }

    const CRLF_DOC: &str =
        "---\r\ntitle: Buy milk\r\nstate: todo\r\n---\r\n\r\nBody text here.\r\n";

    #[test]
    fn crlf_document_parses() {
        let fm = parse_frontmatter(CRLF_DOC).unwrap();
        assert_eq!(fm.get("title").unwrap(), "Buy milk");
        assert_eq!(fm.get("state").unwrap(), "todo");
        assert!(!fm.get("title").unwrap().contains('\r'));
        assert!(!fm.get("state").unwrap().contains('\r'));
        assert_eq!(fm.body, "\r\nBody text here.\r\n");
    }

    #[test]
    fn splicing_a_crlf_document_preserves_crlf_and_does_not_duplicate_the_fence() {
        let out = splice(CRLF_DOC, &[("state", Some("doing".into()))]);
        assert_eq!(
            out.lines().filter(|l| *l == "---").count(),
            2,
            "must not gain a second frontmatter block"
        );
        assert!(
            out.contains("title: Buy milk\r\n"),
            "untouched line must keep its CRLF"
        );
        assert!(
            out.contains("state: doing\r\n"),
            "spliced line must use CRLF too"
        );
    }

    const BLOCK_LIST_DOC: &str = "---\ntitle: Buy milk\nstate: todo\ntags:\n  - home\n  - errands\npriority: high\n---\n\nBody text here.\n";

    #[test]
    fn block_list_values_are_read() {
        let fm = parse_frontmatter(BLOCK_LIST_DOC).unwrap();
        assert_eq!(
            fm.list("tags"),
            vec!["home".to_string(), "errands".to_string()]
        );
    }

    #[test]
    fn splicing_a_block_list_consumes_its_continuation_lines() {
        let out = splice(
            BLOCK_LIST_DOC,
            &[("tags", Some("[home, errands, urgent]".into()))],
        );
        assert!(!out.contains("- home"));
        assert!(!out.contains("- errands"));
        assert!(out.contains("tags: [home, errands, urgent]"));
        let before: Vec<&str> = BLOCK_LIST_DOC
            .lines()
            .filter(|l| !l.starts_with("tags:") && !l.trim_start().starts_with("- "))
            .collect();
        let after: Vec<&str> = out
            .lines()
            .filter(|l| !l.starts_with("tags:") && !l.trim_start().starts_with("- "))
            .collect();
        assert_eq!(before, after, "no field other than tags may change");
    }

    const NESTED_MAP_DOC: &str = "---\ntitle: Buy milk\nmeta:\n  owner: alice\n  weight: 3\nstate: todo\n---\n\nBody text here.\n";

    #[test]
    fn splicing_a_field_beside_a_nested_map_leaves_the_map_intact() {
        let out = splice(NESTED_MAP_DOC, &[("state", Some("doing".into()))]);
        assert!(out.contains("state: doing"));
        assert!(out.contains("meta:\n  owner: alice\n  weight: 3\n"));
    }
}

use crate::frontmatter::{find_fences, parse_frontmatter, split_lines};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const PROBE_BYTES: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Task,
    NotATask,
    /// Cloud placeholder — content is not on disk. Never treated as absence (§8).
    NotMaterialised,
}

/// Checks whether a file is a task without reading it whole, and without
/// triggering a cloud download. Metadata is inspected before `open()`.
///
/// Reuses `frontmatter`'s terminator-preserving line split and fence
/// finder rather than a second, hand-rolled parser, so a CRLF document
/// (`---\r\n`) is recognised here exactly as it is by `parse_frontmatter` —
/// a `starts_with("---\n")` check would silently misclassify every CRLF
/// task file as a note.
///
/// Only the first `PROBE_BYTES` are read for the common case (a note with
/// no frontmatter at all — the overwhelming majority of files in a large
/// vault), so that case still costs exactly one small read. But an opening
/// fence with no closing fence inside that window does not necessarily
/// mean "not a task" — it can just mean the frontmatter block itself
/// (e.g. a wide custom-field schema) is longer than the window. Treating
/// that as `NotATask` would silently drop a real task from every scan, so
/// once an opening fence is seen, this falls back to a full read and the
/// real parser rather than guessing from a truncated buffer. That fallback
/// is gated strictly on "opening fence found": widening it to "search the
/// whole truncated window for `state:`" would misclassify a note whose
/// frontmatter is followed by prose that happens to contain a
/// `state:`-shaped line.
pub fn probe(path: &Path) -> io::Result<Probe> {
    if is_placeholder(path)? {
        return Ok(Probe::NotMaterialised);
    }
    let mut buf = vec![0u8; PROBE_BYTES];
    let mut f = File::open(path)?;
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    let head = String::from_utf8_lossy(&buf);
    let lines = split_lines(&head);
    if lines.is_empty() || lines[0].0 != "---" {
        return Ok(Probe::NotATask);
    }
    if let Some((open_idx, close_idx)) = find_fences(&lines) {
        let has_state = lines[open_idx + 1..close_idx].iter().any(|(content, _)| {
            content
                .split_once(':')
                .is_some_and(|(k, _)| k.trim() == "state")
        });
        return Ok(if has_state {
            Probe::Task
        } else {
            Probe::NotATask
        });
    }
    let full = std::fs::read_to_string(path)?;
    let has_state = parse_frontmatter(&full).is_some_and(|fm| fm.get("state").is_some());
    Ok(if has_state {
        Probe::Task
    } else {
        Probe::NotATask
    })
}

#[cfg(target_os = "macos")]
fn is_placeholder(path: &Path) -> io::Result<bool> {
    use std::os::macos::fs::MetadataExt;
    const SF_DATALESS: u32 = 0x4000_0000;
    // A `.name.icloud` sidecar marks an evicted iCloud file.
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
        let sidecar = dir.join(format!(".{}.icloud", name.to_string_lossy()));
        if sidecar.exists() {
            return Ok(true);
        }
    }
    Ok(std::fs::metadata(path)?.st_flags() & SF_DATALESS != 0)
}

// Placeholder/cloud-stub detection is macOS-only by design for M1: iCloud
// Drive eviction is the only cloud-placeholder mechanism this milestone
// targets. Windows' `FILE_ATTRIBUTE_OFFLINE` (OneDrive Files On-Demand,
// etc.) is deliberately deferred — tracked in the plan's deferred-gaps
// list, not handled here. Linux has no equivalent OS-level placeholder
// attribute to check.
#[cfg(not(target_os = "macos"))]
fn is_placeholder(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn detects_a_task_by_state_key() {
        let d = tempfile::tempdir().unwrap();
        let p = write(&d, "t.md", "---\nstate: todo\ntitle: x\n---\nbody\n");
        assert_eq!(probe(&p).unwrap(), Probe::Task);
    }

    #[test]
    fn a_note_with_frontmatter_but_no_state_is_not_a_task() {
        let d = tempfile::tempdir().unwrap();
        let p = write(&d, "n.md", "---\ntitle: just a note\n---\nbody\n");
        assert_eq!(probe(&p).unwrap(), Probe::NotATask);
    }

    #[test]
    fn a_plain_note_is_not_a_task() {
        let d = tempfile::tempdir().unwrap();
        let p = write(&d, "n.md", "# heading\n\ntext\n");
        assert_eq!(probe(&p).unwrap(), Probe::NotATask);
    }

    #[test]
    fn only_the_first_2kb_is_read() {
        let d = tempfile::tempdir().unwrap();
        let mut body = String::from("---\nstate: todo\n---\n");
        body.push_str(&"x".repeat(500_000));
        let p = write(&d, "big.md", &body);
        assert_eq!(probe(&p).unwrap(), Probe::Task);
    }

    /// Task 7 fixed the exact same bug in `frontmatter.rs`: a CRLF document
    /// opens with `---\r\n`, not `---\n`. `probe` must reuse that
    /// line-handling rather than a hand-rolled `starts_with("---\n")`
    /// check, or every CRLF task file silently vanishes from the scan.
    #[test]
    fn a_crlf_task_file_is_detected_as_a_task() {
        let d = tempfile::tempdir().unwrap();
        let p = write(
            &d,
            "t.md",
            "---\r\nstate: todo\r\ntitle: x\r\n---\r\nbody\r\n",
        );
        assert_eq!(probe(&p).unwrap(), Probe::Task);
    }

    /// Regression guard: `state:` sits well inside the 2 KB probe window,
    /// but enough padding fields follow it to push the closing `---` past
    /// the window. The opening-fence fallback must still find it.
    #[test]
    fn a_task_with_frontmatter_larger_than_the_probe_window_is_still_detected() {
        let d = tempfile::tempdir().unwrap();
        let mut body = String::from("---\nstate: todo\n");
        for i in 0..300 {
            body.push_str(&format!("field{i}: some padding value here\n"));
        }
        assert!(
            body.len() > PROBE_BYTES,
            "test setup must exceed the window"
        );
        body.push_str("---\nbody\n");
        let p = write(&d, "big.md", &body);
        assert_eq!(probe(&p).unwrap(), Probe::Task);
    }

    /// Pins that the opening-fence fallback does not fire for an ordinary
    /// note with no frontmatter at all, however long the note is.
    #[test]
    fn a_long_note_without_frontmatter_is_not_a_task() {
        let d = tempfile::tempdir().unwrap();
        let mut body = String::from("# heading\n\n");
        body.push_str(&"just a plain note line\n".repeat(200));
        assert!(
            body.len() > PROBE_BYTES,
            "test setup must exceed the window"
        );
        let p = write(&d, "n.md", &body);
        assert_eq!(probe(&p).unwrap(), Probe::NotATask);
    }

    #[test]
    fn a_zero_byte_file_is_not_a_task() {
        let d = tempfile::tempdir().unwrap();
        let p = write(&d, "empty.md", "");
        assert_eq!(probe(&p).unwrap(), Probe::NotATask);
    }

    #[test]
    fn a_file_that_is_only_a_fence_is_not_a_task() {
        let d = tempfile::tempdir().unwrap();
        let p = write(&d, "fence.md", "---");
        assert_eq!(probe(&p).unwrap(), Probe::NotATask);
    }

    #[test]
    fn invalid_utf8_is_not_a_task_and_does_not_panic() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("bad.md");
        std::fs::write(&p, [0xff, 0xfe, 0x00, 0xff]).unwrap();
        assert_eq!(probe(&p).unwrap(), Probe::NotATask);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_placeholder_sidecar_wins_over_valid_task_content() {
        let d = tempfile::tempdir().unwrap();
        let p = write(&d, "t.md", "---\nstate: todo\ntitle: x\n---\nbody\n");
        write(&d, ".t.md.icloud", "");
        assert_eq!(probe(&p).unwrap(), Probe::NotMaterialised);
    }
}

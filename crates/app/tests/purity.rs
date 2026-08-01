//! `core` must contain no I/O. This is a structural invariant, not a style
//! rule: the whole architecture rests on `core` being trivially testable and
//! portable to WASM and Swift.
//!
//! Two guards, because either one alone is escapable. The dependency guard is
//! the real guarantee — `core` cannot do file, network or database I/O it has
//! no crate for — and the source guard catches the rest of `std`, which is
//! always available and needs no declaration.

use std::path::{Path, PathBuf};

fn core_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../core")
}

/// Every crate `core` is allowed to depend on, and why it is pure:
/// `thiserror` is derive-only, `ulid`/`jiff`/`blake3` are computation, and
/// `toml_edit` parses a string that a caller read. Anything else — a
/// filesystem, HTTP, database or platform-dirs crate — has to be argued for
/// here first, which is the point.
const ALLOWED_DEPENDENCIES: &[&str] = &["thiserror", "ulid", "jiff", "blake3", "toml_edit"];

#[test]
fn core_depends_on_nothing_that_can_perform_io() {
    let manifest = std::fs::read_to_string(core_dir().join("Cargo.toml")).unwrap();
    let mut in_deps = false;
    let mut offenders = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // `[dev-dependencies]` is deliberately not covered: test-only
            // crates are not part of what ships.
            in_deps = trimmed == "[dependencies]";
            continue;
        }
        if !in_deps || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let name = trimmed
            .split(['=', '.'])
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        if !name.is_empty() && !ALLOWED_DEPENDENCIES.contains(&name.as_str()) {
            offenders.push(name);
        }
    }
    assert!(
        offenders.is_empty(),
        "core gained a dependency not on the pure allow-list: {offenders:?}\n\
         If it genuinely cannot perform I/O, add it to ALLOWED_DEPENDENCIES with a reason."
    );
}

/// Every `.rs` file under `crates/core/src`, at any depth. `read_dir` is not
/// recursive, so a guard that used it saw nothing inside a submodule
/// directory.
fn core_sources() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![core_dir().join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    assert!(!out.is_empty(), "found no core sources — check the path");
    out
}

/// `std` needs no dependency declaration, so the allow-list above cannot see
/// it. Matching is on the module segment rather than the literal `std::fs`:
/// `use std::{fs, net::TcpStream, process::Command};` never contains that
/// string, and a guard that looked for it passed while `core` read files,
/// opened sockets and spawned subprocesses.
const BANNED_STD_MODULES: &[&str] = &["fs", "net", "process", "io", "thread", "env"];

#[test]
fn core_source_never_reaches_for_std_io() {
    let mut offenders = Vec::new();
    for path in core_sources() {
        let src = std::fs::read_to_string(&path).unwrap();
        for module in BANNED_STD_MODULES {
            if mentions_std_module(&src, module) {
                offenders.push(format!("{}: std::{module}", path.display()));
            }
        }
        for banned in [
            "rusqlite",
            "Timestamp::now",
            "SystemTime::now",
            "Instant::now",
        ] {
            if src.contains(banned) {
                offenders.push(format!("{}: {banned}", path.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "core must have no I/O and no clock reads:\n{}",
        offenders.join("\n")
    );
}

/// True when `src` refers to `std::<module>` in any spelling a `use` can take:
/// `std::fs::read`, `use std::{fs, io}`, `use std::fs as f`.
fn mentions_std_module(src: &str, module: &str) -> bool {
    for (idx, _) in src.match_indices("std::") {
        let rest = &src[idx + "std::".len()..];
        if segment_names(rest.trim_start()).any(|s| s == module) {
            return true;
        }
    }
    false
}

/// The module names introduced immediately after a `std::`: either the single
/// following path segment, or every top-level segment of a `{a, b::c}` group.
fn segment_names(rest: &str) -> Box<dyn Iterator<Item = String> + '_> {
    if let Some(group) = rest.strip_prefix('{') {
        let end = group.find('}').unwrap_or(group.len());
        return Box::new(
            group[..end]
                .split(',')
                .map(|part| first_segment(part.trim()).to_string()),
        );
    }
    Box::new(std::iter::once(first_segment(rest).to_string()))
}

fn first_segment(s: &str) -> &str {
    s.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .unwrap_or_default()
}

/// The one impurity `core` still has, kept honest rather than hidden.
/// `TaskUid::generate` reads the system clock and an RNG through `ulid`.
/// Threading uid generation in from `app` is the real fix and is out of this
/// wave's scope; until then the exemption is confined to `model.rs`, so no
/// other module can quietly acquire a clock or a source of randomness.
#[test]
fn ulid_generation_stays_confined_to_the_one_documented_exemption() {
    let mut offenders = Vec::new();
    for path in core_sources() {
        let is_model = path.file_name().is_some_and(|n| n == "model.rs");
        let src = std::fs::read_to_string(&path).unwrap();
        // `#[cfg(test)]` fixtures call `TaskUid::generate`; only the
        // non-test call into `ulid` itself is the impurity.
        if !is_model && src.contains("ulid::") {
            offenders.push(path.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "ulid generation is impure and is exempted only in core/src/model.rs; found it in:\n{}",
        offenders.join("\n")
    );
}

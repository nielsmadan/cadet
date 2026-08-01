/// `core` must contain no I/O. This is a structural invariant, not a style rule:
/// the whole architecture rests on `core` being trivially testable and portable
/// to WASM and Swift.
#[test]
fn core_performs_no_io() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/src");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        for banned in [
            "std::fs",
            "std::net",
            "std::process",
            "rusqlite",
            "Timestamp::now",
        ] {
            if src.contains(banned) {
                offenders.push(format!("{}: {banned}", path.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "core must have no I/O:\n{}",
        offenders.join("\n")
    );
}

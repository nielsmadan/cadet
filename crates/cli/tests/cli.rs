use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;

fn cadet(home: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("cadet").unwrap();
    c.env("CADET_HOME", home);
    c
}

struct Env {
    _home: tempfile::TempDir,
    _vault: tempfile::TempDir,
    home: std::path::PathBuf,
    vault: std::path::PathBuf,
}

fn env() -> Env {
    let home = tempfile::tempdir().unwrap();
    let vault = tempfile::tempdir().unwrap();
    let e = Env {
        home: home.path().to_path_buf(),
        vault: vault.path().to_path_buf(),
        _home: home,
        _vault: vault,
    };
    cadet(&e.home)
        .args([
            "init",
            e.vault.to_str().unwrap(),
            "--prefix",
            "PERS",
            "--name",
            "Personal",
        ])
        .assert()
        .success();
    e
}

#[test]
fn init_creates_project_toml_and_nothing_else() {
    let e = env();
    assert!(e.vault.join("project.toml").exists());
    assert!(!e.vault.join(".git").exists(), "the vault must stay clean");
    assert!(!e.vault.join(".cadet").exists());
}

#[test]
fn add_then_ls_shows_the_task() {
    let e = env();
    cadet(&e.home)
        .args(["add", "Buy oat milk"])
        .assert()
        .success();
    cadet(&e.home)
        .arg("ls")
        .assert()
        .success()
        .stdout(predicates::str::contains("Buy oat milk"));
}

#[test]
fn add_writes_a_readable_markdown_file() {
    let e = env();
    cadet(&e.home)
        .args(["add", "Buy oat milk"])
        .assert()
        .success();
    let raw = std::fs::read_to_string(e.vault.join("buy-oat-milk.md")).unwrap();
    assert!(raw.starts_with("---\n"));
    assert!(raw.contains("key: PERS-1"));
    assert!(raw.contains("title: Buy oat milk"));
    assert!(raw.contains("state: todo"));
}

#[test]
fn done_moves_the_task_out_of_the_default_list() {
    let e = env();
    cadet(&e.home).args(["add", "Buy milk"]).assert().success();
    cadet(&e.home).args(["done", "PERS-1"]).assert().success();
    cadet(&e.home)
        .arg("ls")
        .assert()
        .success()
        .stdout(predicates::str::contains("no tasks"));
    cadet(&e.home)
        .args(["ls", "--all"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Buy milk"));
}

#[test]
fn keys_can_be_given_bare_or_as_a_number() {
    let e = env();
    cadet(&e.home).args(["add", "One"]).assert().success();
    cadet(&e.home)
        .args(["show", "1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("One"));
    cadet(&e.home)
        .args(["show", "PERS-1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("One"));
}

#[test]
fn undo_reverts_the_last_change() {
    let e = env();
    cadet(&e.home).args(["add", "Keep me"]).assert().success();
    cadet(&e.home).args(["add", "Mistake"]).assert().success();
    cadet(&e.home).arg("undo").assert().success();
    cadet(&e.home)
        .arg("ls")
        .assert()
        .success()
        .stdout(predicates::str::contains("Keep me"))
        .stdout(predicates::str::contains("Mistake").not());
}

#[test]
fn an_unknown_key_fails_with_a_clear_message() {
    let e = env();
    cadet(&e.home)
        .args(["done", "PERS-99"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("PERS-99"));
}

#[test]
fn re_running_init_does_not_corrupt_the_registry() {
    let e = env();
    cadet(&e.home)
        .args([
            "init",
            e.vault.to_str().unwrap(),
            "--prefix",
            "PERS",
            "--name",
            "Personal",
            "--force",
        ])
        .assert()
        .success();

    // A corrupted registry (two `[projects.personal]` tables) would make
    // every later command fail with "no project configured".
    cadet(&e.home).arg("ls").assert().success();

    let config = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    assert_eq!(
        config.matches("[projects.personal]").count(),
        1,
        "config.toml must have exactly one entry for `personal`: {config}"
    );
}

#[test]
fn init_refuses_to_overwrite_an_existing_project_without_force() {
    let e = env();
    let mut toml = std::fs::read_to_string(e.vault.join("project.toml")).unwrap();
    toml.push_str("\n# hand-added line\n");
    std::fs::write(e.vault.join("project.toml"), &toml).unwrap();

    cadet(&e.home)
        .args([
            "init",
            e.vault.to_str().unwrap(),
            "--prefix",
            "PERS",
            "--name",
            "Personal",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("already exists"));

    let after = std::fs::read_to_string(e.vault.join("project.toml")).unwrap();
    assert!(
        after.contains("hand-added line"),
        "an existing project.toml must survive a re-init without --force"
    );
}

#[test]
fn a_malformed_registry_is_a_hard_error_not_an_empty_one() {
    let e = env();
    std::fs::write(e.home.join("config.toml"), "not [ valid toml").unwrap();
    cadet(&e.home)
        .arg("ls")
        .assert()
        .failure()
        .stderr(predicates::str::contains("config.toml"));
}

#[test]
fn cadet_adopt_adopts_immediately() {
    let e = env();
    std::fs::write(
        e.vault.join("note.md"),
        "---\nstate: todo\ntitle: Hand made\n---\nbody\n",
    )
    .unwrap();
    cadet(&e.home).arg("adopt").assert().success();
    cadet(&e.home)
        .arg("ls")
        .assert()
        .success()
        .stdout(predicates::str::contains("Hand made"));
}

#[test]
fn a_numeric_key_ambiguous_with_a_title_is_rejected_rather_than_guessed() {
    let e = env();
    cadet(&e.home)
        .args(["add", "first task"])
        .assert()
        .success(); // PERS-1
    cadet(&e.home).args(["add", "1"]).assert().success(); // PERS-2, titled "1"
    cadet(&e.home)
        .args(["show", "1"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("ambiguous"));
}

#[test]
fn an_unknown_project_names_itself_in_the_error() {
    let e = env();
    cadet(&e.home)
        .args(["--project", "ghost", "ls"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("ghost"))
        .stderr(predicates::str::contains("personal"));
}

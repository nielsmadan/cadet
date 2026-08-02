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
            "project",
            "add",
            "personal",
            "--path",
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

/// A second harness, used by the `project` command-group tests below. It
/// gives each project its own path via `vault(name)` inside a shared tempdir,
/// rather than the single-project `Env` above.
struct Harness {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
}

fn harness() -> Harness {
    Harness {
        home: tempfile::tempdir().unwrap(),
        root: tempfile::tempdir().unwrap(),
    }
}

impl Harness {
    fn cadet(&self, args: &[&str]) -> Command {
        let mut c = cadet(self.home.path());
        c.args(args);
        c
    }

    fn vault(&self, name: &str) -> String {
        self.root.path().join(name).to_str().unwrap().to_string()
    }
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
            "project",
            "add",
            "personal",
            "--path",
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
            "project",
            "add",
            "personal",
            "--path",
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

#[test]
fn project_add_creates_and_lists() {
    let h = harness();
    h.cadet(&[
        "project",
        "add",
        "juggler",
        "--path",
        &h.vault("juggler"),
        "--prefix",
        "JUG",
        "--name",
        "Juggler",
    ])
    .assert()
    .success();

    h.cadet(&["project"])
        .assert()
        .success()
        .stdout(predicates::str::contains("juggler"));
}

#[test]
fn project_add_derives_prefix_and_name_when_not_given() {
    let h = harness();
    h.cadet(&["project", "add", "juggler", "--path", &h.vault("juggler")])
        .assert()
        .success();
    h.cadet(&["--project", "juggler", "add", "a task"])
        .assert()
        .success()
        .stdout(predicates::str::contains("JUGG-1"));
}

#[test]
fn project_use_switches_the_default() {
    let h = harness();
    h.cadet(&[
        "project",
        "add",
        "one",
        "--path",
        &h.vault("one"),
        "--prefix",
        "ONE",
    ])
    .assert()
    .success();
    h.cadet(&[
        "project",
        "add",
        "two",
        "--path",
        &h.vault("two"),
        "--prefix",
        "TWO",
    ])
    .assert()
    .success();
    h.cadet(&["project", "use", "two"]).assert().success();
    h.cadet(&["add", "goes to two"])
        .assert()
        .success()
        .stdout(predicates::str::contains("TWO-1"));
}

#[test]
fn project_rm_forgets_the_project_but_leaves_the_files() {
    let h = harness();
    let path = h.vault("gone");
    h.cadet(&["project", "add", "gone", "--path", &path, "--prefix", "GON"])
        .assert()
        .success();
    h.cadet(&["--project", "gone", "add", "a task"])
        .assert()
        .success();
    h.cadet(&["project", "rm", "gone"]).assert().success();

    h.cadet(&["project"])
        .assert()
        .success()
        .stdout(predicates::str::contains("gone").not());
    assert!(
        std::path::Path::new(&path).join("project.toml").exists(),
        "rm must not delete the vault"
    );
}

#[test]
fn project_root_is_shown_and_set() {
    let h = harness();
    h.cadet(&["project", "root", &h.vault("notes")])
        .assert()
        .success();
    h.cadet(&["project", "root"])
        .assert()
        .success()
        .stdout(predicates::str::contains("notes"));
}

#[test]
fn project_add_without_a_path_fails_cleanly_when_not_a_tty() {
    let h = harness();
    h.cadet(&["project", "add", "nopath"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--path"));
}

#[test]
fn init_is_gone() {
    let h = harness();
    h.cadet(&["init", "/tmp/whatever", "--prefix", "X", "--name", "X"])
        .assert()
        .failure();
}

#[test]
fn project_add_uses_the_configured_root_when_not_a_tty() {
    let h = harness();
    h.cadet(&["project", "root", &h.vault("root")])
        .assert()
        .success();
    h.cadet(&["project", "add", "juggler"]).assert().success();
    h.cadet(&["project"])
        .assert()
        .success()
        .stdout(predicates::str::contains("juggler"))
        .stdout(predicates::str::contains(
            std::path::Path::new("root")
                .join("juggler")
                .join("tasks")
                .to_str()
                .unwrap()
                .to_string(),
        ));
}

#[test]
fn force_overwrite_without_explicit_prefix_keeps_the_existing_prefix() {
    let h = harness();
    let path = h.vault("alpha");
    h.cadet(&[
        "project", "add", "alpha", "--path", &path, "--prefix", "ALFA",
    ])
    .assert()
    .success();
    h.cadet(&["--project", "alpha", "add", "task one"])
        .assert()
        .success();
    h.cadet(&["project", "rm", "alpha"]).assert().success();
    h.cadet(&["project", "add", "alpha", "--path", &path, "--force"])
        .assert()
        .success();

    let toml = std::fs::read_to_string(std::path::Path::new(&path).join("project.toml")).unwrap();
    assert!(
        toml.contains("prefix = \"ALFA\""),
        "force-overwrite without --prefix must keep the existing prefix: {toml}"
    );

    h.cadet(&["--project", "alpha", "add", "task two"])
        .assert()
        .success()
        .stdout(predicates::str::contains("ALFA-2"));
}

#[test]
fn force_overwrite_with_an_explicit_prefix_still_changes_it() {
    let h = harness();
    let path = h.vault("alpha");
    h.cadet(&[
        "project", "add", "alpha", "--path", &path, "--prefix", "ALFA",
    ])
    .assert()
    .success();
    h.cadet(&[
        "project", "add", "alpha", "--path", &path, "--prefix", "NEWP", "--force",
    ])
    .assert()
    .success();

    let toml = std::fs::read_to_string(std::path::Path::new(&path).join("project.toml")).unwrap();
    assert!(toml.contains("prefix = \"NEWP\""), "{toml}");
}

#[test]
fn rm_of_the_default_announces_the_new_default() {
    let h = harness();
    h.cadet(&[
        "project",
        "add",
        "alpha",
        "--path",
        &h.vault("alpha"),
        "--prefix",
        "ALP",
    ])
    .assert()
    .success();
    h.cadet(&[
        "project",
        "add",
        "beta",
        "--path",
        &h.vault("beta"),
        "--prefix",
        "BET",
    ])
    .assert()
    .success();
    h.cadet(&["project", "use", "alpha"]).assert().success();
    h.cadet(&["project", "rm", "alpha"])
        .assert()
        .success()
        .stdout(predicates::str::contains("default project is now `beta`"));
}

#[test]
fn rm_of_the_last_project_says_no_default_is_set() {
    let h = harness();
    h.cadet(&[
        "project",
        "add",
        "solo",
        "--path",
        &h.vault("solo"),
        "--prefix",
        "SOL",
    ])
    .assert()
    .success();
    h.cadet(&["project", "rm", "solo"])
        .assert()
        .success()
        .stdout(predicates::str::contains("no default project is set"));
}

#[test]
fn rm_of_a_non_default_project_does_not_mention_the_default() {
    let h = harness();
    h.cadet(&[
        "project",
        "add",
        "alpha",
        "--path",
        &h.vault("alpha"),
        "--prefix",
        "ALP",
    ])
    .assert()
    .success();
    h.cadet(&[
        "project",
        "add",
        "beta",
        "--path",
        &h.vault("beta"),
        "--prefix",
        "BET",
    ])
    .assert()
    .success();
    h.cadet(&["project", "use", "alpha"]).assert().success();
    h.cadet(&["project", "rm", "beta"])
        .assert()
        .success()
        .stdout(predicates::str::contains("default").not());
}

#[test]
fn project_use_of_an_unknown_id_lists_known_projects() {
    let h = harness();
    h.cadet(&[
        "project",
        "add",
        "alpha",
        "--path",
        &h.vault("alpha"),
        "--prefix",
        "ALP",
    ])
    .assert()
    .success();
    h.cadet(&["project", "use", "ghost"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("ghost"))
        .stderr(predicates::str::contains("alpha"));
}

#[test]
fn project_rm_of_an_unknown_id_lists_known_projects() {
    let h = harness();
    h.cadet(&[
        "project",
        "add",
        "alpha",
        "--path",
        &h.vault("alpha"),
        "--prefix",
        "ALP",
    ])
    .assert()
    .success();
    h.cadet(&["project", "rm", "ghost"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("ghost"))
        .stderr(predicates::str::contains("alpha"));
}

#[test]
fn a_degenerate_id_names_itself_in_the_prefix_error() {
    let h = harness();
    h.cadet(&["project", "add", "日本", "--path", &h.vault("jp")])
        .assert()
        .failure()
        .stderr(predicates::str::contains("日本"))
        .stderr(predicates::str::contains("--prefix"));
}

#[test]
fn a_relative_path_is_stored_absolute_and_works_from_a_different_cwd() {
    let h = harness();
    let mut add = h.cadet(&[
        "project", "add", "rel", "--path", "relvault", "--prefix", "REL",
    ]);
    add.current_dir(h.root.path());
    add.assert().success();

    let mut from_elsewhere = h.cadet(&["--project", "rel", "add", "a task"]);
    from_elsewhere.current_dir(h.home.path());
    from_elsewhere
        .assert()
        .success()
        .stdout(predicates::str::contains("REL-1"));
}

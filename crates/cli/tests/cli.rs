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

    fn home(&self) -> &std::path::Path {
        self.home.path()
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

/// A harness for the field-flags / `set` / `ls`-filter tests below. Its
/// project always uses prefix `T`, since those tests address tasks as
/// `T-1` — asserted here rather than relied on by convention, so a harness
/// change that drifts the prefix fails at the harness, not at some unrelated
/// assertion three lines into a test.
struct ProjectHarness {
    home: tempfile::TempDir,
    vault: tempfile::TempDir,
}

impl ProjectHarness {
    fn cadet(&self, args: &[&str]) -> Command {
        let mut c = cadet(self.home.path());
        c.args(args);
        c
    }

    fn read_task(&self, filename: &str) -> String {
        std::fs::read_to_string(self.vault.path().join(filename)).unwrap()
    }
}

fn project_harness() -> ProjectHarness {
    let h = ProjectHarness {
        home: tempfile::tempdir().unwrap(),
        vault: tempfile::tempdir().unwrap(),
    };
    h.cadet(&[
        "project",
        "add",
        "proj",
        "--path",
        h.vault.path().to_str().unwrap(),
        "--prefix",
        "T",
        "--name",
        "Proj",
    ])
    .assert()
    .success();
    let toml = std::fs::read_to_string(h.vault.path().join("project.toml")).unwrap();
    assert!(
        toml.contains("prefix = \"T\""),
        "project_harness must create its project with prefix T: {toml}"
    );
    h
}

fn project_harness_with_field(name: &str, ty: &str) -> ProjectHarness {
    project_harness_with_fields(&[(name, ty)])
}

fn project_harness_with_fields(defs: &[(&str, &str)]) -> ProjectHarness {
    let h = project_harness();
    let path = h.vault.path().join("project.toml");
    let mut toml = std::fs::read_to_string(&path).unwrap();
    for (name, ty) in defs {
        toml.push_str(&format!(
            "\n[[fields]]\nname = \"{name}\"\ntype = \"{ty}\"\n"
        ));
    }
    std::fs::write(&path, toml).unwrap();
    h
}

#[test]
fn add_accepts_every_reserved_field() {
    let h = project_harness();
    h.cadet(&[
        "add",
        "big one",
        "--due",
        "2026-08-10",
        "--tag",
        "home",
        "--tag",
        "urgent",
        "--priority",
        "high",
    ])
    .assert()
    .success();
    let src = h.read_task("big-one.md");
    assert!(src.contains("due: 2026-08-10"), "{src}");
    assert!(src.contains("priority: high"), "{src}");
    assert!(src.contains("home"), "{src}");
    assert!(src.contains("urgent"), "{src}");
}

#[test]
fn set_updates_one_field_and_leaves_the_rest() {
    let h = project_harness();
    h.cadet(&["add", "movable", "--due", "2026-08-10", "--tag", "home"])
        .assert()
        .success();
    h.cadet(&["set", "T-1", "priority=high"]).assert().success();
    let src = h.read_task("movable.md");
    assert!(src.contains("priority: high"), "{src}");
    assert!(src.contains("due: 2026-08-10"), "due must survive: {src}");
    assert!(src.contains("home"), "tags must survive: {src}");
}

#[test]
fn set_with_an_empty_value_clears_the_field() {
    let h = project_harness();
    h.cadet(&["add", "clearable", "--due", "2026-08-10"])
        .assert()
        .success();
    h.cadet(&["set", "T-1", "due="]).assert().success();
    let src = h.read_task("clearable.md");
    assert!(!src.contains("due: 2026-08-10"), "{src}");
}

#[test]
fn ls_filters_by_tag() {
    let h = project_harness();
    h.cadet(&["add", "one", "--tag", "home"]).assert().success();
    h.cadet(&["add", "two", "--tag", "work"]).assert().success();
    h.cadet(&["ls", "--tag", "home"])
        .assert()
        .success()
        .stdout(predicates::str::contains("one").and(predicates::str::contains("two").not()));
}

#[test]
fn ls_filters_by_due_window() {
    let h = project_harness();
    h.cadet(&["add", "soon", "--due", "2026-08-05"])
        .assert()
        .success();
    h.cadet(&["add", "later", "--due", "2026-09-05"])
        .assert()
        .success();
    h.cadet(&["ls", "--due-before", "2026-08-31"])
        .assert()
        .success()
        .stdout(predicates::str::contains("soon").and(predicates::str::contains("later").not()));
}

#[test]
fn an_undeclared_field_names_the_config_file() {
    let h = project_harness();
    h.cadet(&["add", "x", "--set", "estimate=3"])
        .assert()
        .failure()
        .stderr(
            predicates::str::contains("estimate").and(predicates::str::contains("project.toml")),
        );
}

#[test]
fn a_declared_field_round_trips_through_add_and_ls() {
    let h = project_harness_with_field("estimate", "int");
    h.cadet(&["add", "sized", "--set", "estimate=3"])
        .assert()
        .success();
    h.cadet(&["ls", "--field", "estimate=3"])
        .assert()
        .success()
        .stdout(predicates::str::contains("sized"));
}

#[test]
fn a_bad_value_for_a_declared_field_is_rejected_before_writing() {
    let h = project_harness_with_field("estimate", "int");
    h.cadet(&["add", "bad", "--set", "estimate=soon"])
        .assert()
        .failure()
        .stderr(
            predicates::str::contains("estimate").and(predicates::str::contains("whole number")),
        );
    h.cadet(&["ls"])
        .assert()
        .success()
        .stdout(predicates::str::contains("bad").not());
}

#[test]
fn due_before_rejects_a_non_date_bound() {
    let h = project_harness();
    h.cadet(&["ls", "--due-before", "banana"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("due-before"));
}

#[test]
fn due_after_rejects_a_non_date_bound() {
    let h = project_harness();
    h.cadet(&["ls", "--due-after", "banana"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("due-after"));
}

// --- Review round 2 fixes below ---

/// I1: an empty value on an undeclared field must still be caught by the
/// declaration check, not fall through and lose the enriched message.
#[test]
fn set_with_an_empty_value_on_an_undeclared_field_names_the_config_file() {
    let h = project_harness();
    h.cadet(&["add", "x"]).assert().success();
    h.cadet(&["set", "T-1", "estimate="])
        .assert()
        .failure()
        .stderr(
            predicates::str::contains("estimate").and(predicates::str::contains("project.toml")),
        );
}

#[test]
fn add_set_with_an_empty_value_on_an_undeclared_field_names_the_config_file() {
    let h = project_harness();
    h.cadet(&["add", "x", "--set", "estimate="])
        .assert()
        .failure()
        .stderr(
            predicates::str::contains("estimate").and(predicates::str::contains("project.toml")),
        );
}

/// I2: `ls --field` on a reserved name must redirect to the flag that
/// already covers it, not advise a `[[fields]]` declaration that would
/// fail to parse (a reserved name shadowed in `project.toml`) and brick
/// every subsequent command.
#[test]
fn ls_field_on_reserved_names_redirects_instead_of_advising_a_declaration() {
    let h = project_harness();
    h.cadet(&["add", "x"]).assert().success();
    for (name, hint) in [
        ("tags", "--tag"),
        ("due", "--due-before"),
        ("state", "--state"),
        ("priority", "--priority"),
    ] {
        h.cadet(&["ls", "--field", &format!("{name}=x")])
            .assert()
            .failure()
            .stderr(
                predicates::str::contains(hint)
                    .and(predicates::str::contains("declare it in").not()),
            );
    }
    h.cadet(&["ls", "--field", "title=x"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("declare it in").not());
}

#[test]
fn ls_field_on_a_reserved_name_never_corrupts_the_project() {
    let h = project_harness();
    h.cadet(&["add", "x"]).assert().success();
    h.cadet(&["ls", "--field", "tags=x"]).assert().failure();
    // The project must still be usable — following the old (buggy) advice
    // to `declare it in project.toml` a reserved name is what bricked
    // every later command.
    h.cadet(&["ls"])
        .assert()
        .success()
        .stdout(predicates::str::contains("x"));
}

/// I3: `--tag` must trim like `--set tags=` does, and reject an embedded
/// comma outright rather than silently disagreeing with the comma-split
/// `--set` spelling.
#[test]
fn tag_flag_trims_whitespace() {
    let h = project_harness();
    h.cadet(&["add", "spaced", "--tag", "  home  "])
        .assert()
        .success();
    h.cadet(&["ls", "--tag", "home"])
        .assert()
        .success()
        .stdout(predicates::str::contains("spaced"));
}

/// The other half of the same rule: `--set tags=a,,b` drops an empty item,
/// so `--tag ""` must not quietly store one. It also split the backends —
/// markdown drops an empty tag on the way back out of frontmatter, local-db
/// keeps it.
#[test]
fn tag_flag_rejects_an_empty_tag() {
    let h = project_harness();
    h.cadet(&["add", "x", "--tag", "  "])
        .assert()
        .failure()
        .stderr(predicates::str::contains("empty"));
}

/// `--set title=` trims and the positional title did not, so the same field
/// had two spellings that disagreed — and the padded version then stored
/// differently on each backend, since a frontmatter scalar reads back trimmed
/// and a database column keeps every space. On a local-db project only, which
/// is where the difference is visible: markdown launders the padding away on
/// the read-back that fills the cache, so it cannot see this at all.
/// `show <prefix>` matches on the title, so it finds this task only if the
/// padding never reached the store.
#[test]
fn a_padded_positional_title_is_trimmed_like_set_title_is() {
    let h = harness();
    h.cadet(&["project", "add", "scratch", "--backend", "local-db"])
        .assert()
        .success();
    h.cadet(&["add", "   spaced out   "]).assert().success();
    h.cadet(&["show", "spaced"])
        .assert()
        .success()
        .stdout(predicates::str::contains("spaced out"));
}

#[test]
fn tag_flag_rejects_an_embedded_comma() {
    let h = project_harness();
    h.cadet(&["add", "x", "--tag", "home,urgent"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("comma"));
}

#[test]
fn tag_flag_and_set_tags_agree_on_two_tags() {
    let h = project_harness();
    h.cadet(&["add", "viaflags", "--tag", "home", "--tag", "urgent"])
        .assert()
        .success();
    h.cadet(&["add", "viaset", "--set", "tags=home,urgent"])
        .assert()
        .success();
    h.cadet(&["ls", "--tag", "home", "--tag", "urgent"])
        .assert()
        .success()
        .stdout(predicates::str::contains("viaflags").and(predicates::str::contains("viaset")));
}

/// Review finding 2, the twentieth instance of the signature defect.
/// `parse_field_value` rejects a newline in a custom field value because
/// frontmatter is line-oriented and a newline is an injection vector. A title
/// goes into the same frontmatter, and had no guard: `title: two\nlines`
/// writes an orphan frontmatter line and `cadet show` reads the title back as
/// `two`, with the rest unrecoverable. Every route that sets a title is
/// covered, because the one that is not is the one a user finds.
#[test]
fn a_newline_in_a_title_is_rejected_for_every_route_that_sets_one() {
    let h = project_harness();
    for args in [
        vec!["add", "two\nlines"],
        vec!["add", "--set", "title=two\nlines"],
    ] {
        h.cadet(&args)
            .assert()
            .failure()
            .stderr(predicates::str::contains("title").and(predicates::str::contains("newline")));
    }
    h.cadet(&["add", "settable"]).assert().success();
    h.cadet(&["set", "T-1", "title=two\nlines"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("title").and(predicates::str::contains("newline")));

    // Nothing may have landed on the way out.
    h.cadet(&["ls"])
        .assert()
        .success()
        .stdout(predicates::str::contains("lines").not());
}

/// The same input must be rejected on a local-db project too, even though
/// that backend stores a multi-line title perfectly well. A CLI where `add`
/// succeeds on one project and fails on another for the same input is worse
/// than losing a capability nobody wants.
#[test]
fn a_newline_in_a_title_is_rejected_on_a_local_db_project_too() {
    let h = harness();
    h.cadet(&["project", "add", "scratch", "--backend", "local-db"])
        .assert()
        .success();
    h.cadet(&["add", "two\nlines"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("title").and(predicates::str::contains("newline")));
}

/// Also-do: `add --set due=` with a bad value should give the CLI's date
/// message, the same one `add --due` gives, not the core validator's.
#[test]
fn add_set_due_with_a_bad_value_gives_the_cli_date_message() {
    let h = project_harness();
    h.cadet(&["add", "x", "--set", "due=banana"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("is not a date"));
}

#[test]
fn add_set_due_empty_is_rejected_as_nothing_to_clear() {
    let h = project_harness();
    h.cadet(&["add", "x", "--set", "due="])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot clear").and(predicates::str::contains("due")));
}

/// Also-do: `--priority` is a clap `ValueEnum`, so its choices show in
/// `--help` and an invalid value is rejected before the app even opens.
#[test]
fn add_help_lists_the_priority_choices() {
    let h = project_harness();
    h.cadet(&["add", "--help"]).assert().success().stdout(
        predicates::str::contains("high")
            .and(predicates::str::contains("normal"))
            .and(predicates::str::contains("low")),
    );
}

#[test]
fn an_invalid_priority_is_rejected_by_the_cli_itself() {
    let h = project_harness();
    h.cadet(&["add", "x", "--priority", "medium"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("high"));
}

/// Also-do: a repeated assignment name is rejected rather than silently
/// resolved last-wins (`set`) or built into an unmatchable filter (`ls
/// --field`).
#[test]
fn set_rejects_a_repeated_assignment_name() {
    let h = project_harness();
    h.cadet(&["add", "x"]).assert().success();
    h.cadet(&["set", "T-1", "priority=low", "priority=high"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("priority"));
}

#[test]
fn add_set_rejects_a_repeated_assignment_name() {
    let h = project_harness_with_field("estimate", "int");
    h.cadet(&["add", "x", "--set", "estimate=1", "--set", "estimate=2"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("estimate"));
}

#[test]
fn ls_field_rejects_a_repeated_name() {
    let h = project_harness_with_field("estimate", "int");
    h.cadet(&["add", "x", "--set", "estimate=3"])
        .assert()
        .success();
    h.cadet(&["ls", "--field", "estimate=3", "--field", "estimate=5"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("estimate"));
}

/// Close the CLI-level test gaps the reviewer named: repeated `--state` is
/// OR'd and repeated `--tag` is AND'd at the CLI itself, not only proven at
/// the `TaskFilter` unit-test level.
#[test]
fn ls_state_filter_ors_repeated_values_at_the_cli() {
    let h = project_harness();
    h.cadet(&["add", "a"]).assert().success();
    h.cadet(&["add", "b"]).assert().success();
    h.cadet(&["mv", "T-2", "doing"]).assert().success();
    h.cadet(&["ls", "--state", "todo", "--state", "doing"])
        .assert()
        .success()
        .stdout(predicates::str::contains("a").and(predicates::str::contains("b")));
}

#[test]
fn ls_tag_filter_ands_repeated_values_at_the_cli() {
    let h = project_harness();
    h.cadet(&["add", "both", "--tag", "home", "--tag", "urgent"])
        .assert()
        .success();
    h.cadet(&["add", "one", "--tag", "home"]).assert().success();
    h.cadet(&["ls", "--tag", "home", "--tag", "urgent"])
        .assert()
        .success()
        .stdout(predicates::str::contains("both").and(predicates::str::contains("one").not()));
}

#[test]
fn ls_filters_by_priority_at_the_cli() {
    let h = project_harness();
    h.cadet(&["add", "urgent", "--priority", "high"])
        .assert()
        .success();
    h.cadet(&["add", "meh", "--priority", "low"])
        .assert()
        .success();
    h.cadet(&["ls", "--priority", "high"])
        .assert()
        .success()
        .stdout(predicates::str::contains("urgent").and(predicates::str::contains("meh").not()));
}

#[test]
fn ls_field_filters_a_non_int_type() {
    let h = project_harness_with_field("shipped", "bool");
    h.cadet(&["add", "yes", "--set", "shipped=true"])
        .assert()
        .success();
    h.cadet(&["add", "no", "--set", "shipped=false"])
        .assert()
        .success();
    h.cadet(&["ls", "--field", "shipped=true"])
        .assert()
        .success()
        .stdout(predicates::str::contains("yes").and(predicates::str::contains("no").not()));
}

fn commit_count(h: &ProjectHarness) -> u32 {
    let repo_dir = h.home.path().join("repos").join("proj.git");
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&repo_dir)
        .arg("--work-tree")
        .arg(h.vault.path())
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

#[test]
fn set_with_multiple_assignments_makes_exactly_one_commit() {
    let h = project_harness();
    h.cadet(&["add", "multi", "--due", "2026-08-10"])
        .assert()
        .success();
    let before = commit_count(&h);
    h.cadet(&[
        "set",
        "T-1",
        "priority=high",
        "state=doing",
        "due=2026-09-01",
    ])
    .assert()
    .success();
    let after = commit_count(&h);
    assert_eq!(
        after,
        before + 1,
        "set must fold every assignment into one commit, not one per assignment"
    );
}

#[test]
fn add_set_state_agrees_with_add_state_flag() {
    let h = project_harness();
    h.cadet(&["add", "viaflag", "--state", "doing"])
        .assert()
        .success();
    h.cadet(&["add", "viaset", "--set", "state=doing"])
        .assert()
        .success();
    assert!(h.read_task("viaflag.md").contains("state: doing"));
    assert!(h.read_task("viaset.md").contains("state: doing"));
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

fn stdout_of(cmd: Command) -> String {
    let mut cmd = cmd;
    let out = cmd.assert().success().get_output().stdout.clone();
    String::from_utf8(out).unwrap()
}

/// The line `show` prints for `label`, panicking with the whole output when
/// there is none — a plain `contains("nm")` would pass on a task whose title
/// happened to contain the value, which is exactly the confusion this
/// finding is about.
fn labelled_line(out: &str, label: &str) -> String {
    out.lines()
        .find(|l| l.starts_with(&format!("{label}:")))
        .unwrap_or_else(|| panic!("no `{label}:` line in show output:\n{out}"))
        .to_string()
}

fn value_of(out: &str, label: &str) -> String {
    labelled_line(out, label)
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .to_string()
}

#[test]
fn show_displays_a_custom_fields_value() {
    let h = project_harness_with_field("owner", "str");
    h.cadet(&["add", "owned", "--set", "owner=nm"])
        .assert()
        .success();
    let out = stdout_of(h.cadet(&["show", "T-1"]));
    assert_eq!(value_of(&out, "owner"), "nm", "{out}");
}

#[test]
fn show_displays_every_scalar_field_type_readably() {
    let h = project_harness_with_fields(&[
        ("estimate", "int"),
        ("ratio", "float"),
        ("shipped", "bool"),
        ("start", "date"),
    ]);
    h.cadet(&[
        "add",
        "everything",
        "--set",
        "estimate=5",
        "--set",
        "ratio=1.5",
        "--set",
        "shipped=true",
        "--set",
        "start=2026-09-01",
    ])
    .assert()
    .success();
    let out = stdout_of(h.cadet(&["show", "T-1"]));
    assert_eq!(value_of(&out, "estimate"), "5", "{out}");
    assert_eq!(value_of(&out, "ratio"), "1.5", "{out}");
    assert_eq!(value_of(&out, "shipped"), "true", "{out}");
    assert_eq!(value_of(&out, "start"), "2026-09-01", "{out}");
}

/// A `list<string>` must read like the `tags` line, not like `List(["ann",
/// "bob"])` — the finding calls out Rust debug output by name.
#[test]
fn show_renders_a_list_field_like_the_tags_line() {
    let h = project_harness_with_field("people", "list<string>");
    h.cadet(&["add", "shared", "--set", "people=ann,bob"])
        .assert()
        .success();
    let out = stdout_of(h.cadet(&["show", "T-1"]));
    assert_eq!(value_of(&out, "people"), "ann, bob", "{out}");
    assert!(!out.contains("List("), "no debug formatting: {out}");
    assert!(!out.contains('['), "no debug formatting: {out}");
}

#[test]
fn show_displays_a_non_normal_priority() {
    let h = project_harness();
    h.cadet(&["add", "urgent", "--priority", "high"])
        .assert()
        .success();
    let out = stdout_of(h.cadet(&["show", "T-1"]));
    assert_eq!(value_of(&out, "priority"), "high", "{out}");
}

/// `normal` is every task's default, so printing it would put a line on
/// every task that carries no information.
#[test]
fn show_omits_a_normal_priority() {
    let h = project_harness();
    h.cadet(&["add", "ordinary"]).assert().success();
    let out = stdout_of(h.cadet(&["show", "T-1"]));
    assert!(!out.contains("priority"), "{out}");
}

#[test]
fn show_survives_a_field_cleared_back_to_nothing() {
    let h = project_harness_with_field("owner", "str");
    h.cadet(&["add", "owned", "--set", "owner=nm"])
        .assert()
        .success();
    h.cadet(&["set", "T-1", "owner="]).assert().success();
    let out = stdout_of(h.cadet(&["show", "T-1"]));
    assert!(!out.contains("owner"), "{out}");
}

#[test]
fn ls_shows_a_non_normal_priority() {
    let h = project_harness();
    h.cadet(&["add", "urgent", "--priority", "high"])
        .assert()
        .success();
    h.cadet(&["add", "ordinary"]).assert().success();
    let out = stdout_of(h.cadet(&["ls"]));
    let urgent = out
        .lines()
        .find(|l| l.contains("urgent"))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(urgent.contains("high"), "{out}");
    let ordinary = out
        .lines()
        .find(|l| l.contains("ordinary"))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(!ordinary.contains("normal"), "{out}");
}

/// The priority column costs a task list nothing when no task has one — the
/// row stays exactly as it was before the column existed.
#[test]
fn ls_stays_compact_when_no_task_has_a_priority() {
    let h = project_harness();
    h.cadet(&["add", "plain"]).assert().success();
    let out = stdout_of(h.cadet(&["ls"]));
    assert_eq!(out, "T-1        todo     plain\n", "{out:?}");
}

/// `--force` re-registers a project; it must not silently redefine what a
/// task *is*. Wiping `[[fields]]` strands every value already written under
/// a declaration that no longer exists, and wiping `[workflow]` strands
/// every task in a state that no longer exists.
#[test]
fn force_overwrite_preserves_declared_fields_and_workflow() {
    let h = project_harness_with_field("estimate", "int");
    h.cadet(&["add", "sized", "--set", "estimate=5"])
        .assert()
        .success();
    let path = h.vault.path().join("project.toml");
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml = toml.replace(
        r#"states = ["todo", "doing", "blocked", "done"]"#,
        r#"states = ["todo", "review", "done"]"#,
    );
    std::fs::write(&path, &toml).unwrap();
    assert!(
        toml.contains("review"),
        "harness must customise the workflow"
    );

    h.cadet(&[
        "project",
        "add",
        "proj",
        "--path",
        h.vault.path().to_str().unwrap(),
        "--force",
    ])
    .assert()
    .success();

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.contains("estimate"),
        "declarations must survive: {after}"
    );
    assert!(
        after.contains("review"),
        "the workflow must survive: {after}"
    );

    // The symptom, not just the file: an already-written value stays
    // reachable, and a task can still be moved into a customised state.
    h.cadet(&["set", "T-1", "estimate=9"]).assert().success();
    h.cadet(&["mv", "T-1", "review"]).assert().success();
}

/// The three keys `project add` owns are still rewritten — preservation must
/// not turn into "ignores what you asked for".
#[test]
fn force_overwrite_still_rewrites_id_name_and_prefix() {
    let h = project_harness_with_field("estimate", "int");
    h.cadet(&[
        "project",
        "add",
        "proj",
        "--path",
        h.vault.path().to_str().unwrap(),
        "--prefix",
        "ZZ",
        "--name",
        "Renamed",
        "--force",
    ])
    .assert()
    .success();
    let after = std::fs::read_to_string(h.vault.path().join("project.toml")).unwrap();
    assert!(after.contains(r#"prefix = "ZZ""#), "{after}");
    assert!(after.contains(r#"name = "Renamed""#), "{after}");
    assert!(after.contains("estimate"), "{after}");
}

/// A file that is not even TOML is the ONLY case where there is nothing to
/// preserve — `--force` falls back to a fresh template rather than refusing
/// to proceed. Narrowly scoped on purpose: "not valid TOML" is a far smaller
/// set than "not a valid config", and the sibling test above covers the
/// difference. The content here must stay genuinely unparseable.
#[test]
fn force_overwrite_replaces_a_file_that_is_not_even_toml() {
    let h = project_harness();
    let path = h.vault.path().join("project.toml");
    let broken = "this is not [ toml";
    assert!(
        broken.parse::<toml_edit::DocumentMut>().is_err(),
        "this test is only meaningful for input that is not TOML"
    );
    std::fs::write(&path, broken).unwrap();
    h.cadet(&[
        "project",
        "add",
        "proj",
        "--path",
        h.vault.path().to_str().unwrap(),
        "--prefix",
        "T",
        "--force",
    ])
    .assert()
    .success();
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("[workflow]"), "{after}");
}

/// The template's `[[fields]]` example is the only on-ramp to the headline
/// feature, so it has to be *correct*, not just present: uncomment it as
/// written and both fields must work.
#[test]
fn the_template_field_example_works_when_uncommented() {
    let h = project_harness();
    let path = h.vault.path().join("project.toml");
    let toml = std::fs::read_to_string(&path).unwrap();
    let (head, example) = toml
        .split_once("# [[fields]]")
        .unwrap_or_else(|| panic!("the template has no [[fields]] example:\n{toml}"));
    let uncommented: String = example
        .lines()
        .map(|l| {
            l.strip_prefix("# ")
                .unwrap_or_else(|| l.strip_prefix('#').unwrap_or(l))
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{head}[[fields]]{uncommented}\n")).unwrap();

    h.cadet(&["add", "sized", "--set", "estimate=3", "--set", "size=m"])
        .assert()
        .success();
    let out = stdout_of(h.cadet(&["show", "T-1"]));
    assert_eq!(value_of(&out, "estimate"), "3", "{out}");
    assert_eq!(value_of(&out, "size"), "m", "{out}");
}

/// An enum declared with `choices` used to become an enum with no options,
/// which rejected every value with an empty `expects one of:` list.
#[test]
fn an_enum_declared_with_choices_accepts_its_values() {
    let h = project_harness();
    let path = h.vault.path().join("project.toml");
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str("\n[[fields]]\nname = \"size\"\ntype = \"enum\"\nchoices = [\"s\", \"m\"]\n");
    std::fs::write(&path, toml).unwrap();
    h.cadet(&["add", "sized", "--set", "size=m"])
        .assert()
        .success();
    h.cadet(&["add", "bad", "--set", "size=xl"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("s, m"));
}

/// An enum with no options can never be satisfied, so it is a broken
/// declaration rather than a field that rejects everything.
#[test]
fn an_enum_with_no_options_is_a_config_error() {
    let h = project_harness();
    let path = h.vault.path().join("project.toml");
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str("\n[[fields]]\nname = \"size\"\ntype = \"enum\"\n");
    std::fs::write(&path, toml).unwrap();
    h.cadet(&["add", "sized", "--set", "size=m"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("size"))
        .stderr(predicates::str::contains("no options"));
}

/// `--priority high --set priority=low` silently yielded `low`. `--set` is
/// already refused when it repeats a name inside itself; the same collision
/// one level out gets the same answer.
#[test]
fn add_rejects_set_colliding_with_a_dedicated_flag() {
    let h = project_harness();
    for args in [
        vec!["add", "a", "--priority", "high", "--set", "priority=low"],
        vec!["add", "b", "--tag", "home", "--set", "tags=work"],
        vec!["add", "c", "--due", "2026-08-10", "--set", "due=2026-09-01"],
        vec!["add", "d", "--state", "doing", "--set", "state=todo"],
        vec!["add", "e", "--set", "title=other"],
    ] {
        let name = args[args.len() - 1].split('=').next().unwrap().to_string();
        h.cadet(&args)
            .assert()
            .failure()
            .stderr(predicates::str::contains(name));
    }
    h.cadet(&["ls", "--all"])
        .assert()
        .success()
        .stdout(predicates::str::contains("no tasks"));
}

#[test]
fn add_still_accepts_set_alongside_an_unrelated_flag() {
    let h = project_harness_with_field("owner", "str");
    h.cadet(&["add", "fine", "--priority", "high", "--set", "owner=nm"])
        .assert()
        .success();
    let out = stdout_of(h.cadet(&["show", "T-1"]));
    assert_eq!(value_of(&out, "priority"), "high", "{out}");
    assert_eq!(value_of(&out, "owner"), "nm", "{out}");
}

/// `--set title=` with no positional title is the only way to give a title
/// that starts with a dash, so it must stay legal.
#[test]
fn add_accepts_set_title_when_no_positional_title_is_given() {
    let h = project_harness();
    h.cadet(&["add", "--set", "title=from set"])
        .assert()
        .success();
    let out = stdout_of(h.cadet(&["show", "T-1"]));
    assert!(out.contains("from set"), "{out}");
}

/// `--project bogus` and `CADET_PROJECT=bogus` select a project the same
/// way, so they must fail the same way. `CADET_PROJECT` used to report "no
/// default project set" — false, and it points at the wrong thing to fix.
#[test]
fn a_stale_cadet_project_env_var_names_itself_and_the_known_projects() {
    let e = env();
    let mut c = cadet(&e.home);
    c.env("CADET_PROJECT", "ghost");
    c.arg("ls")
        .assert()
        .failure()
        .stderr(predicates::str::contains("ghost"))
        .stderr(predicates::str::contains("personal"))
        .stderr(predicates::str::contains("CADET_PROJECT"))
        .stderr(predicates::str::contains("no default project set").not());
}

#[test]
fn cadet_project_still_selects_a_real_project() {
    let e = env();
    let mut c = cadet(&e.home);
    c.env("CADET_PROJECT", "personal");
    c.args(["add", "via env"]).assert().success();
    cadet(&e.home)
        .arg("ls")
        .assert()
        .success()
        .stdout(predicates::str::contains("via env"));
}

/// `--project` still wins over the environment.
#[test]
fn an_explicit_project_flag_beats_the_env_var() {
    let h = harness();
    for id in ["alpha", "beta"] {
        h.cadet(&["project", "add", id, "--path", &h.vault(id)])
            .assert()
            .success();
    }
    let mut c = h.cadet(&["--project", "beta", "add", "in beta"]);
    c.env("CADET_PROJECT", "alpha");
    c.assert()
        .success()
        .stdout(predicates::str::contains("BETA-1"));
}

/// The write path already rejects an undeclared state — `mv` and `add
/// --state` both do. `ls --state` returning `no tasks` for a typo reads as
/// "you have none of those", which is a wrong answer rather than an error.
#[test]
fn ls_state_filter_rejects_an_undeclared_state() {
    let h = project_harness();
    h.cadet(&["add", "real"]).assert().success();
    h.cadet(&["ls", "--state", "nonsense"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("nonsense"))
        .stderr(predicates::str::contains("todo"));
    // The same name the write path refuses.
    h.cadet(&["mv", "T-1", "nonsense"]).assert().failure();
}

#[test]
fn ls_state_filter_still_accepts_a_declared_state() {
    let h = project_harness();
    h.cadet(&["add", "real"]).assert().success();
    h.cadet(&["ls", "--state", "todo"])
        .assert()
        .success()
        .stdout(predicates::str::contains("real"));
    h.cadet(&["ls", "--state", "doing"])
        .assert()
        .success()
        .stdout(predicates::str::contains("no tasks"));
}

fn fill_with_notes(dir: &std::path::Path, n: usize) {
    std::fs::create_dir_all(dir).unwrap();
    for i in 0..n {
        std::fs::write(dir.join(format!("note-{i}.md")), "hand written\n").unwrap();
    }
}

/// `--path '~'` expands correctly and then makes every `.md` in the user's
/// home directory a task. The expansion is fine; the consequence needs a
/// gate. Refuses outside a TTY rather than prompting into a pipe.
#[test]
fn project_add_refuses_a_folder_already_full_of_notes() {
    let h = harness();
    let dir = h.root.path().join("bignotes");
    fill_with_notes(&dir, 51);
    h.cadet(&["project", "add", "big", "--path", dir.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicates::str::contains("50"))
        .stderr(predicates::str::contains("--yes"));
    assert!(!dir.join("project.toml").exists(), "nothing may be written");
}

#[test]
fn project_add_yes_overrides_the_many_notes_guard() {
    let h = harness();
    let dir = h.root.path().join("bignotes");
    fill_with_notes(&dir, 51);
    h.cadet(&[
        "project",
        "add",
        "big",
        "--path",
        dir.to_str().unwrap(),
        "--yes",
    ])
    .assert()
    .success();
    assert!(dir.join("project.toml").exists());
}

#[test]
fn project_add_ignores_a_handful_of_notes() {
    let h = harness();
    let dir = h.root.path().join("smallnotes");
    fill_with_notes(&dir, 3);
    h.cadet(&["project", "add", "small", "--path", dir.to_str().unwrap()])
        .assert()
        .success();
}

/// A project's own task files must never make it un-re-registerable.
#[test]
fn re_adding_an_existing_project_is_not_blocked_by_its_own_notes() {
    let h = project_harness();
    fill_with_notes(h.vault.path(), 60);
    h.cadet(&[
        "project",
        "add",
        "proj",
        "--path",
        h.vault.path().to_str().unwrap(),
        "--force",
    ])
    .assert()
    .success();
}

/// The dot-entry skip has to match `MarkdownBackend::markdown_files`, or the guard
/// counts files adoption will never look at.
#[test]
fn the_many_notes_guard_ignores_dot_directories() {
    let h = harness();
    let dir = h.root.path().join("dotted");
    fill_with_notes(&dir.join(".obsidian"), 60);
    h.cadet(&["project", "add", "dotted", "--path", dir.to_str().unwrap()])
        .assert()
        .success();
}

/// "Not valid TOML" and "not a valid config" are different sets, and the
/// second is much larger — an enum that spells its options key wrong is
/// valid TOML and an invalid config. `--force` is the repair command, so it
/// must not be the thing that eats exactly the files that need repairing:
/// preserve the document, and let the validation before the write refuse.
#[test]
fn force_overwrite_of_a_config_invalid_project_toml_preserves_it_and_errors() {
    let h = project_harness_with_field("estimate", "int");
    let path = h.vault.path().join("project.toml");
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml = toml.replace(
        r#"states = ["todo", "doing", "blocked", "done"]"#,
        r#"states = ["todo", "review", "done"]"#,
    );
    // A plausible typo: valid TOML, rejected as a config.
    toml.push_str("\n[[fields]]\nname = \"size\"\ntype = \"enum\"\noptions = [\"s\", \"m\"]\n");
    std::fs::write(&path, &toml).unwrap();

    h.cadet(&[
        "project",
        "add",
        "proj",
        "--path",
        h.vault.path().to_str().unwrap(),
        "--force",
    ])
    .assert()
    .failure()
    .stderr(predicates::str::contains("size"));

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        after, toml,
        "nothing may be written when the result is invalid"
    );
}

#[test]
fn a_local_db_project_needs_no_path_and_lands_in_cadet_home() {
    let h = harness();
    h.cadet(&["project", "add", "scratch", "--backend", "local-db"])
        .assert()
        .success();
    assert!(h.home().join("projects").join("scratch.db").exists());
    assert!(h.home().join("projects").join("scratch.toml").exists());
}

#[test]
fn tasks_round_trip_through_a_local_db_project() {
    let h = harness();
    h.cadet(&[
        "project",
        "add",
        "scratch",
        "--backend",
        "local-db",
        "--prefix",
        "S",
    ])
    .assert()
    .success();
    h.cadet(&["add", "buy milk", "--tag", "errand"])
        .assert()
        .success();
    h.cadet(&["ls"])
        .assert()
        .success()
        .stdout(predicates::str::contains("S-1").and(predicates::str::contains("buy milk")));
    h.cadet(&["ls", "--tag", "errand"])
        .assert()
        .success()
        .stdout(predicates::str::contains("buy milk"));
    h.cadet(&["done", "S-1"]).assert().success();
    h.cadet(&["ls"])
        .assert()
        .success()
        .stdout(predicates::str::contains("buy milk").not());
}

#[test]
fn undo_reports_that_a_local_db_project_has_none() {
    let h = harness();
    h.cadet(&["project", "add", "scratch", "--backend", "local-db"])
        .assert()
        .success();
    h.cadet(&["add", "a task"]).assert().success();
    h.cadet(&["undo"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("undo"));
}

#[test]
fn adopt_reports_that_a_local_db_project_has_nothing_to_adopt() {
    let h = harness();
    h.cadet(&["project", "add", "scratch", "--backend", "local-db"])
        .assert()
        .success();
    h.cadet(&["add", "a task"]).assert().success();
    h.cadet(&["adopt"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("adopt"));
}

#[test]
fn the_listing_shows_each_project_backend() {
    let h = harness();
    h.cadet(&[
        "project",
        "add",
        "notes",
        "--path",
        &h.vault("notes"),
        "--prefix",
        "N",
    ])
    .assert()
    .success();
    h.cadet(&["project", "add", "scratch", "--backend", "local-db"])
        .assert()
        .success();
    h.cadet(&["project"])
        .assert()
        .success()
        .stdout(predicates::str::contains("markdown").and(predicates::str::contains("local-db")));
}

#[test]
fn an_unknown_backend_on_the_flag_is_rejected_with_the_choices() {
    let h = harness();
    h.cadet(&["project", "add", "x", "--backend", "telepathy"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("markdown").and(predicates::str::contains("local-db")));
}

#[test]
fn a_markdown_and_a_local_db_project_coexist_with_separate_key_spaces() {
    let h = harness();
    h.cadet(&[
        "project",
        "add",
        "notes",
        "--path",
        &h.vault("notes"),
        "--prefix",
        "N",
    ])
    .assert()
    .success();
    h.cadet(&[
        "project",
        "add",
        "scratch",
        "--backend",
        "local-db",
        "--prefix",
        "S",
    ])
    .assert()
    .success();
    h.cadet(&["--project", "notes", "add", "a note task"])
        .assert()
        .success();
    h.cadet(&["--project", "scratch", "add", "a scratch task"])
        .assert()
        .success();
    h.cadet(&["--project", "notes", "ls"])
        .assert()
        .success()
        .stdout(predicates::str::contains("N-1").and(predicates::str::contains("scratch").not()));
    h.cadet(&["--project", "scratch", "ls"])
        .assert()
        .success()
        .stdout(predicates::str::contains("S-1"));
}

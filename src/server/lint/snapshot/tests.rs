use super::{ProjectSnapshot, RULES_DIRECTORY, input_identity};
use crate::{analysis::Revision, workspace::Document};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use url::Url;

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create project directory");
    }
    fs::write(path, text).expect("write project source");
}

fn document(path: &Path, text: &str) -> Document {
    Document::new(
        Url::from_file_path(path).expect("file URI"),
        "bend".into(),
        Revision(1),
        text.into(),
    )
}

struct Fixture {
    directory: tempfile::TempDir,
    project: PathBuf,
    root: PathBuf,
}

// A rule program is compiled from disk, so every file reachable from these
// rule imports is executable trusted code rather than a linted source.
fn fixture(rule: &str) -> Fixture {
    let directory = tempfile::tempdir().expect("rule project");
    let project = directory.path().join("project");
    write(&project.join("bend-lint.json"), "{\"rules\":{}}");
    write(&project.join("main.bend"), "def main: U32\n  0\n");
    write(&project.join(RULES_DIRECTORY).join("style.bend"), rule);
    let root = project.join("main.bend");
    Fixture {
        directory,
        project,
        root,
    }
}

#[test]
fn an_unsaved_rule_helper_outside_the_rules_directory_is_rejected() {
    let fixture =
        fixture("import ../shared/helper.bend as Helper\n\ndef positive: U32\n  Helper.value\n");
    let helper = fixture.project.join("shared/helper.bend");
    write(&helper, "def value: U32\n  1\n");
    let error =
        ProjectSnapshot::capture(&fixture.root, &[document(&helper, "def value: U32\n  2\n")])
            .err()
            .expect("an unsaved executed rule helper cannot be overlaid");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
}

#[test]
fn a_saved_rule_helper_never_becomes_a_source_only_overlay() {
    let fixture =
        fixture("import ../shared/helper.bend as Helper\n\ndef positive: U32\n  Helper.value\n");
    let helper = fixture.project.join("shared/helper.bend");
    write(&helper, "def value: U32\n  1\n");
    let unsaved_root = "def main: U32\n  1\n";
    let snapshot = ProjectSnapshot::capture(
        &fixture.root,
        &[
            document(&helper, "def value: U32\n  1\n"),
            document(&fixture.root, unsaved_root),
        ],
    )
    .expect("a saved rule helper and an ordinary unsaved source");
    let helper = input_identity(&helper).expect("helper identity");
    assert!(!snapshot.overlays.contains_key(&helper));
    assert_eq!(snapshot.sources[&helper].text, "def value: U32\n  1\n");
    let root = input_identity(&fixture.root).expect("root identity");
    assert_eq!(snapshot.overlays[&root].text, unsaved_root);
}

#[test]
fn saved_rule_helper_changes_invalidate_preparation() {
    let fixture = fixture(
        "import ../shared/helper.bend as Local\nimport ../../external/helper.bend as External\n\ndef positive: U32\n  Local.value\n",
    );
    let local = fixture.project.join("shared/helper.bend");
    let external = fixture.directory.path().join("external/helper.bend");
    write(&local, "def value: U32\n  1\n");
    write(&external, "def external: U32\n  1\n");
    let first = ProjectSnapshot::capture(&fixture.root, &[]).expect("capture rule inputs");
    assert!(input_identity(&local).is_some() && input_identity(&external).is_some());
    write(
        &fixture.directory.path().join("external/helper.bend"),
        "def external: U32\n  2\n",
    );
    assert!(
        !first.unchanged(),
        "a saved external rule helper change must invalidate the captured project"
    );
    let second = ProjectSnapshot::capture(&fixture.root, &[]).expect("recapture rule inputs");
    assert_ne!(first.config_digest(), second.config_digest());
    write(
        &fixture.project.join("shared/helper.bend"),
        "def value: U32\n  2\n",
    );
    let third = ProjectSnapshot::capture(&fixture.root, &[]).expect("recapture rule inputs");
    assert_ne!(second.config_digest(), third.config_digest());
}

#[cfg(unix)]
#[test]
fn a_closed_import_keeps_the_project_root_spelling() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("symlink project");
    let real = directory.path().join("real");
    let project = real.join("project");
    write(&project.join("bend-lint.json"), "{\"rules\":{}}");
    write(
        &project.join("main.bend"),
        "import dep.bend as Dep\ndef main: U32\n  Dep.value\n",
    );
    write(&project.join("dep.bend"), "def value: U32\n  1\n");
    let link = directory.path().join("link");
    symlink(&real, &link).expect("create symlinked root");
    let lexical = link.join("project");
    let snapshot = ProjectSnapshot::capture(&lexical.join("main.bend"), &[])
        .expect("capture through the symlinked root");
    let dependency = input_identity(&project.join("dep.bend")).expect("dependency identity");
    assert_eq!(
        snapshot.uris.get(&dependency),
        Some(&vec![
            Url::from_file_path(lexical.join("dep.bend")).expect("logical file URI")
        ]),
        "closed imported findings must use the client's logical project spelling"
    );
}

#[test]
fn an_absolute_local_rule_helper_is_a_fenced_executed_input() {
    let fixture = fixture("def positive: U32\n  0\n");
    let helper = fixture.directory.path().join("external/helper.bend");
    write(&helper, "def value: U32\n  1\n");
    write(
        &fixture.project.join(RULES_DIRECTORY).join("style.bend"),
        &format!(
            "import {} as Helper\n\ndef positive: U32\n  Helper.value\n",
            helper.display()
        ),
    );
    let snapshot =
        ProjectSnapshot::capture(&fixture.root, &[]).expect("absolute local rule import");
    assert!(
        snapshot
            .source(helper.to_str().expect("UTF-8 helper path"))
            .is_some(),
        "an executed helper must resolve as a checked source so its findings are accepted"
    );
    assert!(snapshot.covers_path(&input_identity(&helper).expect("helper identity")));
    let error =
        ProjectSnapshot::capture(&fixture.root, &[document(&helper, "def value: U32\n  2\n")])
            .err()
            .expect("an unsaved executed helper cannot be overlaid");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    write(&helper, "def value: U32\n  3\n");
    assert!(
        !snapshot.unchanged(),
        "a saved absolute helper is a captured and preparation input"
    );
    let second = ProjectSnapshot::capture(&fixture.root, &[]).expect("recapture helpers");
    assert_ne!(snapshot.config_digest(), second.config_digest());
}

#[test]
fn a_bare_import_of_an_external_rule_helper_is_fenced() {
    let fixture =
        fixture("import ../../external/step.bend as Step\n\ndef positive: U32\n  Step.value\n");
    let external = fixture.directory.path().join("external");
    write(
        &external.join("step.bend"),
        "import leaf.bend as Leaf\n\ndef value: U32\n  Leaf.value\n",
    );
    let leaf = external.join("leaf.bend");
    write(&leaf, "def value: U32\n  1\n");
    let error =
        ProjectSnapshot::capture(&fixture.root, &[document(&leaf, "def value: U32\n  2\n")])
            .err()
            .expect("a bare-imported helper is compiled from disk, never overlaid");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    let first = ProjectSnapshot::capture(&fixture.root, &[]).expect("capture bare closure");
    write(&leaf, "def value: U32\n  3\n");
    assert!(!first.unchanged(), "a saved bare helper is a rule input");
    let second = ProjectSnapshot::capture(&fixture.root, &[]).expect("recapture closure");
    assert_ne!(first.config_digest(), second.config_digest());
}

#[test]
fn a_created_missing_absolute_rule_helper_invalidates_the_cached_project() {
    let fixture = fixture("def positive: U32\n  0\n");
    let missing = fixture.directory.path().join("external/later.bend");
    write(
        &fixture.project.join(RULES_DIRECTORY).join("style.bend"),
        &format!(
            "import {} as Later\n\ndef positive: U32\n  Later.value\n",
            missing.display()
        ),
    );
    let first = ProjectSnapshot::capture(&fixture.root, &[]).expect("missing absolute rule import");
    assert!(
        first.covers_path(&missing),
        "an absent executed helper keeps its capturing entry covered for later saves"
    );
    write(&missing, "def value: U32\n  1\n");
    assert!(
        !first.unchanged(),
        "creating a missing executed helper invalidates the cached closure"
    );
    let second = ProjectSnapshot::capture(&fixture.root, &[]).expect("recapture helpers");
    assert_ne!(first.config_digest(), second.config_digest());
}

#[cfg(unix)]
#[test]
fn an_external_rule_helper_alias_retarget_invalidates_the_project() {
    use std::os::unix::fs::symlink;

    let fixture = fixture(
        "import ../../external/helper.bend as Helper\n\ndef positive: U32\n  Helper.value\n",
    );
    let first = fixture.directory.path().join("first");
    let second = fixture.directory.path().join("second");
    write(&first.join("helper.bend"), "def value: U32\n  1\n");
    write(&second.join("helper.bend"), "def value: U32\n  1\n");
    let external = fixture.directory.path().join("external");
    symlink(&first, &external).expect("link the external helper directory");
    let snapshot = ProjectSnapshot::capture(&fixture.root, &[]).expect("symlinked helper");
    fs::remove_file(&external).expect("remove the external helper link");
    symlink(&second, &external).expect("retarget the external helper directory");
    assert!(
        !snapshot.unchanged(),
        "a retargeted helper alias is an input change even when the old bytes are unchanged"
    );
}

#[test]
fn hub_and_base_rule_imports_are_not_local_inputs() {
    let fixture = fixture(
        "import Base\nimport rules-package@1.2.3.4/layout/format.bend as Format\n\ndef positive: U32\n  0\n",
    );
    let snapshot = ProjectSnapshot::capture(&fixture.root, &[]).expect("hub and Base rule imports");
    let phantom = fixture
        .project
        .join("bend-lint-rules/rules-package@1.2.3.4/layout/format.bend");
    assert!(
        snapshot
            .source(phantom.to_str().expect("UTF-8 phantom path"))
            .is_none(),
        "a Hub package body is pinned external context, never a local rule input"
    );
}

#[test]
fn a_trusted_rule_entry_keeps_its_disk_source_and_stays_out_of_overlays() {
    let fixture = fixture("def positive: U32\n  0\n");
    let helper = fixture.project.join("shared/helper.bend");
    write(&helper, "def value: U32\n  1\n");
    let rule = fixture.project.join(RULES_DIRECTORY).join("style.bend");
    let rule_text = "import ../shared/helper.bend as Helper\n\ndef positive: U32\n  Helper.value\n";
    write(&rule, rule_text);
    let snapshot = ProjectSnapshot::capture(
        &rule,
        &[
            document(&rule, rule_text),
            document(&fixture.root, "def main: U32\n  1\n"),
        ],
    )
    .expect("capture a trusted rule file as the lint entry");
    let identity = input_identity(&rule).expect("rule identity");
    assert_eq!(snapshot.sources[&identity].text, rule_text);
    assert!(!snapshot.overlays.contains_key(&identity));
    assert!(
        snapshot
            .source(rule.to_str().expect("UTF-8 rule path"))
            .is_some(),
        "a finding about the checked rule file must resolve to its saved source"
    );
}

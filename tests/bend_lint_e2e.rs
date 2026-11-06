#[path = "support/lsp_client.rs"]
pub mod lsp_client;
mod support;

use bend2_lsp::analysis::LineIndex;
use lsp_client::LspClient;
use process_wrap::tokio::{CommandWrap, KillOnDrop};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::LazyLock,
    time::{Duration, Instant},
};
use support::Must;
use tempfile::tempdir;
use tokio::io::{AsyncRead, AsyncReadExt};
use tower_lsp::lsp_types::TextEdit;
use url::Url;

const ANALYZER_TIMEOUT_MS: u64 = 120_000;

// Prepared at most once per test binary, with the failure cached too: upstream
// has no cross-process single-flight for its built rule program.
static GENUINE_ANALYZER: LazyLock<Result<(PathBuf, Vec<String>), String>> =
    LazyLock::new(setup_genuine_analyzer);

// This is intentionally opt-in: unlike protocol fixtures it executes real,
// trusted project rules and may populate upstream's pinned Bend/BendHub cache.
// BEND2_LINT_TEST_PATH is an absolute installed bend-lint executable, or Bun.
// For Bun, BEND2_LINT_TEST_ARGUMENTS is a JSON string array containing the
// installed or officially shipped src/linter/seam.ts path, not the raw checkout,
// bunx or a package manager. Shipping preserves the published rule API identity.
// The tested upstream schema is standalone 0.3.0 at revision
// c6f31c0c2ef4587368dd6a5443a6d42e22e975fc, with rule API 0.3.0.0. Its npm
// tarball is pinned by bootstrap_bend_lint and matches the published SRI.
#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn genuine_analyzer_matches_its_cli_report_through_saved_stdio_document() {
    let (executable, arguments) = configured_analyzer();
    let directory = tempdir().must_be("real analyzer workspace");
    let project = directory.path().join("genuine analyzer project 🦀");
    fs::create_dir(&project).must_be("create genuine project");
    // Independent expected source: only the four-space body becomes two spaces.
    let source = "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n    Nat.add(x, y)\n";
    let canonical = "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n  Nat.add(x, y)\n";
    let root = project.join("main.bend");
    fs::write(&root, source).must_be("write genuine Bend source");
    fs::write(
        project.join("bend-lint.json"),
        "{\"packages\":{\"bend-lint-rules\":\"0.3.0.0\"},\"rules\":{\"bend-lint-rules/language/unneeded_plus\":{},\"bend-lint-rules/layout/format\":{}}}",
    )
    .must_be("enable genuine upstream rules");
    let report = match std::panic::catch_unwind(|| genuine_report(&executable, &arguments, &root)) {
        Ok(report) => report,
        Err(failure) => {
            retain_failed_analyzer(directory, &root, &executable, &arguments);
            std::panic::resume_unwind(failure);
        }
    };
    let expected = finding(
        &report["findings"],
        "bend-lint-rules/language/unneeded_plus",
    );
    assert_eq!(expected["range"]["start"], json!({"line":2,"character":7}));
    assert_eq!(expected["range"]["end"], json!({"line":2,"character":8}));
    let format = finding(&report["findings"], "bend-lint-rules/layout/format");
    assert_eq!(format["range"]["start"], json!({"line":3,"character":2}));
    let reported_edit = reported_format_edit(format, &root, canonical);

    let lsp_started = Instant::now();
    let mut client = configured_client(&project, &executable, &arguments);
    let document = uri(&root);
    let packet = opened_diagnostics(&mut client, &document, source, lsp_started);
    let diagnostics = &packet["params"]["diagnostics"];
    let actual = finding(diagnostics, "bend-lint-rules/language/unneeded_plus");
    assert_eq!(actual["source"], "bend-lint");
    assert_eq!(actual["message"], expected["message"]);
    assert_eq!(actual["range"], expected["range"]);
    assert_eq!(actual["severity"], 4, "upstream Hint remains LSP Hint");
    let format_actual = finding(diagnostics, "bend-lint-rules/layout/format");
    assert_eq!(format_actual["source"], "bend-lint");
    assert_eq!(format_actual["message"], format["message"]);
    assert_eq!(format_actual["range"], format["range"]);
    assert_eq!(
        format_actual["severity"], 2,
        "upstream Warning remains LSP Warning"
    );
    explicit_user_fix(
        &mut client,
        &root,
        source,
        canonical,
        format_actual,
        reported_edit,
    );
    client.finish();
}

// Linux is the only supported target where the fixture can create a native
// filename whose bytes are not UTF-8: macOS APFS rejects such a name with
// EILSEQ. The editor opens a UTF-8 alias whose identity is that native entry,
// so the genuine report and an explicit edit must stay on the alias spelling
// without a lossy path substitution, a temporary project copy, or dropped
// identity/byte validation.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn genuine_entry_through_utf8_alias_of_non_utf8_filename_matches_cli() {
    use std::{
        ffi::OsString,
        os::unix::{ffi::OsStringExt, fs::symlink},
    };

    let (executable, arguments) = configured_analyzer();
    let directory = tempdir().must_be("non-UTF-8 entry workspace");
    let project = directory.path().join("non-UTF-8 entry 🦀");
    fs::create_dir(&project).must_be("create genuine project");
    let target = project.join(OsString::from_vec(b"entry-\xff.bend".to_vec()));
    let alias = project.join("entry.bend");
    let source = "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n    Nat.add(x, y)\n";
    let formatted = "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n  Nat.add(x, y)\n";
    fs::write(&target, source).must_be("write the non-UTF-8 native entry");
    symlink(&target, &alias).must_be("create the UTF-8 entry alias");
    assert!(
        target.to_str().is_none(),
        "the physical entry name is not UTF-8"
    );
    assert_eq!(
        canonical(&alias),
        canonical(&target),
        "the UTF-8 alias names the same physical entry"
    );
    let saved = fs::read(&target).must_be("read the saved native entry bytes");
    fs::write(
        project.join("bend-lint.json"),
        serde_json::to_vec(&json!({
            "packages":{"bend-lint-rules":"0.3.0.0"},
            "rules":{UNNEEDED_PLUS:{},FORMAT:{}}
        }))
        .must_be("serialize genuine alias-rule config"),
    )
    .must_be("enable genuine rules");

    // The external oracle uses the same published overlay transport as the
    // server: the UTF-8 alias as the entry and an overlay for that alias. It
    // never names the physical non-UTF-8 target lossily and never copies the
    // project; the genuine producer either accepts this lossless view or its
    // exact failure is the upstream limitation.
    let mut overlays = BTreeMap::new();
    overlays.insert(alias.clone(), source.to_owned());
    let report = genuine_overlay_report(&executable, &arguments, &project, &alias, &overlays);
    let expected = finding(&report["findings"], UNNEEDED_PLUS);
    assert_eq!(
        expected["range"]["start"],
        json!({"line":2,"character":7}),
        "the genuine CLI reports the saved alias entry"
    );
    let format = finding(&report["findings"], FORMAT);
    assert_lossless_finding_path(&format["path"], &project, &target);

    let started = Instant::now();
    let mut client = configured_client(&project, &executable, &arguments);
    let document = uri(&alias);
    let packet = opened_diagnostics(&mut client, &document, source, started);
    let diagnostics = &packet["params"]["diagnostics"];
    let actual = finding(diagnostics, UNNEEDED_PLUS);
    assert_eq!(actual["source"], "bend-lint");
    assert_eq!(actual["message"], expected["message"]);
    assert_eq!(actual["range"], expected["range"]);
    assert_eq!(actual["severity"], 4, "upstream Hint remains LSP Hint");
    let format_actual = finding(diagnostics, FORMAT);
    assert_eq!(format_actual["message"], format["message"]);
    assert_eq!(format_actual["range"], format["range"]);
    // The finding keeps the captured native identity while naming the alias
    // spelling, without U+FFFD replacement or a copied project tree.
    assert_lossless_finding_path(&format_actual["data"]["path"], &project, &target);
    assert_eq!(
        fs::read(&target).must_be("saved entry after the check"),
        saved,
        "the check never rewrites or copies the non-UTF-8 entry"
    );

    let response = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":document},"range":format_actual["range"],
            "context":{"diagnostics":[format_actual],"triggerKind":1,
                "only":["quickfix.bend-lint.dangerous"]}
        }),
    );
    let actions = response["result"]
        .as_array()
        .must_be("explicit genuine actions");
    assert_eq!(actions.len(), 1, "only the actual reported dangerous fix");
    let action = &actions[0];
    assert_eq!(action["kind"], "quickfix.bend-lint.dangerous");
    let changes = action["edit"]["documentChanges"]
        .as_array()
        .must_be("versioned alias edit");
    assert_eq!(changes.len(), 1, "the edit targets only the opened alias");
    assert_eq!(changes[0]["textDocument"]["uri"], document);
    assert_eq!(changes[0]["textDocument"]["version"], 1);
    assert_eq!(changes[0]["edits"][0]["newText"], formatted);
    assert_eq!(
        fs::read(&target).must_be("entry after the explicit action"),
        saved,
        "the server never writes the alias or its non-UTF-8 target"
    );
    client.finish();
}

#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn organize_imports_satisfies_genuine_analyzer_and_preserves_other_findings() {
    const IMPORTS: &str = "bend-lint-rules/layout/imports";
    const CANONICAL_PATH: &str = "bend-lint-rules/layout/canon_import_path";
    let (executable, arguments) = configured_analyzer();
    let directory = tempdir().must_be("real import ordering workspace");
    let project = directory.path().join("import ordering 🦀");
    fs::create_dir_all(project.join("support")).must_be("create imported modules");
    for name in ["alpha", "zeta"] {
        fs::write(
            project.join("support").join(format!("{name}.bend")),
            "import Base\n\ndef one() -> U32:\n  1\n",
        )
        .must_be("write real imported module");
    }
    let source = concat!(
        "import support/zeta.bend as zeta\n",
        "import Base\n",
        "import ./support/alpha.bend as Alpha\n",
        "\ndef main() -> U32:\n",
        "  (zeta.one() + Alpha.one() : U32)\n",
    );
    let root = project.join("main.bend");
    fs::write(&root, source).must_be("write unsorted imports");
    fs::write(
        project.join("bend-lint.json"),
        serde_json::to_vec(&json!({
            "packages":{"bend-lint-rules":"0.3.0.0"},
            "rules":{IMPORTS:{},CANONICAL_PATH:{}}
        }))
        .must_be("serialize real import rules"),
    )
    .must_be("enable genuine import rules");
    let before_cli = genuine_report(&executable, &arguments, &root);
    finding(&before_cli["findings"], IMPORTS);
    let started = Instant::now();
    let mut client = configured_client(&project, &executable, &arguments);
    let document = uri(&root);
    let before_lsp = opened_diagnostics(&mut client, &document, source, started);
    finding(&before_lsp["params"]["diagnostics"], IMPORTS);
    let organized = organize_imports_source(&mut client, &root, source, 1)
        .must_be("organize imports action for the reported order");
    fs::write(&root, &organized).must_be("user applies the import action");
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument":{"uri":document,"version":2},
            "contentChanges":[{"text":organized}]
        }),
    );
    // This versioned read also acknowledges the editor's preceding mutation.
    assert!(
        organize_imports_source(&mut client, &root, &organized, 2).is_none(),
        "organizing the edited result must be idempotent"
    );
    // The versioned read acknowledged the edit. A fresh save must not wait for
    // an unrelated compiler publication or background index refresh.
    client.notify(
        "textDocument/didSave",
        json!({"textDocument":{"uri":document}}),
    );
    // The still-noncanonical path is an independent finding. Its arrival at
    // revision 2 proves lint completed, rather than observing an interim clear.
    let after_lsp = wait_for_lint_diagnostics(&mut client, &document, started, 2);
    let after_cli = genuine_report(&executable, &arguments, &root);
    for findings in [&after_cli["findings"], &after_lsp["params"]["diagnostics"]] {
        assert!(
            findings
                .as_array()
                .must_be("genuine findings after organizing")
                .iter()
                .all(|finding| finding["code"] != IMPORTS),
            "Organize imports must satisfy the genuine import-order rule: {findings}"
        );
        finding(findings, CANONICAL_PATH);
    }
    client.finish();
}

const UNNEEDED_PLUS: &str = "bend-lint-rules/language/unneeded_plus";
const UNUSED_RESULT: &str = "bend-lint-rules/performance/unused_result";
const FORMAT: &str = "bend-lint-rules/layout/format";

fn unused_result_sources() -> (&'static str, &'static str, &'static str) {
    let main = concat!(
        "import Base\n",
        "import dep.bend as Dep\n",
        "\n",
        "def f(n: Nat, acc: U32) -> U32:\n",
        "  match n:\n",
        "    case 0n:\n",
        "      acc\n",
        "    case 1n+m:\n",
        "      xs = Dep.build(3n, [])\n",
        "      f(m, acc)\n",
        "\n",
        "def g(+x: Nat, y: Nat) -> Nat:\n",
        "    Nat.add(x, y)\n",
    );
    let dep = concat!(
        "import Base\n",
        "\n",
        "def build(n: Nat, acc: List<&2, U32>) -> List<&2, U32>:\n",
        "  match n:\n",
        "    case 0n:\n",
        "      acc\n",
        "    case 1n+m:\n",
        "      build(m, 1 <> acc)\n",
    );
    let scalar = concat!(
        "import Base\n",
        "\n",
        "def build(n: Nat, acc: List<&2, U32>) -> U32:\n",
        "  0\n",
    );
    (main, dep, scalar)
}

#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn unsaved_imported_types_change_genuine_semantic_findings_without_disk() {
    let (executable, arguments) = configured_analyzer();
    let (main_source, dep_source, scalar_dep) = unused_result_sources();
    let directory = tempdir().must_be("genuine imported-type workspace");
    let project = directory.path().join("imported types 🦀");
    fs::create_dir(&project).must_be("create genuine project");
    let main = project.join("main.bend");
    let dep = project.join("dep.bend");
    fs::write(&main, main_source).must_be("write saved entry");
    fs::write(&dep, dep_source).must_be("write saved integral dependency");
    fs::write(
        project.join("bend-lint.json"),
        serde_json::to_vec(&json!({
            "packages":{"bend-lint-rules":"0.3.0.0"},
            "rules":{UNUSED_RESULT:{},UNNEEDED_PLUS:{},FORMAT:{}}
        }))
        .must_be("serialize genuine semantic rule config"),
    )
    .must_be("enable genuine semantic rules");
    let disk_report = genuine_report(&executable, &arguments, &main);
    assert_eq!(
        disk_report["skipped"],
        json!([]),
        "saved fixture must type-check"
    );
    let disk_unused = finding(&disk_report["findings"], UNUSED_RESULT);
    let disk_range = disk_unused["range"].clone();
    // Only the imported type changes. The costly unused-result finding must
    // disappear while an independent checker-backed finding remains.
    let mut overlays = BTreeMap::new();
    overlays.insert(canonical(&dep), scalar_dep.to_owned());
    let overlay_report =
        genuine_overlay_report(&executable, &arguments, &project, &main, &overlays);
    assert_eq!(
        overlay_report["skipped"],
        json!([]),
        "the scalar overlay must pass checking, not suppress semantic rules"
    );
    assert!(
        overlay_report["findings"]
            .as_array()
            .must_be("genuine overlay findings")
            .iter()
            .all(|finding| finding["code"] != UNUSED_RESULT),
        "the unsaved scalar import genuinely changes the semantic result: {overlay_report}"
    );
    finding(&overlay_report["findings"], UNNEEDED_PLUS);
    finding(&overlay_report["findings"], FORMAT);
    let started = Instant::now();
    let mut client = configured_client(&project, &executable, &arguments);
    let document = uri(&main);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":main_source
        }}),
    );
    let baseline = wait_for_lint_diagnostics(&mut client, &document, started, 1);
    assert_eq!(
        finding(&baseline["params"]["diagnostics"], UNUSED_RESULT)["range"],
        disk_range,
        "the live saved analysis matches its genuine CLI report"
    );
    // Only the imported buffer is dirty: its type changes from a built list to
    // a scalar, so the semantic rule must stop reporting the unused binding.
    let dep_document = uri(&dep);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":dep_document,"languageId":"bend","version":1,"text":scalar_dep
        }}),
    );
    let updated = wait_for_root_packet(&mut client, &document, 1, |packet| {
        has_code(packet, UNNEEDED_PLUS)
            && has_code(packet, FORMAT)
            && !has_code(packet, UNUSED_RESULT)
    });
    assert!(!has_code(&updated, UNUSED_RESULT));
    assert_eq!(
        fs::read_to_string(&dep).must_be("disk dependency unchanged"),
        dep_source
    );
    client.finish();
}

#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn virtual_nonexistent_import_analyses_without_creating_disk_files() {
    let (executable, arguments) = configured_analyzer();
    let (main_source, dep_source, _) = unused_result_sources();
    let main_source = main_source.replace("dep.bend", "new/dep.bend");
    let directory = tempdir().must_be("virtual genuine import workspace");
    let project = directory.path().join("virtual import 🦀");
    fs::create_dir(&project).must_be("create genuine project");
    let main = project.join("main.bend");
    fs::write(&main, &main_source).must_be("write saved entry");
    fs::write(
        project.join("bend-lint.json"),
        serde_json::to_vec(&json!({
            "packages":{"bend-lint-rules":"0.3.0.0"},
            "rules":{UNUSED_RESULT:{},UNNEEDED_PLUS:{}}
        }))
        .must_be("serialize genuine virtual rule config"),
    )
    .must_be("enable genuine semantic rules");
    let mut client = configured_client(&project, &executable, &arguments);
    let document = uri(&main);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":main_source
        }}),
    );
    // The imported file exists only in editor memory, under a directory that
    // does not exist on disk.
    let virtual_dep = project.join("new/dep.bend");
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":uri(&virtual_dep),"languageId":"bend","version":1,"text":dep_source
        }}),
    );
    let packet = wait_for_root_packet(&mut client, &document, 1, |packet| {
        has_code(packet, UNUSED_RESULT)
    });
    assert_eq!(
        finding(&packet["params"]["diagnostics"], UNUSED_RESULT)["range"]["start"],
        json!({"line":8,"character":6})
    );
    assert!(
        !project.join("new").exists(),
        "a virtual import must not create its missing directory"
    );
    assert!(!virtual_dep.exists());
    client.finish();
}

#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn virtual_nonexistent_entry_analyses_without_creating_disk_files() {
    let (executable, arguments) = configured_analyzer();
    let directory = tempdir().must_be("virtual genuine entry workspace");
    let project = directory.path().join("virtual entry 🦀");
    fs::create_dir(&project).must_be("create genuine project");
    fs::write(
        project.join("bend-lint.json"),
        serde_json::to_vec(&json!({
            "packages":{"bend-lint-rules":"0.3.0.0"},
            "rules":{UNNEEDED_PLUS:{}}
        }))
        .must_be("serialize genuine virtual entry config"),
    )
    .must_be("enable genuine semantic rule");
    let entry = project.join("new/main.bend");
    let document = uri(&entry);
    let source = "import Base\n\ndef main(+x: U32) -> U32:\n  x\n";
    let mut client = configured_client(&project, &executable, &arguments);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":source
        }}),
    );
    let packet = wait_for_root_packet(&mut client, &document, 1, |packet| {
        has_code(packet, UNNEEDED_PLUS)
    });
    assert_eq!(
        finding(&packet["params"]["diagnostics"], UNNEEDED_PLUS)["range"],
        json!({
            "start":{"line":2,"character":10},
            "end":{"line":2,"character":11}
        })
    );
    assert!(!project.join("new").exists());
    assert!(!entry.exists());
    client.finish();
}

// 0.3.0 validates configured `entries` against the overlay view (upstream #5),
// so an unsaved entry named in bend-lint.json is analyzed without a disk copy.
#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn configured_overlay_only_entry_is_analyzed_without_a_disk_file() {
    let (executable, arguments) = configured_analyzer();
    let directory = tempdir().must_be("configured virtual entry workspace");
    let project = directory.path().join("configured virtual entry 🦀");
    fs::create_dir_all(project.join("src")).must_be("create the entry folder");
    fs::write(
        project.join("bend-lint.json"),
        serde_json::to_vec(&json!({
            "entries":["src/main.bend"],
            "packages":{"bend-lint-rules":"0.3.0.0"},
            "rules":{UNNEEDED_PLUS:{}}
        }))
        .must_be("serialize configured virtual entry config"),
    )
    .must_be("enable genuine rule");
    let entry = project.join("src/main.bend");
    let source = "import Base\n\ndef main(+x: U32) -> U32:\n  x\n";
    assert!(!entry.exists(), "the configured entry starts virtual");
    // TempDir may use a Windows 8.3 alias. The native runner and Bun must receive
    // one physical source view; keep the original editor URI below to exercise
    // the provider independently of the CLI oracle's path normalization.
    let oracle_project = canonical_cli_path(&project);
    let oracle_entry = oracle_project.join("src/main.bend");
    let mut overlays = BTreeMap::new();
    overlays.insert(oracle_entry.clone(), source.to_owned());
    let report = genuine_overlay_report(
        &executable,
        &arguments,
        &oracle_project,
        &oracle_entry,
        &overlays,
    );
    finding(&report["findings"], UNNEEDED_PLUS);
    let started = Instant::now();
    let mut client = configured_client(&project, &executable, &arguments);
    let document = uri(&entry);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":source
        }}),
    );
    let packet = wait_for_lint_diagnostics(&mut client, &document, started, 1);
    assert_eq!(
        finding(&packet["params"]["diagnostics"], UNNEEDED_PLUS)["range"],
        json!({
            "start":{"line":2,"character":10},
            "end":{"line":2,"character":11}
        })
    );
    assert!(
        !entry.exists(),
        "a configured overlay-only entry never creates a disk file"
    );
    client.finish();
}

// Upstream 0.3.0 follows every local import when it keys a rules build
// (issue #7). This scenario proves it through the real stdio provider: each
// helper change is prepared by the genuine CLI first (oracle and cache warmup),
// then a watched-file event must make the LSP publish the new rule messages
// from the same open, unsaved source. It runs both a bare and a `/`-rooted
// absolute helper import; 0.2.1 keyed the build without either and kept the
// stale runner. The shipped package is copied to the project's node_modules,
// as a per-project install lays it out, because Bend imports project rules only
// by a plain relative path from the build.
#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn custom_rule_build_cache_follows_bare_and_absolute_helper_imports() {
    let (executable, arguments) = configured_analyzer();
    let (_directory, project, arguments) = custom_rule_workspace(&arguments);
    let rules = project.join("bend-lint-rules");
    let entry = project.join("main.bend");
    let bare_helper = rules.join("bare_helper.bend");
    #[cfg(unix)]
    let absolute_helper = rules.join("absolute_helper.bend");

    // Every helper change is prepared by the genuine CLI first: that report is
    // the independent oracle, and the native runner it builds is reused by the
    // provider's preparation after the watcher event instead of being rebuilt.
    let report_path = project.join("cache-report.json");
    let document = uri(&entry);
    let source = fs::read_to_string(&entry).must_be("read the saved entry");
    #[cfg(unix)]
    let initial_absolute = Some(1);
    #[cfg(not(unix))]
    let initial_absolute: Option<u32> = None;
    #[cfg(unix)]
    let changes: Vec<(u32, Option<u32>, &Path)> = vec![
        (2, initial_absolute, bare_helper.as_path()),
        (2, Some(2), absolute_helper.as_path()),
    ];
    #[cfg(not(unix))]
    let changes: Vec<(u32, Option<u32>, &Path)> =
        vec![(2, initial_absolute, bare_helper.as_path())];

    // Each CLI prepare builds the runner before the watcher event, so the
    // provider's own preparation reuses that build instead of duplicating it.
    fs::write(&bare_helper, helper_source(1)).must_be("write the initial bare helper");
    #[cfg(unix)]
    fs::write(&absolute_helper, helper_source(1)).must_be("write the initial absolute helper");
    let report = prepare_custom_rules(&executable, &arguments, &project, &report_path);
    eprintln!("custom-rule cache initial: prepared {}", report["program"]);
    let findings = genuine_report(&executable, &arguments, &entry);
    assert_cli_probes(&findings["findings"], 1, initial_absolute);

    let started = Instant::now();
    let mut client = configured_client(&project, &executable, &arguments);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":source
        }}),
    );
    let packet = wait_for_lint_diagnostics(&mut client, &document, started, 1);
    assert_lsp_probes(&packet["params"]["diagnostics"], 1, initial_absolute);

    for (bare, absolute, changed) in changes {
        fs::write(&bare_helper, helper_source(bare)).must_be("update the bare helper");
        #[cfg(unix)]
        {
            if let Some(version) = absolute {
                fs::write(&absolute_helper, helper_source(version))
                    .must_be("update the absolute helper");
            }
        }
        #[cfg(not(unix))]
        assert!(absolute.is_none(), "absolute imports are Unix-only");
        let report = prepare_custom_rules(&executable, &arguments, &project, &report_path);
        eprintln!(
            "custom-rule cache bare={bare} absolute={absolute:?}: prepared {}",
            report["program"]
        );
        let findings = genuine_report(&executable, &arguments, &entry);
        assert_cli_probes(&findings["findings"], bare, absolute);
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":uri(changed),"type":2}]}),
        );
        let expected_bare = format!("helper v{bare}");
        let expected_absolute = absolute.map(|version| format!("helper v{version}"));
        let packet = wait_for_root_packet(&mut client, &document, 1, move |packet| {
            let diagnostics = &packet["params"]["diagnostics"];
            code_message(diagnostics, "bare_probe").as_deref() == Some(expected_bare.as_str())
                && match &expected_absolute {
                    Some(expected) => {
                        code_message(diagnostics, "absolute_probe").as_deref()
                            == Some(expected.as_str())
                    }
                    None => code_message(diagnostics, "absolute_probe").is_none(),
                }
        });
        assert_lsp_probes(&packet["params"]["diagnostics"], bare, absolute);
    }
    assert_eq!(
        fs::read_to_string(&entry).must_be("saved entry after live updates"),
        source,
        "the open source is never changed or saved by helper edits"
    );
    client.finish();
}

fn custom_rule_workspace(arguments: &[String]) -> (tempfile::TempDir, PathBuf, Vec<String>) {
    let shipped_seam = arguments
        .first()
        .must_be("this scenario needs the shipped seam (Bun with BEND2_LINT_TEST_ARGUMENTS)");
    let shipped_seam = shipped_seam.as_str();
    let shipped_seam = Path::new(shipped_seam);
    assert!(
        shipped_seam.is_absolute(),
        "the shipped seam path must be absolute"
    );
    let linter = shipped_seam.parent().must_be("shipped seam folder");
    let shipped = linter
        .parent()
        .and_then(Path::parent)
        .must_be("shipped package root");
    assert!(shipped.join("package.json").is_file());
    assert!(shipped.join("src/linter/cli.bend").is_file());
    // Bend accepts an absolute import only as a `/`-rooted path of plain names,
    // so the Unix project lives in a plain absolute folder; bare imports need no
    // such spelling.
    #[cfg(unix)]
    let directory = tempfile::Builder::new()
        .prefix("bend-lint-cache-")
        .tempdir_in("/tmp")
        .must_be("plain absolute custom-rule workspace");
    #[cfg(not(unix))]
    let directory = tempdir().must_be("custom-rule workspace");
    let project = directory.path().join("project");
    fs::create_dir(&project).must_be("create custom-rule project");
    // Construct the CLI module and rule paths from one physical root: Windows
    // temporary directories may otherwise retain an 8.3 alias on just one side.
    let project = canonical_cli_path(&project);
    let installed = project.join("node_modules/bend-lint");
    copy_shipped_package(shipped, &installed).must_be("copy the shipped package into the project");
    let seam = installed.join("src/linter/seam.ts");
    let arguments = vec![seam.to_string_lossy().into_owned()];

    let rules = project.join("bend-lint-rules");
    fs::create_dir(&rules).must_be("create the project rules folder");
    let entry = project.join("main.bend");
    fs::write(&entry, "import Base\n\ndef main() -> U32:\n  1\n").must_be("write the linted entry");
    fs::write(
        rules.join("bare_probe.bend"),
        probe_rule("import bare_helper.bend as Helper"),
    )
    .must_be("write the bare-import probe rule");
    #[cfg(unix)]
    {
        let absolute_helper = rules.join("absolute_helper.bend");
        let written = absolute_helper.to_string_lossy().replace('\\', "/");
        assert!(
            !written.chars().any(char::is_whitespace),
            "Bend import paths cannot contain whitespace: {written}"
        );
        fs::write(
            rules.join("absolute_probe.bend"),
            probe_rule(&format!("import {written} as Helper")),
        )
        .must_be("write the absolute-import probe rule");
    }
    let mut configured = serde_json::Map::new();
    configured.insert("bare_probe".to_owned(), json!("warning"));
    #[cfg(unix)]
    {
        configured.insert("absolute_probe".to_owned(), json!("warning"));
    }
    fs::write(
        project.join("bend-lint.json"),
        serde_json::to_vec(&json!({"rules": Value::Object(configured)}))
            .must_be("serialize custom-rule config"),
    )
    .must_be("write custom-rule config");
    (directory, project, arguments)
}

fn copy_shipped_package(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_shipped_package(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn helper_source(version: u32) -> String {
    format!("import Base\n\ndef message() -> String:\n  \"helper v{version}\"\n")
}

fn probe_rule(import: &str) -> String {
    format!(
        "import Base\nimport bend-lint-api@0.3.0.0/linter/api/impl.bend as LintImpl\nimport bend-lint-api@0.3.0.0/linter/api/spec.bend as Lint\n{import}\n\ndef run(+here: Lint.Here) -> Lint.Ask(List<&2, Lint.Finding>):\n  Lint.done(\n    [\n      Lint.Finding{{\n        Lint.Warning{{}},\n        Lint.Span{{Lint.Here.path(here), 0, 1}},\n        Helper.message(),\n        []\n      }}\n    ]\n  )\n\ndef rule() -> Lint.Rule:\n  Lint.Rule{{Lint.Needs{{False{{}}, False{{}}}}, [], Lint.OnFile{{run}}}}\n"
    )
}

fn prepare_custom_rules(
    executable: &Path,
    arguments: &[String],
    project: &Path,
    report_path: &Path,
) -> Value {
    let (status, stdout, stderr) =
        run_prepare_contained(executable, arguments, project, report_path)
            .must_be("prepare the custom rules");
    assert!(
        status.success(),
        "custom-rule preparation failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        stdout.is_empty(),
        "custom-rule preparation wrote unexpected stdout"
    );
    let report: Value =
        serde_json::from_slice(&fs::read(report_path).must_be("read the custom-rule report"))
            .must_be("parse the custom-rule report");
    assert_eq!(
        report["bendLint"], "0.3.0",
        "genuine 0.3.0 report: {report}"
    );
    assert_eq!(report["exitCode"], 0);
    if env::var("BEND2_LINT_REQUIRE_NATIVE").is_ok_and(|value| value == "1" || value == "true") {
        assert_eq!(
            report["backend"], "native",
            "the native custom-rule build: {report}"
        );
    }
    report
}

fn code_message(findings: &Value, code: &str) -> Option<String> {
    findings
        .as_array()?
        .iter()
        .find(|finding| finding["code"] == code)
        .and_then(|finding| finding["message"].as_str())
        .map(str::to_owned)
}

fn probe_range() -> Value {
    json!({"start":{"line":0,"character":0},"end":{"line":0,"character":1}})
}

fn assert_cli_probes(findings: &Value, bare: u32, absolute: Option<u32>) {
    let bare_finding = finding(findings, "bare_probe");
    assert_eq!(bare_finding["message"], format!("helper v{bare}"));
    assert_eq!(bare_finding["severity"], "warning");
    assert_eq!(bare_finding["range"], probe_range());
    match absolute {
        Some(version) => {
            let absolute_finding = finding(findings, "absolute_probe");
            assert_eq!(absolute_finding["message"], format!("helper v{version}"));
            assert_eq!(absolute_finding["severity"], "warning");
            assert_eq!(absolute_finding["range"], probe_range());
        }
        None => assert!(
            code_message(findings, "absolute_probe").is_none(),
            "no absolute-import probe rule outside Unix"
        ),
    }
}

fn assert_lsp_probes(diagnostics: &Value, bare: u32, absolute: Option<u32>) {
    let bare_finding = finding(diagnostics, "bare_probe");
    assert_eq!(bare_finding["source"], "bend-lint");
    assert_eq!(bare_finding["message"], format!("helper v{bare}"));
    assert_eq!(bare_finding["severity"], 2, "Warning stays LSP Warning");
    assert_eq!(bare_finding["range"], probe_range());
    match absolute {
        Some(version) => {
            let absolute_finding = finding(diagnostics, "absolute_probe");
            assert_eq!(absolute_finding["source"], "bend-lint");
            assert_eq!(absolute_finding["message"], format!("helper v{version}"));
            assert_eq!(absolute_finding["severity"], 2, "Warning stays LSP Warning");
            assert_eq!(absolute_finding["range"], probe_range());
        }
        None => assert!(
            code_message(diagnostics, "absolute_probe").is_none(),
            "no absolute-import probe rule outside Unix"
        ),
    }
}

#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn unsaved_crlf_non_bmp_range_and_versioned_fix_match_the_genuine_cli() {
    let (executable, arguments) = configured_analyzer();
    let directory = tempdir().must_be("genuine UTF-16 workspace");
    let project = directory.path().join("unicode ranges 🦀");
    fs::create_dir(&project).must_be("create genuine project");
    let canonical_source = "import Base\r\n\r\ndef main() -> String:\r\n  \"😀\" ++ \"x\"\r\n";
    let unsaved = "import Base\r\n\r\ndef main() -> String:\r\n  \"😀\"++\"x\"\r\n";
    let main = project.join("main.bend");
    fs::write(&main, canonical_source).must_be("write canonical CRLF entry");
    fs::write(
        project.join("bend-lint.json"),
        serde_json::to_vec(&json!({
            "packages":{"bend-lint-rules":"0.3.0.0"},
            "rules":{FORMAT:{"endOfLine":"crlf"}}
        }))
        .must_be("serialize genuine format config"),
    )
    .must_be("enable genuine format rule");
    let mut overlays = BTreeMap::new();
    overlays.insert(canonical(&main), unsaved.to_owned());
    let report = genuine_overlay_report(&executable, &arguments, &project, &main, &overlays);
    let reported = finding(&report["findings"], FORMAT);
    // 0.3.0 skips the CR column before an LF (upstream #6), so the genuine JSON
    // range ends at the last editable unit instead of between CR and LF.
    assert_eq!(
        reported["range"]["end"],
        json!({"line":3,"character":11}),
        "the genuine CRLF boundary is already LSP-representable"
    );
    let started = Instant::now();
    let mut client = configured_client(&project, &executable, &arguments);
    let document = uri(&main);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":canonical_source
        }}),
    );
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":document,"version":2},
        "contentChanges":[{"text":unsaved}]}),
    );
    let packet = wait_for_lint_diagnostics(&mut client, &document, started, 2);
    let format = finding(&packet["params"]["diagnostics"], FORMAT);
    assert_eq!(
        format["range"]["start"],
        json!({"line":3,"character":6}),
        "the character after the non-BMP emoji is a UTF-16 offset"
    );
    assert_eq!(format["codeDescription"]["href"], rule_docs(FORMAT));
    assert_eq!(
        format["range"]["end"],
        json!({"line":3,"character":11}),
        "the provider publishes the same representable CRLF boundary; edits below remain byte-exact"
    );
    assert_eq!(format["message"], reported["message"]);
    assert_eq!(reported["docs"], rule_docs(FORMAT));
    let actions = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":document},"range":format["range"],
            "context":{"diagnostics":[format],"triggerKind":1,
                "only":["quickfix.bend-lint.dangerous"]}
        }),
    )["result"]
        .as_array()
        .must_be("genuine versioned actions")
        .clone();
    assert_eq!(actions.len(), 1);
    let changes = actions[0]["edit"]["documentChanges"]
        .as_array()
        .must_be("versioned genuine changes");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["textDocument"]["version"], 2);
    let fixes = reported["fixes"]
        .as_array()
        .must_be("reported genuine fixes");
    let edits = fixes[0]["edits"]
        .as_array()
        .must_be("reported genuine edits");
    assert_eq!(
        changes[0]["edits"]
            .as_array()
            .must_be("offered edits")
            .len(),
        edits.len()
    );
    for (offered, expected) in changes[0]["edits"]
        .as_array()
        .must_be("offered edits")
        .iter()
        .zip(edits)
    {
        assert_eq!(offered["range"], expected["range"]);
        assert_eq!(offered["newText"], expected["text"]);
    }
    assert_eq!(
        fs::read_to_string(&main).must_be("disk remains canonical CRLF"),
        canonical_source
    );
    client.finish();
}

#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn genuine_partial_checks_expose_skips_without_fake_diagnostics() {
    let (executable, arguments) = configured_analyzer();
    let directory = tempdir().must_be("genuine skipped workspace");
    let project = directory.path().join("skipped checks 🦀");
    fs::create_dir(&project).must_be("create genuine project");
    let main_source = concat!(
        "import Base\n",
        "import dep.bend as Dep\n",
        "\n",
        "def main(+x: U32) -> U32:\n",
        "  x\n",
    );
    let saved_dep = "import Base\n\ndef value() -> U32:\n  1\n";
    let broken_dep = "def broken(:\n";
    let main = project.join("main.bend");
    let dep = project.join("dep.bend");
    fs::write(&main, main_source).must_be("write saved entry");
    fs::write(&dep, saved_dep).must_be("write valid saved dependency");
    fs::write(
        project.join("bend-lint.json"),
        serde_json::to_vec(&json!({
            "packages":{"bend-lint-rules":"0.3.0.0"},
            "rules":{UNNEEDED_PLUS:{}}
        }))
        .must_be("serialize genuine skip rule config"),
    )
    .must_be("enable genuine rule");
    let mut overlays = BTreeMap::new();
    overlays.insert(canonical(&dep), broken_dep.to_owned());
    let report = genuine_overlay_report(&executable, &arguments, &project, &main, &overlays);
    let skipped = report["skipped"]
        .as_array()
        .must_be("genuine skipped reasons");
    assert!(
        skipped.iter().any(|skip| skip["reason"] == "unloaded"),
        "the real analyzer reports the unloaded input: {report}"
    );
    let started = Instant::now();
    let mut client = configured_client(&project, &executable, &arguments);
    let document = uri(&main);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":main_source
        }}),
    );
    wait_for_lint_diagnostics(&mut client, &document, started, 1);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":uri(&dep),"languageId":"bend","version":1,"text":broken_dep
        }}),
    );
    let deadline = Instant::now() + Duration::from_secs(150);
    let status = loop {
        let response = client.request(
            "workspace/executeCommand",
            json!({"command":"bend2.lint.status","arguments":[{"uri":document}]}),
        );
        let status = response["result"].clone();
        if status["state"] == "ready" && status["partial"] == true {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the genuine partial result did not arrive: {status}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status["state"], "ready", "{status}");
    assert!(status["skippedChecks"].as_u64().must_be("skipped count") >= 1);
    assert!(
        status["reason"]
            .as_str()
            .must_be("partial reason")
            .contains("skipped check")
    );
    let notice = client
        .receive_matching(Duration::from_secs(10), |message| {
            message["method"] == "window/logMessage"
                && message["params"]["message"].as_str().is_some_and(|text| {
                    text.contains("partially checked") && text.contains("unloaded")
                })
        })
        .must_be("the real skip reasons are logged operationally");
    assert!(notice["params"]["message"].is_string());
    assert_eq!(
        fs::read_to_string(&dep).must_be("disk dependency unchanged"),
        saved_dep
    );
    client.finish();
}

#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn genuine_structured_project_errors_are_attributable_without_diagnostics() {
    let (executable, arguments) = configured_analyzer();
    let directory = tempdir().must_be("genuine error workspace");
    let project = directory.path().join("structured errors 🦀");
    fs::create_dir(&project).must_be("create genuine project");
    let main = project.join("main.bend");
    fs::write(&main, "import Base\n\ndef main() -> U32:\n  1\n").must_be("write saved entry");
    fs::write(
        project.join("bend-lint.json"),
        "{\"packages\":{\"bend-lint-rules\":\"0.3.0.0\"},\"rules\":42}",
    )
    .must_be("write genuinely invalid rule settings");
    let mut client = configured_client(&project, &executable, &arguments);
    let document = uri(&main);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,
            "text":"import Base\n\ndef main() -> U32:\n  1\n"
        }}),
    );
    let failure = client
        .receive_matching(Duration::from_secs(150), |message| {
            matches!(
                message["method"].as_str(),
                Some("window/logMessage" | "window/showMessage")
            ) && message["params"]["message"]
                .as_str()
                .is_some_and(|text| text.contains("bend-lint.json"))
        })
        .must_be("the genuine structured error is attributable");
    assert!(
        failure["params"]["message"]
            .as_str()
            .must_be("structured error message")
            .contains("rules"),
        "the upstream message is preserved: {failure}"
    );
    let status = client.request(
        "workspace/executeCommand",
        json!({"command":"bend2.lint.status","arguments":[{"uri":document}]}),
    )["result"]
        .clone();
    assert_eq!(status["state"], "failed", "{status}");
    assert!(
        client
            .receive_matching(Duration::from_millis(350), |message| {
                lint_packet(message, &document)
            })
            .is_none(),
        "fatal analyzer errors never become source diagnostics"
    );
    assert_eq!(
        fs::read_to_string(&main).must_be("disk entry unchanged"),
        "import Base\n\ndef main() -> U32:\n  1\n"
    );
    client.finish();
}

// The analyzer is the published 0.3.0 CLI with its matching 0.3.0.0 rule
// package, so an unsaved buffer must reach analysis instead of the saved copy.
// The saved source is formatted and still carries an unneeded unary plus; only
// the unsaved buffer removes the plus and introduces four-space indentation.
#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn unsaved_change_republishes_genuine_findings_without_touching_disk() {
    const UNNEEDED_PLUS: &str = "bend-lint-rules/language/unneeded_plus";
    const FORMAT: &str = "bend-lint-rules/layout/format";
    let (executable, arguments) = configured_analyzer();
    let directory = tempdir().must_be("real unsaved overlay workspace");
    let project = directory.path().join("unsaved overlay 🦀");
    fs::create_dir(&project).must_be("create genuine project");
    let saved = "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n  Nat.add(x, y)\n";
    let unsaved = "import Base\n\ndef f(x: Nat, y: Nat) -> Nat:\n    Nat.add(x, y)\n";
    let root = project.join("main.bend");
    fs::write(&root, saved).must_be("write genuine saved source");
    fs::write(
        project.join("bend-lint.json"),
        "{\"packages\":{\"bend-lint-rules\":\"0.3.0.0\"},\"rules\":{\"bend-lint-rules/language/unneeded_plus\":{},\"bend-lint-rules/layout/format\":{}}}",
    )
    .must_be("enable matching 0.3.0 genuine upstream rules");
    let started = Instant::now();
    let mut client = configured_client(&project, &executable, &arguments);
    let document = uri(&root);
    // Opening an unsaved-capable buffer schedules analysis of its current text.
    // There is deliberately no pre-save quiet window: a warmed genuine runner may
    // publish this version 1 packet before any didSave.
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":saved
        }}),
    );
    let saved_packet = wait_for_lint_diagnostics(&mut client, &document, started, 1);
    assert_eq!(saved_packet["params"]["version"], 1);
    let saved_findings = &saved_packet["params"]["diagnostics"];
    finding(saved_findings, UNNEEDED_PLUS);
    assert!(
        saved_findings
            .as_array()
            .must_be("saved diagnostics")
            .iter()
            .all(|finding| finding["code"] != FORMAT),
        "the saved source is already formatted: {saved_findings}"
    );
    // The editor sends the exact unsaved buffer, leaving the stale saved copy on
    // disk. Analysis must observe the unsaved text, not the saved text.
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument":{"uri":document,"version":2},
            "contentChanges":[{"text":unsaved}]
        }),
    );
    // The saved run above already primed the genuine CLI, rules and compiler.
    // This is a warm-path publication requirement, not a cold-build allowance.
    let unsaved_packet = wait_for_lint_diagnostics_within(
        &mut client,
        &document,
        started,
        2,
        Duration::from_secs(10),
    );
    let findings = &unsaved_packet["params"]["diagnostics"];
    assert_eq!(
        finding(findings, FORMAT)["range"]["start"],
        json!({"line":3,"character":2}),
        "the unsaved indentation, not the formatted saved copy, was analyzed"
    );
    assert!(
        findings
            .as_array()
            .must_be("unsaved diagnostics")
            .iter()
            .all(|finding| finding["code"] != UNNEEDED_PLUS),
        "removing the unary plus in the unsaved buffer must remove its saved-only finding: {findings}"
    );
    assert_eq!(
        fs::read_to_string(&root).must_be("disk source remains untouched"),
        saved,
        "unsaved analysis must never write overlay text to disk"
    );
    client.finish();
}

fn retain_failed_analyzer(
    directory: tempfile::TempDir,
    root: &Path,
    executable: &Path,
    arguments: &[String],
) {
    let Some(output) = env::var_os("BEND2_LINT_TEST_FAILURE_DIR").filter(|value| !value.is_empty())
    else {
        return;
    };
    let output = PathBuf::from(output);
    fs::create_dir_all(&output).must_be("create analyzer failure evidence directory");
    let retained = directory.keep();
    let project = root.parent().must_be("retained genuine project");
    for name in ["main.bend", "bend-lint.json"] {
        fs::copy(project.join(name), output.join(name)).must_be("retain genuine fixture input");
    }
    let replay = json!({
        "executable": executable,
        "arguments": arguments,
        "entry": root,
        "cwd": project,
        "retained_root": retained,
    });
    fs::write(
        output.join("replay.json"),
        serde_json::to_vec_pretty(&replay).must_be("serialize exact failed analyzer invocation"),
    )
    .must_be("retain exact failed analyzer invocation");
}

fn configured_analyzer() -> (PathBuf, Vec<String>) {
    match &*GENUINE_ANALYZER {
        Ok(analyzer) => analyzer.clone(),
        Err(error) => panic!("genuine analyzer preparation failed: {error}"),
    }
}

fn setup_genuine_analyzer() -> Result<(PathBuf, Vec<String>), String> {
    let executable = env::var_os("BEND2_LINT_TEST_PATH").ok_or_else(|| {
        "set BEND2_LINT_TEST_PATH to the genuine installed analyzer or Bun".to_owned()
    })?;
    let executable = PathBuf::from(executable);
    if !executable.is_absolute() {
        return Err("use an explicit absolute executable".to_owned());
    }
    let arguments: Vec<String> = env::var("BEND2_LINT_TEST_ARGUMENTS").map_or_else(
        |_| Ok(Vec::new()),
        |value| {
            serde_json::from_str(&value)
                .map_err(|error| format!("BEND2_LINT_TEST_ARGUMENTS must be a JSON array: {error}"))
        },
    )?;
    if arguments
        .iter()
        .any(|argument| argument.starts_with("--fix"))
    {
        return Err("BEND2_LINT_TEST_ARGUMENTS must not enable fixes".to_owned());
    }
    prepare_genuine_analyzer(&executable, &arguments)?;
    Ok((executable, arguments))
}

// A trusted temporary project with a standard 0.3.0 rule selects the one shared
// standard-rule program; preparation builds/downloads it at most once here.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(600);

fn prepare_genuine_analyzer(executable: &Path, arguments: &[String]) -> Result<(), String> {
    let directory = tempdir()
        .map_err(|error| format!("could not create the preparation workspace: {error}"))?;
    let project = directory.path();
    fs::write(
        project.join("bend-lint.json"),
        "{\"packages\":{\"bend-lint-rules\":\"0.3.0.0\"},\"rules\":{\"bend-lint-rules/language/unneeded_plus\":{}}}",
    )
    .map_err(|error| format!("could not write the preparation config: {error}"))?;
    let report_path = project.join("report.json");
    let started = Instant::now();
    let (status, stdout, stderr) =
        run_prepare_contained(executable, arguments, project, &report_path)?;
    eprintln!(
        "genuine analyzer prepare completed after {:.3}s with {status}; stderr:\n{}",
        started.elapsed().as_secs_f64(),
        String::from_utf8_lossy(&stderr)
    );
    if !status.success() {
        return Err(format!(
            "genuine analyzer preparation failed with {status}: {}",
            String::from_utf8_lossy(&stderr)
        ));
    }
    if !stdout.is_empty() {
        return Err("genuine analyzer prepare wrote unexpected stdout; the 0.3.0 contract writes only its report".to_owned());
    }
    let metadata = fs::metadata(&report_path)
        .map_err(|error| format!("genuine analyzer prepare did not write its report: {error}"))?;
    if metadata.len() > 1024 * 1024 {
        return Err("genuine analyzer preparation report exceeds the 1 MiB limit".to_owned());
    }
    let bytes = fs::read(&report_path)
        .map_err(|error| format!("could not read the preparation report: {error}"))?;
    let report: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("preparation report is not JSON: {error}"))?;
    if report["bendLint"] != "0.3.0" || report["exitCode"] != 0 {
        return Err(format!("unsupported preparation report identity: {report}"));
    }
    let backend = report["backend"]
        .as_str()
        .ok_or_else(|| format!("preparation report omits its backend: {report}"))?;
    eprintln!(
        "genuine analyzer backend: {backend}; runtime: {}; platform: {}",
        report["runtime"], report["platform"]
    );
    let require_native =
        env::var("BEND2_LINT_REQUIRE_NATIVE").is_ok_and(|value| value == "1" || value == "true");
    if require_native && backend != "native" {
        return Err(format!(
            "genuine analyzer prepared the '{backend}' rule backend, but this job requires native rules; install a supported Clang"
        ));
    }
    Ok(())
}

const SETUP_STREAM_LIMIT: u64 = 8 * 1024 * 1024;

async fn read_setup_pipe(pipe: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.take(SETUP_STREAM_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await?;
    if u64::try_from(bytes.len()).map_err(std::io::Error::other)? > SETUP_STREAM_LIMIT {
        return Err(std::io::Error::other(
            "genuine analyzer preparation output exceeds the 8 MiB limit",
        ));
    }
    Ok(bytes)
}

// One bounded preparation with the repository's process-tree containment: the
// timeout kills and reaps the analyzer and its native compiler descendants, and
// both pipes are drained to completion before the result is reported.
fn run_prepare_contained(
    executable: &Path,
    arguments: &[String],
    project: &Path,
    report: &Path,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the contained preparation runtime: {error}"))?;
    runtime.block_on(async {
        let mut command = CommandWrap::with_new(executable, |command| {
            command
                .args(arguments)
                .args(["prepare", "--json", "--report"])
                .arg(report)
                .current_dir(project)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(process_wrap::tokio::JobObject);
        let mut child = command
            .spawn()
            .map_err(|error| format!("could not start the genuine analyzer: {error}"))?;
        let (Some(stdout), Some(stderr)) = (child.stdout().take(), child.stderr().take()) else {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err("genuine analyzer preparation did not expose both pipes".to_owned());
        };
        let stdout = tokio::spawn(read_setup_pipe(stdout));
        let stderr = tokio::spawn(read_setup_pipe(stderr));
        if let Ok(status) = tokio::time::timeout(PREPARE_TIMEOUT, child.wait()).await {
            let status = status.map_err(|error| {
                format!("could not await the genuine analyzer preparation: {error}")
            })?;
            let stdout = stdout
                .await
                .map_err(|error| format!("stdout drain task failed: {error}"))?
                .map_err(|error| format!("could not drain preparation stdout: {error}"))?;
            let stderr = stderr
                .await
                .map_err(|error| format!("stderr drain task failed: {error}"))?
                .map_err(|error| format!("could not drain preparation stderr: {error}"))?;
            Ok((status, stdout, stderr))
        } else {
            let _ = child.start_kill();
            let _ = child.wait().await;
            // The contained tree is gone, so the drains reach EOF; wait for
            // them so no pipe reader outlives the preparation attempt.
            let _ = stdout.await;
            let _ = stderr.await;
            Err(format!(
                "genuine analyzer preparation exceeded {}s; its process tree is being killed and reaped",
                PREPARE_TIMEOUT.as_secs()
            ))
        }
    })
}

fn genuine_report(executable: &Path, arguments: &[String], root: &Path) -> Value {
    let started = Instant::now();
    let cli = Command::new(executable)
        .args(arguments)
        .arg(root)
        .arg("--json")
        .args(["--jobs", "1"])
        .current_dir(root.parent().must_be("genuine project"))
        .output()
        .must_be("run genuine bend-lint CLI without fixes");
    eprintln!(
        "genuine analyzer CLI completed after {:.3}s with {}; stderr:\n{}",
        started.elapsed().as_secs_f64(),
        cli.status,
        String::from_utf8_lossy(&cli.stderr)
    );
    assert!(
        cli.status.success(),
        "real analyzer failed: {}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let report: Value =
        serde_json::from_slice(&cli.stdout).must_be("genuine upstream CLI JSON envelope");
    assert_eq!(report["ok"], true);
    assert!(report["suppressed"].is_array());
    report
}

fn genuine_overlay_report(
    executable: &Path,
    arguments: &[String],
    project: &Path,
    entry: &Path,
    overlays: &BTreeMap<PathBuf, String>,
) -> Value {
    let mut child = Command::new(executable)
        .args(arguments)
        .arg(entry)
        .args(["--json", "--overlays", "--jobs", "1"])
        .current_dir(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .must_be("spawn genuine overlay CLI");
    // The CLI consumes ordinary native paths, not Windows verbatim prefixes.
    // Normalize independently of the server's serializer so this remains an
    // external oracle for the source view rather than an echo of LSP wiring.
    let view: BTreeMap<_, _> = overlays
        .iter()
        .map(|(path, text)| {
            let path = Url::from_file_path(path)
                .must_be("lossless overlay file URL")
                .to_file_path()
                .must_be("native overlay protocol path");
            (path, text)
        })
        .collect();
    let input = serde_json::to_vec(&view).must_be("serialize genuine overlay view");
    child
        .stdin
        .take()
        .must_be("genuine overlay stdin")
        .write_all(&input)
        .must_be("write the complete genuine overlay view");
    let output = child.wait_with_output().must_be("genuine overlay CLI");
    assert!(
        output.status.success(),
        "real overlay analyzer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value =
        serde_json::from_slice(&output.stdout).must_be("genuine overlay CLI JSON envelope");
    assert_eq!(report["ok"], true);
    report
}

fn rule_docs(code: &str) -> String {
    format!("https://github.com/MattCozendey/bend-lint/blob/main/src/{code}.bend")
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).must_be("canonical genuine source path")
}

// Use ordinary native CLI paths, not Rust's Windows verbatim prefix. Resolving
// the existing directory first also expands a temporary directory's 8.3 alias.
fn canonical_cli_path(path: &Path) -> PathBuf {
    Url::from_file_path(canonical(path))
        .must_be("physical CLI file URL")
        .to_file_path()
        .must_be("ordinary native CLI path")
}

// A genuine finding path may be the UTF-8 alias spelling or the native name.
// Either must resolve to the captured physical entry: a replacement character
// or a renamed/copied path is a lossy substitution, not coverage.
#[cfg(target_os = "linux")]
fn assert_lossless_finding_path(reported: &Value, project: &Path, target: &Path) {
    let text = reported
        .as_str()
        .must_be("genuine finding path is a UTF-8 string");
    assert!(
        !text.contains('\u{FFFD}'),
        "the finding path keeps its actual bytes: {text}"
    );
    let path = Path::new(text);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project.join(path)
    };
    assert_eq!(
        fs::canonicalize(&path).must_be("finding path resolves on disk"),
        canonical(target),
        "the finding names the same physical non-UTF-8 entry"
    );
}

fn lint_sources(packet: &Value) -> Vec<&Value> {
    packet["params"]["diagnostics"]
        .as_array()
        .must_be("published diagnostics")
        .iter()
        .filter(|diagnostic| diagnostic["source"] == "bend-lint")
        .collect()
}

fn has_code(packet: &Value, code: &str) -> bool {
    lint_sources(packet)
        .iter()
        .any(|diagnostic| diagnostic["code"] == code)
}

fn wait_for_root_packet(
    client: &mut LspClient,
    document: &str,
    version: i32,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    client
        .receive_matching(Duration::from_secs(150), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == document
                && message["params"]["version"] == version
                && predicate(message)
        })
        .must_be("genuine root analysis publication")
}

fn finding<'a>(findings: &'a Value, code: &str) -> &'a Value {
    findings
        .as_array()
        .must_be("genuine findings")
        .iter()
        .find(|finding| finding["code"] == code)
        .must_be("genuine expected rule finding")
}

fn reported_format_edit<'a>(format: &'a Value, root: &Path, canonical: &str) -> &'a Value {
    let fixes = format["fixes"].as_array().must_be("real reported fixes");
    assert_eq!(fixes.len(), 1);
    assert_eq!(fixes[0]["applicability"], "dangerous");
    let edits = fixes[0]["edits"].as_array().must_be("real reported edits");
    assert_eq!(edits.len(), 1);
    let edit = &edits[0];
    let target = Path::new(edit["path"].as_str().must_be("reported edit path"));
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.parent().must_be("genuine project").join(target)
    };
    assert_eq!(
        fs::canonicalize(target).must_be("actual reported edit target"),
        fs::canonicalize(root).must_be("actual source path")
    );
    assert_eq!(
        edit["range"],
        json!({
            "start":{"line":0,"character":0},
            "end":{"line":4,"character":0}
        })
    );
    assert_eq!(edit["text"], canonical);
    edit
}

fn configured_client(project: &Path, executable: &Path, arguments: &[String]) -> LspClient {
    let mut command = env::var_os("BEND2_LSP_TEST_BINARY").map_or_else(
        || Command::new(env!("CARGO_BIN_EXE_bend2-lsp")),
        Command::new,
    );
    command.current_dir(project).stderr(Stdio::inherit());
    let mut client = LspClient::spawn(command);
    client.initialize(project);
    configure_analyzer(&mut client, executable, arguments);
    client
}

fn configure_analyzer(client: &mut LspClient, executable: &Path, arguments: &[String]) {
    let mut settings = json!({"bend":{"lint":{
        "enabled":true,"path":executable,"arguments":arguments,
        "dangerousCodeActions":true,"timeoutMs":ANALYZER_TIMEOUT_MS
    }}});
    if let Some(compiler) = env::var_os("BEND2_COMPILER_TEST_PATH") {
        let compiler = PathBuf::from(compiler);
        assert!(compiler.is_absolute());
        settings["bend2-lsp"] = json!({"compilerPath":compiler});
    }
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":settings}),
    );
}

fn opened_diagnostics(
    client: &mut LspClient,
    document: &str,
    source: &str,
    started: Instant,
) -> Value {
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":source
        }}),
    );
    // Opening schedules live analysis of the current buffer; no save is needed.
    eprintln!(
        "[LSP +{:.3}s] didOpen; analyzer timeout is {ANALYZER_TIMEOUT_MS}ms",
        started.elapsed().as_secs_f64()
    );
    let packet = wait_for_lint_diagnostics(client, document, started, 1);
    assert_eq!(packet["params"]["version"], 1);
    packet
}

fn wait_for_lint_diagnostics(
    client: &mut LspClient,
    document: &str,
    started: Instant,
    version: i32,
) -> Value {
    wait_for_lint_diagnostics_within(client, document, started, version, Duration::from_secs(150))
}

fn wait_for_lint_diagnostics_within(
    client: &mut LspClient,
    document: &str,
    started: Instant,
    version: i32,
    allowance: Duration,
) -> Value {
    let waiting = Instant::now();
    let deadline = waiting + allowance;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Include already buffered messages, with their original receive times:
        // initialization/configuration warnings can explain a missing provider.
        let Some((message, received)) = client.receive_matching_timed(remaining, |message| {
            message["method"] == "window/logMessage"
                || (message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == document)
        }) else {
            break;
        };
        eprintln!(
            "[LSP +{:.3}s] {message}",
            received.duration_since(started).as_secs_f64()
        );
        if lint_packet(&message, document) && message["params"]["version"] == version {
            return message;
        }
    }
    panic!(
        "real analyzer finding did not arrive through stdio LSP after {:.3}s \
         (allowed {allowance:?}; configured analyzer timeout: {ANALYZER_TIMEOUT_MS}ms; \
         LSP exit status: {:?}); see timestamped window/logMessage and \
         publishDiagnostics messages above",
        waiting.elapsed().as_secs_f64(),
        client.try_wait()
    );
}

fn explicit_user_fix(
    client: &mut LspClient,
    root: &Path,
    source: &str,
    canonical: &str,
    diagnostic: &Value,
    reported_edit: &Value,
) {
    let document = uri(root);
    let automatic = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":document},"range":diagnostic["range"],
            "context":{"diagnostics":[diagnostic],"triggerKind":2,
                "only":["quickfix.bend-lint.dangerous"]}
        }),
    );
    assert_eq!(
        automatic["result"],
        json!([]),
        "no automatic dangerous action"
    );
    let response = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":document},"range":diagnostic["range"],
            "context":{"diagnostics":[diagnostic],"triggerKind":1,
                "only":["quickfix.bend-lint.dangerous"]}
        }),
    );
    let actions = response["result"]
        .as_array()
        .must_be("explicit genuine actions");
    assert_eq!(actions.len(), 1, "only the actual reported dangerous fix");
    let action = &actions[0];
    assert_eq!(action["kind"], "quickfix.bend-lint.dangerous");
    assert_eq!(action["isPreferred"], false);
    assert!(
        action["title"]
            .as_str()
            .must_be("dangerous title")
            .starts_with("Dangerous")
    );
    assert_eq!(action["diagnostics"][0]["code"], diagnostic["code"]);
    assert!(action["command"].is_null(), "only client-applied edits");
    assert!(
        action["edit"]["changes"].is_null(),
        "never unversioned changes"
    );
    let changes = action["edit"]["documentChanges"]
        .as_array()
        .must_be("versioned edit");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["textDocument"]["uri"], document);
    assert_eq!(changes[0]["textDocument"]["version"], 1);
    let edits = changes[0]["edits"].as_array().must_be("actual replacement");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0]["range"], reported_edit["range"]);
    assert_eq!(edits[0]["newText"], reported_edit["text"]);
    assert_eq!(
        fs::read_to_string(root).must_be("unchanged genuine source"),
        source
    );
    // Simulate a user's acceptance only after validating the full-file range
    // and document version; the server itself must never write the edit.
    let mut applied = source.to_owned();
    let end = applied.len();
    applied.replace_range(
        0..end,
        edits[0]["newText"].as_str().must_be("replacement text"),
    );
    assert_eq!(
        applied, canonical,
        "independently expected two-space formatting"
    );
    fs::write(root, &applied).must_be("user explicitly applies the versioned edit");
    assert_eq!(
        fs::read_to_string(root).must_be("user-applied source"),
        canonical
    );
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument":{"uri":document,"version":2},
            "contentChanges":[{"text":applied}]
        }),
    );
}

fn organize_imports_source(
    client: &mut LspClient,
    root: &Path,
    source: &str,
    version: i32,
) -> Option<String> {
    let document = uri(root);
    let response = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":document},
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},
            "context":{"diagnostics":[],"only":["source.organizeImports"],"triggerKind":1}
        }),
    );
    let action = response["result"]
        .as_array()
        .must_be("import action response")
        .iter()
        .find(|action| action["kind"] == "source.organizeImports")?;
    let changes = action["edit"]["documentChanges"]
        .as_array()
        .must_be("versioned import edit");
    assert_eq!(
        changes.len(),
        1,
        "organizing only edits the current document"
    );
    assert_eq!(changes[0]["textDocument"]["uri"], document);
    assert_eq!(changes[0]["textDocument"]["version"], version);
    let edits: Vec<TextEdit> =
        serde_json::from_value(changes[0]["edits"].clone()).must_be("import text edits");
    let index = LineIndex::new(source);
    let mut edits: Vec<_> = edits
        .into_iter()
        .map(|edit| {
            (
                index.offset(source, edit.range.start.line, edit.range.start.character),
                index.offset(source, edit.range.end.line, edit.range.end.character),
                edit.new_text,
            )
        })
        .collect();
    edits.sort_unstable_by_key(|(start, end, _)| (*start, *end));
    let mut result = source.to_owned();
    for (start, end, replacement) in edits.into_iter().rev() {
        result.replace_range(start..end, &replacement);
    }
    Some(result)
}

fn uri(path: &Path) -> String {
    Url::from_file_path(path)
        .must_be("real file URI")
        .to_string()
}

fn lint_packet(message: &Value, document: &str) -> bool {
    message["method"] == "textDocument/publishDiagnostics"
        && message["params"]["uri"] == document
        && message["params"]["diagnostics"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["source"] == "bend-lint"))
}

#[path = "bend_lint_e2e/dx.rs"]
mod dx;

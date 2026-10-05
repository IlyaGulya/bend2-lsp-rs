mod support;

use std::{fmt::Write as _, fs, path::PathBuf, sync::LazyLock};
use support::Must;

use bend2_lsp::{analysis, workspace};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};

const SOURCE: &str = include_str!("fixtures/analyzer_input.bend");
const MEDIUM_SOURCE: &str = include_str!("fixtures/analyzer_medium.bend");
const LARGE_SOURCE: &str = include_str!("fixtures/analyzer_large.bend");
static UNICODE_SOURCE: LazyLock<String> =
    LazyLock::new(|| format!("{MEDIUM_SOURCE}\n#{}", "é".repeat(4096)));
static ASCII_LINE_INDEX: LazyLock<analysis::LineIndex> =
    LazyLock::new(|| analysis::LineIndex::new(SOURCE));
static UNICODE_LINE_INDEX: LazyLock<analysis::LineIndex> =
    LazyLock::new(|| analysis::LineIndex::new(UNICODE_SOURCE.as_str()));
static ASCII_EOF_POSITION: LazyLock<(u32, u32)> =
    LazyLock::new(|| ASCII_LINE_INDEX.position(SOURCE, SOURCE.len()));
static UNICODE_EOF_POSITION: LazyLock<(u32, u32)> =
    LazyLock::new(|| UNICODE_LINE_INDEX.position(UNICODE_SOURCE.as_str(), UNICODE_SOURCE.len()));

static SMALL_SNAPSHOT: LazyLock<analysis::DocumentSnapshot> =
    LazyLock::new(|| analysis::DocumentSnapshot::new(analysis::Revision(0), SOURCE.to_owned()));
static MEDIUM_SNAPSHOT: LazyLock<analysis::DocumentSnapshot> = LazyLock::new(|| {
    analysis::DocumentSnapshot::new(analysis::Revision(0), MEDIUM_SOURCE.to_owned())
});
static LARGE_SNAPSHOT: LazyLock<analysis::DocumentSnapshot> = LazyLock::new(|| {
    analysis::DocumentSnapshot::new(analysis::Revision(0), LARGE_SOURCE.to_owned())
});

static CONSTRUCTOR_SNAPSHOT: LazyLock<analysis::DocumentSnapshot> = LazyLock::new(|| {
    analysis::DocumentSnapshot::new(
        analysis::Revision(0),
        "type SyntaxTerm is Data:\n  TermVar{index: Nat}\n  TermRef{name: String}\n".to_owned(),
    )
});

static PARAMETER_SNAPSHOT: LazyLock<analysis::DocumentSnapshot> = LazyLock::new(|| {
    analysis::DocumentSnapshot::new(
        analysis::Revision(0),
        "def typed(builtin: U32, qualified: Ast.Term, nested: (Ast.Term, List(U32))) -> U32:\n  builtin\n".to_owned(),
    )
});

fn parameter_annotation_case(
    index: usize,
) -> (&'static analysis::DocumentSnapshot, analysis::SymbolId) {
    let snapshot = LazyLock::force(&PARAMETER_SNAPSHOT);
    let declaration = snapshot
        .syntax
        .symbols()
        .first()
        .must_be("typed declaration");
    let parameter = snapshot
        .syntax
        .parameters(declaration)
        .get(index)
        .must_be("indexed parameter");
    (snapshot, parameter.id)
}

struct WarmSnapshot {
    source: &'static str,
    snapshot: &'static analysis::DocumentSnapshot,
    completion_prefix: &'static str,
    identifier_name: &'static str,
}

impl WarmSnapshot {
    fn small() -> Self {
        Self {
            source: SOURCE,
            snapshot: &SMALL_SNAPSHOT,
            completion_prefix: "transform_",
            identifier_name: "transform_31",
        }
    }

    fn medium() -> Self {
        Self {
            source: MEDIUM_SOURCE,
            snapshot: &MEDIUM_SNAPSHOT,
            completion_prefix: "worker_",
            identifier_name: "worker_0199",
        }
    }

    fn large() -> Self {
        Self {
            source: LARGE_SOURCE,
            snapshot: &LARGE_SNAPSHOT,
            completion_prefix: "worker_",
            identifier_name: "worker_0599",
        }
    }
}

fn folding_source(lines: usize) -> String {
    let mut source = String::with_capacity(lines * 7);
    for line in 0..lines {
        if line > 0 {
            source.push('\n');
        }
        source.push_str(if line % 2 == 0 { "block" } else { "  body" });
    }
    source
}

static FOLDING_100: LazyLock<analysis::DocumentSnapshot> =
    LazyLock::new(|| analysis::DocumentSnapshot::new(analysis::Revision(0), folding_source(100)));
static FOLDING_1000: LazyLock<analysis::DocumentSnapshot> =
    LazyLock::new(|| analysis::DocumentSnapshot::new(analysis::Revision(0), folding_source(1_000)));
static FOLDING_10000: LazyLock<analysis::DocumentSnapshot> = LazyLock::new(|| {
    analysis::DocumentSnapshot::new(analysis::Revision(0), folding_source(10_000))
});

struct Folding100(&'static analysis::DocumentSnapshot);

impl Default for Folding100 {
    fn default() -> Self {
        Self(&FOLDING_100)
    }
}

struct Folding1000(&'static analysis::DocumentSnapshot);

impl Default for Folding1000 {
    fn default() -> Self {
        Self(&FOLDING_1000)
    }
}

struct Folding10000(&'static analysis::DocumentSnapshot);

impl Default for Folding10000 {
    fn default() -> Self {
        Self(&FOLDING_10000)
    }
}

const WORKSPACE_FILE_COUNT: usize = 100;
const FUNCTIONS_PER_CLIENT: usize = 6;
const CALLS_PER_FUNCTION: usize = 8;

fn common_module_source() -> String {
    let mut source = String::from("def identity(value):\n  value\n");
    for function in 0..FUNCTIONS_PER_CLIENT {
        let _ = writeln!(source, "def common_worker_{function:02}(value):");
        source.push_str("  identity(value)\n");
    }
    source
}

fn workspace_file_source(index: usize, file_count: usize) -> String {
    if index + 1 == file_count {
        return common_module_source();
    }

    let mut source = String::new();
    if index == 0 {
        for target in 1..file_count {
            let alias = if target + 1 == file_count {
                "Common".to_owned()
            } else {
                format!("File{target:03}")
            };
            let _ = writeln!(source, "import ./f{target:03}.bend as {alias}");
        }
    } else {
        let _ = writeln!(source, "import ./f{:03}.bend as Common", file_count - 1);
    }

    for function in 0..FUNCTIONS_PER_CLIENT {
        let _ = writeln!(source, "def worker_{index:03}_{function:02}(value):");
        source.push_str("  ");
        for _ in 0..CALLS_PER_FUNCTION {
            source.push_str("Common.identity(");
        }
        source.push_str("value");
        for _ in 0..CALLS_PER_FUNCTION {
            source.push(')');
        }
        source.push('\n');
    }
    source
}

struct WorkspaceFiles {
    _directory: tempfile::TempDir,
    paths: Vec<PathBuf>,
    root_text: String,
    common_text: String,
}

impl WorkspaceFiles {
    fn create() -> Self {
        let directory = tempfile::tempdir().must_be("temporary benchmark workspace");
        let paths: Vec<PathBuf> = (0..WORKSPACE_FILE_COUNT)
            .map(|index| directory.path().join(format!("f{index:03}.bend")))
            .collect();
        for (index, path) in paths.iter().enumerate() {
            fs::write(path, workspace_file_source(index, paths.len()))
                .must_be("write benchmark source");
        }
        let root_text = fs::read_to_string(&paths[0]).must_be("read benchmark root");
        let common_text = fs::read_to_string(paths.last().must_be("common module path"))
            .must_be("read common module");
        Self {
            _directory: directory,
            paths,
            root_text,
            common_text,
        }
    }
}

struct InitialWorkspace {
    files: WorkspaceFiles,
    database: workspace::WorkspaceDb,
}

impl Default for InitialWorkspace {
    fn default() -> Self {
        Self {
            files: WorkspaceFiles::create(),
            database: workspace::WorkspaceDb::default(),
        }
    }
}

struct LoadedWorkspace {
    files: WorkspaceFiles,
    database: workspace::WorkspaceDb,
}

impl Default for LoadedWorkspace {
    fn default() -> Self {
        let files = WorkspaceFiles::create();
        let root_uri = url::Url::from_file_path(&files.paths[0]).must_be("root URI");
        let mut database = workspace::WorkspaceDb::default();
        let root = database.set_open_document(
            workspace::Document::new(
                root_uri.clone(),
                "bend".into(),
                analysis::Revision(1),
                files.root_text.clone(),
            ),
            Some(files.paths[0].clone()),
        );
        database.load_reachable(std::slice::from_ref(&root));

        let root_document = database.open_document(&root_uri).must_be("open root");
        let import = analysis::imports(&root_document)
            .first()
            .must_be("root import");
        assert!(database.import_target(&root_uri, import.path).is_some());
        assert_eq!(database.open_documents().len(), 1);
        let indexed = database.indexed_documents();
        assert_eq!(indexed.len(), WORKSPACE_FILE_COUNT);
        let declarations: usize = indexed
            .iter()
            .map(|document| document.syntax.symbols().len())
            .sum();
        let calls: usize = indexed
            .iter()
            .map(|document| document.syntax.calls().len())
            .sum();
        assert!(
            (500..=1_000).contains(&declarations),
            "workspace declaration count was {declarations}"
        );
        assert!(calls >= 4_000, "workspace call count was {calls}");
        assert_eq!(database.file_id_by_uri(&root_uri), Some(root));
        assert_eq!(database.file_id_by_path(&files.paths[0]), Some(root));

        database
            .update_open_snapshot(&root_uri, root_document.snapshot.clone())
            .must_be("update root snapshot");
        let closed = database.close_document(&root_uri).must_be("close root");
        database.load_reachable(std::slice::from_ref(&closed));
        let root = database.set_open_document(
            workspace::Document::with_snapshot(
                root_uri.clone(),
                "bend".into(),
                root_document.snapshot,
            ),
            Some(files.paths[0].clone()),
        );
        database.load_reachable(std::slice::from_ref(&root));
        let common_path = files.paths.last().must_be("common module path").clone();
        let common_uri = url::Url::from_file_path(&common_path).must_be("common module URI");
        let common = database.set_open_document(
            workspace::Document::new(
                common_uri,
                "bend".into(),
                analysis::Revision(1),
                files.common_text.clone(),
            ),
            Some(common_path),
        );
        database.load_reachable(std::slice::from_ref(&common));
        Self { files, database }
    }
}

// In-memory snapshots isolate semantic indexing from disk I/O and syntax
// construction. Unrelated files deliberately reuse the target spelling, but
// resolve it locally; sparse cases keep exactly three matching importer files.
struct SemanticSources {
    directory: tempfile::TempDir,
    documents: Vec<(workspace::Document, PathBuf)>,
}

impl SemanticSources {
    fn new(file_count: usize, matched_files: usize) -> Self {
        let directory = tempfile::tempdir().must_be("semantic benchmark workspace");
        let target = std::sync::Arc::new(analysis::DocumentSnapshot::new(
            analysis::Revision(1),
            "def identity(value):\n  value\n".into(),
        ));
        let client = std::sync::Arc::new(analysis::DocumentSnapshot::new(
            analysis::Revision(1),
            "import ./target.bend as Dep\ndef client(value):\n  Dep.identity(value)\n  Dep.identity(value)\n".into(),
        ));
        let unrelated = std::sync::Arc::new(analysis::DocumentSnapshot::new(
            analysis::Revision(1),
            "def identity(value):\n  value\ndef unrelated(value):\n  identity(value)\n".into(),
        ));
        let documents = (0..file_count)
            .map(|index| {
                let (name, snapshot) = if index == 0 {
                    ("target.bend".to_owned(), target.clone())
                } else if index <= matched_files {
                    (format!("client{index:05}.bend"), client.clone())
                } else {
                    (format!("unrelated{index:05}.bend"), unrelated.clone())
                };
                let path = directory.path().join(name);
                let uri = url::Url::from_file_path(&path).must_be("semantic file URI");
                (
                    workspace::Document::with_snapshot(uri, "bend".into(), snapshot),
                    path,
                )
            })
            .collect();
        Self {
            directory,
            documents,
        }
    }

    // Exercise interleaved imported members, duplicate aliases, multiple callers,
    // and bare non-call occurrences without changing the original workloads.
    fn interleaved(file_count: usize) -> Self {
        let directory = tempfile::tempdir().must_be("interleaved semantic workspace");
        let target = std::sync::Arc::new(analysis::DocumentSnapshot::new(
            analysis::Revision(1),
            "def identity(value):\n  value\ndef alternate(value):\n  value\n".into(),
        ));
        let client = std::sync::Arc::new(analysis::DocumentSnapshot::new(
            analysis::Revision(1),
            "import ./target.bend as A\nimport ./target.bend as B\ndef client(value):\n  A.identity(A.alternate(value))\n  B.identity(value)\n  A.identity\ndef second(value):\n  B.alternate(B.identity(value))\n  A.alternate(value)\n".into(),
        ));
        let documents = (0..file_count)
            .map(|index| {
                let (name, snapshot) = if index == 0 {
                    ("target.bend".to_owned(), target.clone())
                } else {
                    (format!("client{index:05}.bend"), client.clone())
                };
                let path = directory.path().join(name);
                let uri = url::Url::from_file_path(&path).must_be("interleaved semantic URI");
                (
                    workspace::Document::with_snapshot(uri, "bend".into(), snapshot),
                    path,
                )
            })
            .collect();
        Self {
            directory,
            documents,
        }
    }

    fn build(self) -> (tempfile::TempDir, workspace::WorkspaceDb) {
        let mut database = workspace::WorkspaceDb::default();
        for (document, path) in self.documents {
            database.set_open_document(document, Some(path));
        }
        (self.directory, database)
    }
}

struct SemanticWorkspace {
    _directory: tempfile::TempDir,
    database: workspace::WorkspaceDb,
    target: workspace::GlobalSymbolId,
    caller: workspace::GlobalSymbolId,
    target_uri: url::Url,
    replacement: std::sync::Arc<analysis::DocumentSnapshot>,
}

impl SemanticWorkspace {
    fn new(file_count: usize, matched_files: usize) -> Self {
        let (directory, database) = SemanticSources::new(file_count, matched_files).build();
        let target_uri = url::Url::from_file_path(directory.path().join("target.bend"))
            .must_be("semantic target URI");
        let caller_uri = url::Url::from_file_path(directory.path().join("client00001.bend"))
            .must_be("semantic caller URI");
        let target = database
            .symbol_by_name(&target_uri, "identity")
            .must_be("semantic target")
            .id;
        let caller = database
            .symbol_by_name(&caller_uri, "client")
            .must_be("semantic caller")
            .id;
        let references = database.references(target, false);
        assert_eq!(references.len(), matched_files * 2);
        assert!(
            references
                .iter()
                .all(|reference| reference.document.uri != target_uri)
        );
        let incoming = database.incoming_calls(target);
        assert_eq!(incoming.len(), matched_files);
        assert!(incoming.iter().all(|group| group.ranges.len() == 2));
        let outgoing = database.outgoing_calls(caller);
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].symbol.id, target);
        assert_eq!(outgoing[0].ranges.len(), 2);
        let replacement = std::sync::Arc::new(analysis::DocumentSnapshot::new(
            analysis::Revision(2),
            "def inserted(value):\n  value\ndef identity(value):\n  value\n".into(),
        ));
        Self {
            _directory: directory,
            database,
            target,
            caller,
            target_uri,
            replacement,
        }
    }
}

static SEMANTIC_SPARSE_100: LazyLock<SemanticWorkspace> =
    LazyLock::new(|| SemanticWorkspace::new(100, 3));
static SEMANTIC_SPARSE_1000: LazyLock<SemanticWorkspace> =
    LazyLock::new(|| SemanticWorkspace::new(1_000, 3));
static SEMANTIC_SPARSE_10000: LazyLock<SemanticWorkspace> =
    LazyLock::new(|| SemanticWorkspace::new(10_000, 3));
static SEMANTIC_MATCHED_100: LazyLock<SemanticWorkspace> =
    LazyLock::new(|| SemanticWorkspace::new(100, 99));
static SEMANTIC_MATCHED_1000: LazyLock<SemanticWorkspace> =
    LazyLock::new(|| SemanticWorkspace::new(1_000, 999));
static SEMANTIC_MATCHED_10000: LazyLock<SemanticWorkspace> =
    LazyLock::new(|| SemanticWorkspace::new(10_000, 9_999));

struct ReferenceLookupWorkspace {
    _directory: tempfile::TempDir,
    database: workspace::WorkspaceDb,
    target: workspace::GlobalSymbolId,
}

impl ReferenceLookupWorkspace {
    fn new(file_count: usize, matched_files: usize) -> Self {
        let mut sources = SemanticSources::new(file_count, matched_files);
        let independent_target_uri = if matched_files == 0 {
            // Intern the same member name through a different file's external
            // relation: querying target.bend must reach an absent bucket, not
            // stop at an unknown name or invalid symbol identity.
            let uri = sources.documents[1].0.uri.clone();
            let document = &mut sources.documents[2].0;
            document.snapshot = std::sync::Arc::new(analysis::DocumentSnapshot::new(
                analysis::Revision(1),
                "import ./unrelated00001.bend as Other\ndef client(value):\n  Other.identity(value)\n  Other.identity(value)\n".into(),
            ));
            Some(uri)
        } else {
            None
        };
        let (directory, database) = sources.build();
        let target_uri = url::Url::from_file_path(directory.path().join("target.bend"))
            .must_be("reference lookup target URI");
        let target = database
            .symbol_by_name(&target_uri, "identity")
            .must_be("reference lookup target")
            .id;
        assert_eq!(
            database.global_symbol_id(&target_uri, target.local_symbol()),
            Some(target)
        );
        let mut expected_sources = (1..=matched_files)
            .map(|index| {
                let uri = url::Url::from_file_path(
                    directory.path().join(format!("client{index:05}.bend")),
                )
                .must_be("reference lookup source URI");
                database
                    .file_id_by_uri(&uri)
                    .must_be("reference lookup source identity")
            })
            .collect::<std::collections::HashSet<_>>();
        let (groups, occurrences) = database.external_reference_groups(target).fold(
            (0, 0),
            |(groups, occurrences), group| {
                assert!(expected_sources.remove(&group.source()));
                assert_eq!(group.occurrence_count(), 2);
                (groups + 1, occurrences + group.occurrence_count())
            },
        );
        assert_eq!(groups, matched_files);
        assert_eq!(occurrences, matched_files * 2);
        assert!(expected_sources.is_empty());
        if let Some(uri) = independent_target_uri {
            let independent_target = database
                .symbol_by_name(&uri, "identity")
                .must_be("independent reference lookup target")
                .id;
            let mut groups = database.external_reference_groups(independent_target);
            let group = groups.next().must_be("independent external relation");
            assert_eq!(group.occurrence_count(), 2);
            assert!(groups.next().is_none());
        }
        Self {
            _directory: directory,
            database,
            target,
        }
    }
}

static REFERENCE_LOOKUP_ABSENT_100: LazyLock<ReferenceLookupWorkspace> =
    LazyLock::new(|| ReferenceLookupWorkspace::new(100, 0));
static REFERENCE_LOOKUP_ABSENT_1000: LazyLock<ReferenceLookupWorkspace> =
    LazyLock::new(|| ReferenceLookupWorkspace::new(1_000, 0));
static REFERENCE_LOOKUP_ABSENT_10000: LazyLock<ReferenceLookupWorkspace> =
    LazyLock::new(|| ReferenceLookupWorkspace::new(10_000, 0));
static REFERENCE_LOOKUP_SINGLE_100: LazyLock<ReferenceLookupWorkspace> =
    LazyLock::new(|| ReferenceLookupWorkspace::new(100, 1));
static REFERENCE_LOOKUP_SINGLE_1000: LazyLock<ReferenceLookupWorkspace> =
    LazyLock::new(|| ReferenceLookupWorkspace::new(1_000, 1));
static REFERENCE_LOOKUP_SINGLE_10000: LazyLock<ReferenceLookupWorkspace> =
    LazyLock::new(|| ReferenceLookupWorkspace::new(10_000, 1));
static REFERENCE_LOOKUP_THREE_100: LazyLock<ReferenceLookupWorkspace> =
    LazyLock::new(|| ReferenceLookupWorkspace::new(100, 3));
static REFERENCE_LOOKUP_THREE_1000: LazyLock<ReferenceLookupWorkspace> =
    LazyLock::new(|| ReferenceLookupWorkspace::new(1_000, 3));
static REFERENCE_LOOKUP_THREE_10000: LazyLock<ReferenceLookupWorkspace> =
    LazyLock::new(|| ReferenceLookupWorkspace::new(10_000, 3));

#[library_benchmark]
fn cold_snapshot_build_small() -> analysis::DocumentSnapshot {
    std::hint::black_box(analysis::DocumentSnapshot::new(
        analysis::Revision(0),
        std::hint::black_box(SOURCE).to_owned(),
    ))
}

#[library_benchmark]
fn cold_snapshot_build_medium() -> analysis::DocumentSnapshot {
    std::hint::black_box(analysis::DocumentSnapshot::new(
        analysis::Revision(0),
        std::hint::black_box(MEDIUM_SOURCE).to_owned(),
    ))
}

#[library_benchmark]
fn cold_snapshot_build_large() -> analysis::DocumentSnapshot {
    std::hint::black_box(analysis::DocumentSnapshot::new(
        analysis::Revision(0),
        std::hint::black_box(LARGE_SOURCE).to_owned(),
    ))
}

#[library_benchmark]
#[bench::small(WarmSnapshot::small())]
#[bench::medium(WarmSnapshot::medium())]
#[bench::large(WarmSnapshot::large())]
fn semantic_tokens_warm(fixture: WarmSnapshot) -> Vec<analysis::SemanticToken> {
    std::hint::black_box(analysis::semantic_tokens(std::hint::black_box(
        fixture.snapshot,
    )))
}

#[library_benchmark]
#[bench::small(WarmSnapshot::small())]
#[bench::medium(WarmSnapshot::medium())]
#[bench::large(WarmSnapshot::large())]
fn completion_warm(fixture: WarmSnapshot) -> Vec<analysis::Completion> {
    std::hint::black_box(analysis::completion_items(
        std::hint::black_box(fixture.snapshot),
        std::hint::black_box(fixture.completion_prefix),
    ))
}

#[library_benchmark]
#[bench::small(WarmSnapshot::small())]
#[bench::medium(WarmSnapshot::medium())]
#[bench::large(WarmSnapshot::large())]
fn identifier_ranges_warm(fixture: WarmSnapshot) -> Vec<analysis::TextRange> {
    std::hint::black_box(analysis::identifier_ranges(
        std::hint::black_box(fixture.snapshot),
        std::hint::black_box(fixture.identifier_name),
    ))
}

#[library_benchmark]
#[bench::small(WarmSnapshot::small())]
#[bench::medium(WarmSnapshot::medium())]
#[bench::large(WarmSnapshot::large())]
fn references_warm(fixture: WarmSnapshot) -> Vec<analysis::Reference> {
    let snapshot = fixture.snapshot;
    let references = snapshot
        .syntax
        .name_id(&snapshot.text, "identity")
        .and_then(|name| snapshot.syntax.symbol_by_name(name))
        .map_or_else(Vec::new, |symbol| {
            snapshot.syntax.references(symbol.id).copied().collect()
        });
    std::hint::black_box(references)
}

#[library_benchmark]
#[bench::small(WarmSnapshot::small())]
#[bench::medium(WarmSnapshot::medium())]
#[bench::large(WarmSnapshot::large())]
fn call_hierarchy_warm(fixture: WarmSnapshot) -> Vec<analysis::CallSite> {
    let snapshot = fixture.snapshot;
    let calls = snapshot
        .syntax
        .name_id(&snapshot.text, "identity")
        .and_then(|name| snapshot.syntax.symbol_by_name(name))
        .map_or_else(Vec::new, |symbol| {
            snapshot.syntax.calls_to(symbol.id).copied().collect()
        });
    std::hint::black_box(calls)
}

#[library_benchmark]
#[bench::small(WarmSnapshot::small())]
#[bench::medium(WarmSnapshot::medium())]
#[bench::large(WarmSnapshot::large())]
fn inlay_hints_warm(fixture: WarmSnapshot) -> Vec<analysis::InlayHint> {
    std::hint::black_box(analysis::inlay_hints(
        std::hint::black_box(fixture.snapshot),
        analysis::TextRange::new(0, fixture.source.len()),
    ))
}

fn setup_ascii_position(source: &'static str) -> &'static str {
    let index = LazyLock::force(&ASCII_LINE_INDEX);
    std::hint::black_box(index.position(source, source.len()));
    source
}

fn setup_ascii_offset(source: &'static str) -> &'static str {
    let (line, character) = *LazyLock::force(&ASCII_EOF_POSITION);
    std::hint::black_box(ASCII_LINE_INDEX.offset(source, line, character));
    source
}

fn prewarm_unicode_position() -> &'static str {
    let source = LazyLock::force(&UNICODE_SOURCE).as_str();
    let index = LazyLock::force(&UNICODE_LINE_INDEX);
    std::hint::black_box(index.position(source, source.len()));
    source
}

fn prewarm_unicode_offset() -> &'static str {
    let source = LazyLock::force(&UNICODE_SOURCE).as_str();
    let (line, character) = *LazyLock::force(&UNICODE_EOF_POSITION);
    std::hint::black_box(UNICODE_LINE_INDEX.offset(source, line, character));
    source
}

#[library_benchmark]
#[bench::prewarmed(args = (SOURCE), setup = setup_ascii_position)]
fn ascii_position_conversion(source: &'static str) -> (u32, u32) {
    std::hint::black_box(ASCII_LINE_INDEX.position(source, source.len()))
}

#[library_benchmark]
#[bench::prewarmed(args = (SOURCE), setup = setup_ascii_offset)]
fn ascii_offset_conversion(source: &'static str) -> usize {
    let (line, character) = *ASCII_EOF_POSITION;
    std::hint::black_box(ASCII_LINE_INDEX.offset(source, line, character))
}

#[library_benchmark]
#[bench::prewarmed(prewarm_unicode_position())]
fn unicode_position_conversion(source: &'static str) -> (u32, u32) {
    std::hint::black_box(UNICODE_LINE_INDEX.position(source, source.len()))
}

#[library_benchmark]
#[bench::prewarmed(prewarm_unicode_offset())]
fn unicode_offset_conversion(source: &'static str) -> usize {
    let (line, character) = *UNICODE_EOF_POSITION;
    std::hint::black_box(UNICODE_LINE_INDEX.offset(source, line, character))
}

#[library_benchmark]
#[bench::hundred_lines(Folding100::default())]
fn folding_100_lines(snapshot: Folding100) -> Vec<analysis::FoldingRange> {
    std::hint::black_box(analysis::folding_ranges(snapshot.0))
}

#[library_benchmark]
#[bench::thousand_lines(Folding1000::default())]
fn folding_1000_lines(snapshot: Folding1000) -> Vec<analysis::FoldingRange> {
    std::hint::black_box(analysis::folding_ranges(snapshot.0))
}

#[library_benchmark]
#[bench::ten_thousand_lines(Folding10000::default())]
fn folding_10000_lines(snapshot: Folding10000) -> Vec<analysis::FoldingRange> {
    std::hint::black_box(analysis::folding_ranges(snapshot.0))
}

#[library_benchmark]
#[bench::hundred_files(InitialWorkspace::default())]
fn workspace_initial_build(mut fixture: InitialWorkspace) -> Vec<workspace::Document> {
    let root = fixture.database.set_open_document(
        workspace::Document::new(
            url::Url::from_file_path(&fixture.files.paths[0]).must_be("root URI"),
            "bend".into(),
            analysis::Revision(1),
            std::mem::take(&mut fixture.files.root_text),
        ),
        Some(fixture.files.paths[0].clone()),
    );
    fixture.database.load_reachable(std::slice::from_ref(&root));
    std::hint::black_box(fixture.database.indexed_documents())
}

#[library_benchmark]
#[bench::hundred_files(LoadedWorkspace::default())]
fn workspace_incremental_invalidation(mut fixture: LoadedWorkspace) -> Vec<workspace::Document> {
    let common_uri = url::Url::from_file_path(&fixture.files.paths[WORKSPACE_FILE_COUNT - 1])
        .must_be("common module URI");
    let common = fixture
        .database
        .open_document(&common_uri)
        .must_be("open common module");
    let mut revised_text = fixture.files.common_text.clone();
    revised_text.push_str("# revision 2\n");
    let snapshot = std::sync::Arc::new(analysis::DocumentSnapshot::new(
        analysis::Revision(common.revision.0 + 1),
        revised_text,
    ));
    let (id, imports_changed) = fixture
        .database
        .update_open_snapshot(&common_uri, snapshot)
        .must_be("update common snapshot");
    if imports_changed {
        fixture.database.load_reachable(std::slice::from_ref(&id));
    }
    std::hint::black_box(fixture.database.dependents(id))
}

#[library_benchmark]
#[bench::hundred_files(LoadedWorkspace::default())]
fn workspace_references(fixture: LoadedWorkspace) -> Vec<(usize, analysis::TextRange)> {
    let common_uri = url::Url::from_file_path(&fixture.files.paths[WORKSPACE_FILE_COUNT - 1])
        .must_be("common URI");
    let common = fixture
        .database
        .cached_document(&common_uri)
        .must_be("cached common document");
    let target = common
        .syntax
        .name_id(&common.text, "identity")
        .and_then(|name| common.syntax.symbol_by_name(name))
        .must_be("common identity symbol");
    let name = common.syntax.name_text(&common.text, target.name);
    let documents = fixture.database.indexed_documents();
    let mut references = Vec::new();

    for (document_index, document) in documents.iter().enumerate() {
        if document.uri == common_uri {
            references.extend(
                document
                    .syntax
                    .references(target.id)
                    .map(|reference| (document_index, reference.range)),
            );
            continue;
        }

        let Some(name_id) = document.syntax.name_id(&document.text, name) else {
            continue;
        };
        let imports = analysis::imports(document);
        let Some(alias) = imports.iter().find_map(|import| {
            let target_document = fixture.database.import_target(&document.uri, import.path)?;
            (target_document.uri == common_uri)
                .then(|| import.alias_text(&document.text))
                .flatten()
        }) else {
            continue;
        };
        for reference in document.syntax.references_named(name_id) {
            if reference.qualifier.is_some_and(|qualifier| {
                document.syntax.name_text(&document.text, qualifier) == alias
            }) {
                references.push((document_index, reference.range));
            }
        }
    }
    references.sort_unstable_by_key(|(file, range)| (*file, range.start, range.end));
    std::hint::black_box(references)
}

#[library_benchmark]
#[bench::sixteen_revisions(LoadedWorkspace::default())]
fn workspace_burst_revision_invalidation(
    mut fixture: LoadedWorkspace,
) -> Vec<(i32, bool, Vec<url::Url>)> {
    let common_uri = url::Url::from_file_path(&fixture.files.paths[WORKSPACE_FILE_COUNT - 1])
        .must_be("common URI");
    let mut results = Vec::new();
    for revision in 2..=17 {
        let mut text = fixture.files.common_text.clone();
        writeln!(text, "# revision {revision}").must_be("append revision marker");
        let snapshot = std::sync::Arc::new(analysis::DocumentSnapshot::new(
            analysis::Revision(revision),
            text,
        ));
        let (id, imports_changed) = fixture
            .database
            .update_open_snapshot(&common_uri, snapshot)
            .must_be("update common snapshot");
        if imports_changed {
            fixture.database.load_reachable(std::slice::from_ref(&id));
        }
        let dependents = fixture
            .database
            .dependents(id)
            .into_iter()
            .map(|document| document.uri)
            .collect();
        results.push((revision, imports_changed, dependents));
    }
    std::hint::black_box(results)
}

#[library_benchmark]
#[bench::prewarmed(LazyLock::force(&CONSTRUCTOR_SNAPSHOT))]
fn constructor_definition_warm(
    snapshot: &analysis::DocumentSnapshot,
) -> Option<analysis::TextRange> {
    std::hint::black_box(analysis::declaration_range(
        std::hint::black_box(snapshot),
        "TermVar",
    ))
}

#[library_benchmark]
#[bench::builtin(parameter_annotation_case(0))]
#[bench::qualified(parameter_annotation_case(1))]
#[bench::nested(parameter_annotation_case(2))]
fn parameter_annotation_warm(
    (snapshot, parameter): (&analysis::DocumentSnapshot, analysis::SymbolId),
) -> Option<analysis::TextRange> {
    std::hint::black_box(snapshot.syntax.binding_type_range(
        std::hint::black_box(&snapshot.text),
        std::hint::black_box(parameter),
    ))
}

#[library_benchmark]
#[bench::sparse_100(SemanticSources::new(100, 3))]
#[bench::sparse_1000(SemanticSources::new(1_000, 3))]
#[bench::sparse_10000(SemanticSources::new(10_000, 3))]
#[bench::matched_100(SemanticSources::new(100, 99))]
#[bench::matched_1000(SemanticSources::new(1_000, 999))]
#[bench::matched_10000(SemanticSources::new(10_000, 9_999))]
#[bench::interleaved_100(SemanticSources::interleaved(100))]
fn cold_workspace_semantic_build(
    sources: SemanticSources,
) -> (tempfile::TempDir, workspace::WorkspaceDb) {
    std::hint::black_box(sources.build())
}

#[library_benchmark]
#[bench::sparse_100(SemanticWorkspace::new(100, 3))]
#[bench::sparse_1000(SemanticWorkspace::new(1_000, 3))]
#[bench::sparse_10000(SemanticWorkspace::new(10_000, 3))]
#[bench::matched_100(SemanticWorkspace::new(100, 99))]
#[bench::matched_1000(SemanticWorkspace::new(1_000, 999))]
#[bench::matched_10000(SemanticWorkspace::new(10_000, 9_999))]
fn cold_workspace_semantic_update(mut fixture: SemanticWorkspace) -> SemanticWorkspace {
    fixture
        .database
        .update_open_snapshot(&fixture.target_uri, fixture.replacement.clone())
        .must_be("semantic target replacement");
    std::hint::black_box(fixture)
}

#[library_benchmark]
#[bench::sparse_100(LazyLock::force(&SEMANTIC_SPARSE_100))]
#[bench::sparse_1000(LazyLock::force(&SEMANTIC_SPARSE_1000))]
#[bench::sparse_10000(LazyLock::force(&SEMANTIC_SPARSE_10000))]
#[bench::matched_100(LazyLock::force(&SEMANTIC_MATCHED_100))]
#[bench::matched_1000(LazyLock::force(&SEMANTIC_MATCHED_1000))]
#[bench::matched_10000(LazyLock::force(&SEMANTIC_MATCHED_10000))]
fn workspace_references_warm(fixture: &SemanticWorkspace) -> Vec<workspace::WorkspaceOccurrence> {
    std::hint::black_box(fixture.database.references(fixture.target, false))
}

#[library_benchmark]
#[bench::sparse_100(LazyLock::force(&SEMANTIC_SPARSE_100))]
#[bench::sparse_1000(LazyLock::force(&SEMANTIC_SPARSE_1000))]
#[bench::sparse_10000(LazyLock::force(&SEMANTIC_SPARSE_10000))]
#[bench::matched_100(LazyLock::force(&SEMANTIC_MATCHED_100))]
#[bench::matched_1000(LazyLock::force(&SEMANTIC_MATCHED_1000))]
#[bench::matched_10000(LazyLock::force(&SEMANTIC_MATCHED_10000))]
fn workspace_incoming_calls_warm(
    fixture: &SemanticWorkspace,
) -> Vec<workspace::WorkspaceCallGroup> {
    std::hint::black_box(fixture.database.incoming_calls(fixture.target))
}

#[library_benchmark]
#[bench::sparse_100(LazyLock::force(&SEMANTIC_SPARSE_100))]
#[bench::sparse_1000(LazyLock::force(&SEMANTIC_SPARSE_1000))]
#[bench::sparse_10000(LazyLock::force(&SEMANTIC_SPARSE_10000))]
#[bench::matched_100(LazyLock::force(&SEMANTIC_MATCHED_100))]
#[bench::matched_1000(LazyLock::force(&SEMANTIC_MATCHED_1000))]
#[bench::matched_10000(LazyLock::force(&SEMANTIC_MATCHED_10000))]
fn workspace_outgoing_calls_warm(
    fixture: &SemanticWorkspace,
) -> Vec<workspace::WorkspaceCallGroup> {
    std::hint::black_box(fixture.database.outgoing_calls(fixture.caller))
}

#[library_benchmark]
#[bench::absent_100(LazyLock::force(&REFERENCE_LOOKUP_ABSENT_100))]
#[bench::absent_1000(LazyLock::force(&REFERENCE_LOOKUP_ABSENT_1000))]
#[bench::absent_10000(LazyLock::force(&REFERENCE_LOOKUP_ABSENT_10000))]
#[bench::single_100(LazyLock::force(&REFERENCE_LOOKUP_SINGLE_100))]
#[bench::single_1000(LazyLock::force(&REFERENCE_LOOKUP_SINGLE_1000))]
#[bench::single_10000(LazyLock::force(&REFERENCE_LOOKUP_SINGLE_10000))]
#[bench::three_100(LazyLock::force(&REFERENCE_LOOKUP_THREE_100))]
#[bench::three_1000(LazyLock::force(&REFERENCE_LOOKUP_THREE_1000))]
#[bench::three_10000(LazyLock::force(&REFERENCE_LOOKUP_THREE_10000))]
fn workspace_reference_lookup_warm(
    fixture: &ReferenceLookupWorkspace,
) -> (usize, usize, Option<workspace::FileId>) {
    std::hint::black_box(
        std::hint::black_box(&fixture.database)
            .external_reference_groups(std::hint::black_box(fixture.target))
            .fold((0, 0, None), |(groups, occurrences, source), group| {
                (
                    groups + 1,
                    occurrences + std::hint::black_box(group.occurrence_count()),
                    source.max(Some(std::hint::black_box(group.source()))),
                )
            }),
    )
}

library_benchmark_group!(
    name = analysis_hot_paths;
    benchmarks = cold_snapshot_build_small, cold_snapshot_build_medium, cold_snapshot_build_large,
        semantic_tokens_warm, completion_warm, identifier_ranges_warm, references_warm,
        call_hierarchy_warm, inlay_hints_warm,
        ascii_position_conversion, ascii_offset_conversion,
        unicode_position_conversion, unicode_offset_conversion,
        folding_100_lines, folding_1000_lines, folding_10000_lines,
        workspace_initial_build, workspace_incremental_invalidation,
        workspace_references, workspace_burst_revision_invalidation,
        constructor_definition_warm, parameter_annotation_warm,
        cold_workspace_semantic_build, cold_workspace_semantic_update,
        workspace_references_warm, workspace_incoming_calls_warm, workspace_outgoing_calls_warm,
        workspace_reference_lookup_warm
);

main!(library_benchmark_groups = analysis_hot_paths);

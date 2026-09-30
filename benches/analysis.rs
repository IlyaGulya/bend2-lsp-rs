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

library_benchmark_group!(
    name = analysis_hot_paths;
    benchmarks = cold_snapshot_build_small, cold_snapshot_build_medium, cold_snapshot_build_large,
        semantic_tokens_warm, completion_warm, identifier_ranges_warm, references_warm,
        call_hierarchy_warm, inlay_hints_warm,
        ascii_position_conversion, ascii_offset_conversion,
        unicode_position_conversion, unicode_offset_conversion,
        folding_100_lines, folding_1000_lines, folding_10000_lines,
        workspace_initial_build, workspace_incremental_invalidation,
        workspace_references, workspace_burst_revision_invalidation
);

main!(library_benchmark_groups = analysis_hot_paths);

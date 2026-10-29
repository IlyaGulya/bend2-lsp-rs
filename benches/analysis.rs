mod support;

use std::{fmt::Write as _, fs, path::PathBuf, sync::LazyLock};
use support::Must;

use bend2_lsp::{analysis, workspace};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use sha2::{Digest as _, Sha256};

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

const COMPLETION_SOURCE: &str = "type Shape is Data:\n  Circle{}\ntype Wrapped is Data:\n  Box{value: Shape}\ndef main(value: Wrapped):\n  match value:\n    case Box{Ci";
static COMPLETION_SNAPSHOT: LazyLock<analysis::DocumentSnapshot> = LazyLock::new(|| {
    analysis::DocumentSnapshot::new(analysis::Revision(0), COMPLETION_SOURCE.to_owned())
});

const METADATA_SOURCE: &str = concat!(
    "# Choose a dependent function. 🦀\n",
    "# Keep the written multiline return annotation.\n",
    "def choose(\n",
    "  ~A: Type,\n",
    "  value: A,\n",
    "  project: (A -> A),\n",
    ") ->\n",
    "  (A -> A):\n",
    "  project\n",
    "# This belongs to next, not choose.\n",
    "def next() -> Type:\n",
    "  Type\n",
);
static METADATA_SNAPSHOT: LazyLock<analysis::DocumentSnapshot> = LazyLock::new(|| {
    analysis::DocumentSnapshot::new(analysis::Revision(0), METADATA_SOURCE.to_owned())
});

fn metadata_case() -> (&'static analysis::DocumentSnapshot, analysis::SymbolId) {
    let snapshot = LazyLock::force(&METADATA_SNAPSHOT);
    let declaration =
        analysis::declaration_metadata_by_name(snapshot, "choose").must_be("choose metadata");
    (snapshot, declaration.symbol)
}

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

fn scoped_completion_case(fixture: &WarmSnapshot) -> (&'static analysis::DocumentSnapshot, usize) {
    let declaration = fixture
        .snapshot
        .syntax
        .symbols()
        .iter()
        .find(|symbol| symbol.kind == analysis::SymbolKind::Function)
        .must_be("function with local parameters");
    (fixture.snapshot, declaration.scope.end.saturating_sub(1))
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

static IMPORT_COMPLETION_WORKSPACE: LazyLock<(LoadedWorkspace, url::Url)> = LazyLock::new(|| {
    let workspace = LoadedWorkspace::default();
    let source =
        url::Url::from_file_path(&workspace.files.paths[0]).must_be("completion source URI");
    (workspace, source)
});

#[library_benchmark]
#[bench::exact(LazyLock::force(&IMPORT_COMPLETION_WORKSPACE), "identity")]
#[bench::fuzzy(LazyLock::force(&IMPORT_COMPLETION_WORKSPACE), "idty")]
#[bench::no_match(LazyLock::force(&IMPORT_COMPLETION_WORKSPACE), "no_such_importable_name")]
fn import_completion_candidates_warm(
    fixture: &(LoadedWorkspace, url::Url),
    prefix: &str,
) -> Vec<(workspace::Document, String, Vec<analysis::Completion>)> {
    std::hint::black_box(fixture.0.database.import_completion_candidates(
        std::hint::black_box(&fixture.1),
        std::hint::black_box(prefix),
        false,
    ))
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
#[bench::small(scoped_completion_case(&WarmSnapshot::small()))]
#[bench::medium(scoped_completion_case(&WarmSnapshot::medium()))]
#[bench::large(scoped_completion_case(&WarmSnapshot::large()))]
fn scoped_completion_warm(
    (snapshot, offset): (&analysis::DocumentSnapshot, usize),
) -> Vec<analysis::Completion> {
    std::hint::black_box(analysis::scoped_completion_items(
        std::hint::black_box(snapshot),
        std::hint::black_box(offset),
        std::hint::black_box(""),
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
#[bench::empty(LoadedWorkspace::default(), "")]
#[bench::case_insensitive(LoadedWorkspace::default(), "CoMmOn_WoRkEr_")]
fn workspace_symbols_warm(
    fixture: LoadedWorkspace,
    query: &str,
) -> Vec<(usize, String, analysis::SymbolKind, analysis::TextRange)> {
    let query = analysis::WorkspaceSymbolQuery::new(std::hint::black_box(query));
    let documents = fixture.database.indexed_documents();
    let mut symbols = Vec::new();
    for (document_index, document) in documents.iter().enumerate() {
        symbols.extend(query.symbols(document).map(|symbol| {
            (
                document_index,
                symbol.name.to_owned(),
                symbol.kind,
                symbol.range,
            )
        }));
    }
    std::hint::black_box(symbols)
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
#[bench::hundred_files(InitialWorkspace::default())]
fn workspace_orphan_graph_release(mut fixture: InitialWorkspace) -> usize {
    let root_uri = url::Url::from_file_path(&fixture.files.paths[0]).must_be("root URI");
    let root = fixture.database.set_open_document(
        workspace::Document::new(
            root_uri.clone(),
            "bend".into(),
            analysis::Revision(1),
            std::mem::take(&mut fixture.files.root_text),
        ),
        Some(fixture.files.paths[0].clone()),
    );
    fixture.database.load_reachable(std::slice::from_ref(&root));
    let snapshot = std::sync::Arc::new(analysis::DocumentSnapshot::new(
        analysis::Revision(2),
        "def detached: U32\n  0\n".to_owned(),
    ));
    fixture
        .database
        .update_open_snapshot(&root_uri, snapshot)
        .must_be("detach imported graph");
    fixture.database.load_reachable(std::slice::from_ref(&root));
    let closed = fixture
        .database
        .close_document(&root_uri)
        .must_be("close root");
    fixture
        .database
        .load_reachable(std::slice::from_ref(&closed));
    std::hint::black_box(fixture.database.indexed_documents().len())
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
#[bench::prefix(LazyLock::force(&CONSTRUCTOR_SNAPSHOT), "Term")]
#[bench::empty(LazyLock::force(&CONSTRUCTOR_SNAPSHOT), "")]
#[bench::subsequence(LazyLock::force(&CONSTRUCTOR_SNAPSHOT), "TV")]
fn constructor_completion_warm(
    snapshot: &analysis::DocumentSnapshot,
    prefix: &str,
) -> Vec<analysis::Completion> {
    std::hint::black_box(analysis::constructor_completion_items(
        std::hint::black_box(snapshot),
        None,
        std::hint::black_box(prefix),
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
fn cold_completion_context_build() -> analysis::DocumentSnapshot {
    std::hint::black_box(analysis::DocumentSnapshot::new(
        analysis::Revision(0),
        std::hint::black_box(COMPLETION_SOURCE.to_owned()),
    ))
}

#[library_benchmark]
fn cold_snapshot_build_metadata() -> analysis::DocumentSnapshot {
    std::hint::black_box(analysis::DocumentSnapshot::new(
        analysis::Revision(0),
        std::hint::black_box(METADATA_SOURCE).to_owned(),
    ))
}

#[library_benchmark]
#[bench::nested(LazyLock::force(&COMPLETION_SNAPSHOT))]
fn pattern_completion_context_warm(
    snapshot: &analysis::DocumentSnapshot,
) -> Option<analysis::CasePatternType> {
    std::hint::black_box(
        snapshot
            .syntax
            .case_pattern_type(std::hint::black_box(snapshot.text.len())),
    )
}

fn workspace_symbol_response_case() -> Vec<tower_lsp::lsp_types::SymbolInformation> {
    use tower_lsp::lsp_types::{Location, Position, Range, SymbolInformation, SymbolKind};

    (0..10_000)
        .map(|index| {
            let (name, line) = match index {
                0 => ("identity".to_owned(), 0),
                1 => ("main".to_owned(), 1),
                index => (format!("consumer_{:04}", index - 2), 1),
            };
            let filename = match name.as_str() {
                "identity" => "common",
                "main" => "entry",
                name => name,
            };
            let uri = url::Url::parse(&format!(
                "file:///tmp/bend2-workspace-symbol-response/{filename}.bend"
            ))
            .must_be("symbol URI");
            let end = 4 + u32::try_from(name.len()).must_be("symbol name length");
            SymbolInformation {
                name,
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                location: Location {
                    uri,
                    range: Range::new(Position::new(line, 4), Position::new(line, end)),
                },
                container_name: None,
            }
        })
        .collect()
}

#[library_benchmark]
#[bench::ten_thousand_symbols(workspace_symbol_response_case())]
fn workspace_symbol_response_warm(
    symbols: Vec<tower_lsp::lsp_types::SymbolInformation>,
) -> Vec<u8> {
    use tower_lsp::jsonrpc::{Error, IntoResponse};

    let response = Ok::<_, Error>(Some(symbols))
        .into_response(Some(17.into()))
        .must_be("workspace symbol response");
    std::hint::black_box(response.to_json_bytes().must_be("JSON-RPC wire response"))
}

#[library_benchmark]
#[bench::prewarmed(metadata_case())]
fn declaration_metadata_warm(
    (snapshot, declaration): (&'static analysis::DocumentSnapshot, analysis::SymbolId),
) -> Option<&'static analysis::DeclarationSite> {
    std::hint::black_box(analysis::declaration_metadata(
        std::hint::black_box(snapshot),
        std::hint::black_box(declaration),
    ))
}

#[library_benchmark]
#[bench::prewarmed(metadata_case())]
fn declaration_documentation_warm(
    (snapshot, declaration): (&'static analysis::DocumentSnapshot, analysis::SymbolId),
) -> Option<&'static str> {
    std::hint::black_box(analysis::declaration_documentation(
        std::hint::black_box(snapshot),
        std::hint::black_box(declaration),
    ))
}

#[library_benchmark]
#[bench::prewarmed(metadata_case())]
fn declaration_hover_warm(
    (snapshot, declaration): (&analysis::DocumentSnapshot, analysis::SymbolId),
) -> Option<String> {
    std::hint::black_box(analysis::declaration_hover_by_id(
        std::hint::black_box(snapshot),
        std::hint::black_box(declaration),
    ))
}

#[library_benchmark]
#[bench::prewarmed(metadata_case())]
fn signature_help_warm(
    (snapshot, declaration): (&analysis::DocumentSnapshot, analysis::SymbolId),
) -> Option<analysis::SignatureHelp> {
    std::hint::black_box(analysis::signature_help_for_symbol(
        std::hint::black_box(snapshot),
        std::hint::black_box(declaration),
        std::hint::black_box(2),
    ))
}

// These fixtures prepare file facts and views outside the measured warm calls.
// Owned cold inputs become returned views; update fixtures are returned as well,
// so neither benchmark includes database/source teardown in its measured body.
struct SemanticFixture {
    database: workspace::WorkspaceDb,
    roots: Vec<workspace::FileId>,
    views: Vec<std::sync::Arc<workspace::SemanticView>>,
    dependency: url::Url,
    next_snapshot: std::sync::Arc<analysis::DocumentSnapshot>,
    offset: usize,
    target: workspace::SemanticTarget,
    caller: workspace::SemanticEntityId,
}

impl SemanticFixture {
    fn new(declarations: usize, roots: usize, export_change: bool) -> Self {
        let directory = std::env::temp_dir().join("bend2-semantic-benchmark");
        let dependency_path = directory.join("dep.bend");
        let dependency =
            url::Url::from_file_path(&dependency_path).must_be("semantic dependency URI");
        let mut source = String::from("def target(value: U32) -> U32:\n  value\n");
        for index in 0..declarations {
            writeln!(source, "def unused_{index}(value: U32) -> U32:\n  value")
                .must_be("semantic declaration");
        }
        let mut database = workspace::WorkspaceDb::default();
        let mut entries = Vec::with_capacity(roots);
        let entry_source =
            "import ./dep.bend as D\ndef main(value: U32) -> U32:\n  D.target!(value)\n";
        for index in 0..roots {
            let path = directory.join(format!("entry_{index}.bend"));
            let root = database.set_open_document(
                workspace::Document::new(
                    url::Url::from_file_path(&path).must_be("semantic entry URI"),
                    "bend".into(),
                    analysis::Revision(1),
                    entry_source.to_owned(),
                ),
                Some(path),
            );
            entries.push(root);
        }
        database.set_open_document(
            workspace::Document::new(
                dependency.clone(),
                "bend".into(),
                analysis::Revision(1),
                source.clone(),
            ),
            Some(dependency_path),
        );
        let mut views = Vec::with_capacity(roots);
        for root in &entries {
            let view = std::sync::Arc::new(
                database
                    .capture_semantic_view(*root, None)
                    .must_be("semantic input")
                    .build(),
            );
            assert!(database.publish_semantic_view(view.clone(), None));
            views.push(view);
        }
        let offset = entry_source
            .find("D.target")
            .must_be("semantic call offset");
        let target = views[0]
            .resolve_at(entries[0], offset)
            .must_be("semantic call")
            .target;
        let caller = views[0]
            .resolve_at(entries[0], offset)
            .must_be("semantic caller")
            .enclosing
            .must_be("caller entity");
        assert!(matches!(target, workspace::SemanticTarget::Global(_)));
        // Construct the next file facts before measuring an update; no parser
        // or source-generation work is hidden inside the view-update row.
        let revised = if export_change {
            source.replacen("def target", "def replacement", 1)
        } else {
            source.replacen("\n  value\n", "\n  0\n", 1)
        };
        Self {
            database,
            roots: entries,
            views,
            dependency,
            offset,
            target,
            caller,
            next_snapshot: std::sync::Arc::new(analysis::DocumentSnapshot::new(
                analysis::Revision(2),
                revised,
            )),
        }
    }

    fn with_unrelated_files(mut self, count: usize) -> Self {
        for index in 0..count {
            let path = std::env::temp_dir()
                .join("bend2-semantic-benchmark")
                .join(format!("unrelated_{index}.bend"));
            self.database.set_open_document(
                workspace::Document::new(
                    url::Url::from_file_path(&path).must_be("unrelated source URI"),
                    "bend".into(),
                    analysis::Revision(1),
                    "def noise(x):\n  x\n".to_owned(),
                ),
                Some(path),
            );
        }
        self
    }
}

static SEMANTIC_100: LazyLock<SemanticFixture> =
    LazyLock::new(|| SemanticFixture::new(100, 1, false));
static SEMANTIC_1000: LazyLock<SemanticFixture> =
    LazyLock::new(|| SemanticFixture::new(1_000, 1, false));
static SEMANTIC_10000: LazyLock<SemanticFixture> =
    LazyLock::new(|| SemanticFixture::new(10_000, 1, false));
static SEMANTIC_ROOTS_2: LazyLock<SemanticFixture> =
    LazyLock::new(|| SemanticFixture::new(100, 2, false));
static SEMANTIC_ROOTS_8: LazyLock<SemanticFixture> =
    LazyLock::new(|| SemanticFixture::new(100, 8, false));
static SEMANTIC_SPARSE_100: LazyLock<SemanticFixture> =
    LazyLock::new(|| SemanticFixture::new(1, 1, false).with_unrelated_files(100));
static SEMANTIC_SPARSE_1000: LazyLock<SemanticFixture> =
    LazyLock::new(|| SemanticFixture::new(1, 1, false).with_unrelated_files(1_000));
static SEMANTIC_SPARSE_10000: LazyLock<SemanticFixture> =
    LazyLock::new(|| SemanticFixture::new(1, 1, false).with_unrelated_files(10_000));

fn semantic_input(declarations: usize) -> workspace::SemanticViewInput {
    let fixture = match declarations {
        100 => LazyLock::force(&SEMANTIC_100),
        1_000 => LazyLock::force(&SEMANTIC_1000),
        _ => LazyLock::force(&SEMANTIC_10000),
    };
    fixture
        .database
        .capture_semantic_view(fixture.roots[0], None)
        .must_be("captured facts")
}

#[library_benchmark]
#[bench::declarations_100(semantic_input(100))]
#[bench::declarations_1000(semantic_input(1_000))]
#[bench::declarations_10000(semantic_input(10_000))]
fn semantic_view_cold(input: workspace::SemanticViewInput) -> workspace::SemanticView {
    std::hint::black_box(input.build())
}

#[library_benchmark]
#[bench::one_view(SemanticFixture::new(100, 1, false))]
#[bench::two_views(SemanticFixture::new(100, 2, false))]
#[bench::eight_views(SemanticFixture::new(100, 8, false))]
#[bench::export_change(SemanticFixture::new(100, 1, true))]
fn semantic_views_preparsed_update(mut fixture: SemanticFixture) -> SemanticFixture {
    fixture
        .database
        .update_open_snapshot(&fixture.dependency, fixture.next_snapshot.clone())
        .must_be("apply prepared semantic revision");
    fixture.views.clear();
    for root in &fixture.roots {
        let view = std::sync::Arc::new(
            fixture
                .database
                .capture_semantic_view(*root, None)
                .must_be("capture changed context")
                .build(),
        );
        assert!(fixture.database.publish_semantic_view(view.clone(), None));
        fixture.views.push(view);
    }
    let occurrence = fixture.views[0]
        .resolve_at(fixture.roots[0], fixture.offset)
        .must_be("changed semantic occurrence");
    fixture.target = occurrence.target;
    fixture.caller = occurrence.enclosing.must_be("changed caller");
    std::hint::black_box(fixture)
}

#[library_benchmark]
#[bench::unrelated_100(LazyLock::force(&SEMANTIC_100))]
#[bench::unrelated_1000(LazyLock::force(&SEMANTIC_1000))]
#[bench::unrelated_10000(LazyLock::force(&SEMANTIC_10000))]
fn semantic_resolve_warm(
    fixture: &'static SemanticFixture,
) -> Option<&'static workspace::SemanticOccurrence> {
    std::hint::black_box(fixture.views[0].resolve_at(
        std::hint::black_box(fixture.roots[0]),
        std::hint::black_box(fixture.offset),
    ))
}

#[library_benchmark]
#[bench::unrelated_100(LazyLock::force(&SEMANTIC_100))]
#[bench::unrelated_1000(LazyLock::force(&SEMANTIC_1000))]
#[bench::unrelated_10000(LazyLock::force(&SEMANTIC_10000))]
fn semantic_references_warm(
    fixture: &'static SemanticFixture,
) -> &'static [workspace::SemanticOccurrence] {
    std::hint::black_box(fixture.views[0].references(std::hint::black_box(fixture.target)))
}

#[library_benchmark]
#[bench::unrelated_100(LazyLock::force(&SEMANTIC_100))]
#[bench::unrelated_1000(LazyLock::force(&SEMANTIC_1000))]
#[bench::unrelated_10000(LazyLock::force(&SEMANTIC_10000))]
fn semantic_incoming_calls_warm(fixture: &SemanticFixture) -> usize {
    std::hint::black_box(
        fixture.views[0]
            .calls_to(std::hint::black_box(fixture.target))
            .fold(0, |count, call| {
                std::hint::black_box(call);
                count + 1
            }),
    )
}

#[library_benchmark]
#[bench::unrelated_100(LazyLock::force(&SEMANTIC_100))]
#[bench::unrelated_1000(LazyLock::force(&SEMANTIC_1000))]
#[bench::unrelated_10000(LazyLock::force(&SEMANTIC_10000))]
fn semantic_outgoing_calls_warm(fixture: &SemanticFixture) -> usize {
    std::hint::black_box(
        fixture.views[0]
            .calls_from(std::hint::black_box(fixture.caller))
            .fold(0, |count, call| {
                std::hint::black_box(call);
                count + 1
            }),
    )
}

#[library_benchmark]
#[bench::unrelated_100(LazyLock::force(&SEMANTIC_100))]
#[bench::unrelated_1000(LazyLock::force(&SEMANTIC_1000))]
#[bench::unrelated_10000(LazyLock::force(&SEMANTIC_10000))]
fn semantic_view_reuse_warm(
    fixture: &SemanticFixture,
) -> Option<std::sync::Arc<workspace::SemanticView>> {
    std::hint::black_box(
        fixture
            .database
            .semantic_view(std::hint::black_box(fixture.roots[0])),
    )
}

#[library_benchmark]
#[bench::declarations_100(LazyLock::force(&SEMANTIC_100))]
#[bench::declarations_1000(LazyLock::force(&SEMANTIC_1000))]
#[bench::declarations_10000(LazyLock::force(&SEMANTIC_10000))]
fn semantic_view_capture(fixture: &SemanticFixture) -> workspace::SemanticViewInput {
    std::hint::black_box(
        fixture
            .database
            .capture_semantic_view(fixture.roots[0], None)
            .must_be("captured context"),
    )
}

#[library_benchmark]
#[bench::unrelated_100(LazyLock::force(&SEMANTIC_100))]
#[bench::unrelated_1000(LazyLock::force(&SEMANTIC_1000))]
#[bench::unrelated_10000(LazyLock::force(&SEMANTIC_10000))]
fn semantic_completion_warm(fixture: &SemanticFixture) -> Vec<workspace::SemanticCompletion> {
    std::hint::black_box(fixture.views[0].completion_candidates(
        std::hint::black_box(fixture.roots[0]),
        std::hint::black_box("D.tar"),
    ))
}

#[library_benchmark]
#[bench::declarations_100(LazyLock::force(&SEMANTIC_100))]
#[bench::declarations_1000(LazyLock::force(&SEMANTIC_1000))]
#[bench::declarations_10000(LazyLock::force(&SEMANTIC_10000))]
fn semantic_rename_preflight_warm(fixture: &SemanticFixture) -> bool {
    std::hint::black_box(fixture.views[0].rename_is_safe(
        std::hint::black_box(fixture.target),
        std::hint::black_box("renamed"),
    ))
}

#[library_benchmark]
#[bench::unrelated_files_100(LazyLock::force(&SEMANTIC_SPARSE_100))]
#[bench::unrelated_files_1000(LazyLock::force(&SEMANTIC_SPARSE_1000))]
#[bench::unrelated_files_10000(LazyLock::force(&SEMANTIC_SPARSE_10000))]
fn semantic_sparse_view_reuse_warm(
    fixture: &SemanticFixture,
) -> Option<std::sync::Arc<workspace::SemanticView>> {
    std::hint::black_box(
        fixture
            .database
            .semantic_view(std::hint::black_box(fixture.roots[0])),
    )
}

#[library_benchmark]
#[bench::unrelated_files_100(LazyLock::force(&SEMANTIC_SPARSE_100))]
#[bench::unrelated_files_1000(LazyLock::force(&SEMANTIC_SPARSE_1000))]
#[bench::unrelated_files_10000(LazyLock::force(&SEMANTIC_SPARSE_10000))]
fn semantic_sparse_relevant_views_warm(
    fixture: &SemanticFixture,
) -> Vec<std::sync::Arc<workspace::SemanticView>> {
    let file = fixture
        .database
        .file_id_by_uri(&fixture.dependency)
        .must_be("dependency identity");
    std::hint::black_box(fixture.database.views_for_file(std::hint::black_box(file)))
}

#[library_benchmark]
#[bench::one_view(LazyLock::force(&SEMANTIC_100))]
#[bench::two_views(LazyLock::force(&SEMANTIC_ROOTS_2))]
#[bench::eight_views(LazyLock::force(&SEMANTIC_ROOTS_8))]
fn semantic_shared_source_ownership(fixture: &SemanticFixture) -> (usize, usize) {
    let mut snapshots = std::collections::HashSet::new();
    let mut unique_source_bytes = 0;
    let mut logical_source_bytes = 0;
    for view in &fixture.views {
        for document in view.documents() {
            logical_source_bytes += document.text.len();
            if snapshots.insert(std::sync::Arc::as_ptr(&document.snapshot)) {
                unique_source_bytes += document.text.len();
            }
        }
    }
    std::hint::black_box((unique_source_bytes, logical_source_bytes))
}

// Diagnostic-only scope and orphan-release rows (AC53.6/AC53.7). They live in a
// separate group, are unpaired, and do not appear in the frozen paired workload
// manifest. The hosted harness that runs them is
// `.github/workflows/performance-scope-diagnostics.yml`. Every fixture is fully
// built and asserted before the measured call; the receipt line can therefore
// only exist when the intended cold construction or published view was real.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn record_scope_diagnostic(row: &str, facts: &str) {
    use std::io::Write as _;
    let Ok(path) = std::env::var("BEND2_SCOPE_DIAGNOSTIC_RECEIPT") else {
        return;
    };
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .must_be("scope diagnostic receipt file");
    writeln!(file, "row={row} {facts}").must_be("scope diagnostic receipt line");
}

fn nested_scope_source(depth: usize) -> String {
    let mut source = String::from("def nested(value: U32) -> U32:\n  ");
    for index in 0..depth {
        let _ = write!(source, "x{index} => ");
    }
    source.push_str("value\n");
    source
}

fn verified_nested_scope_source(depth: usize, row: &str) -> String {
    let source = nested_scope_source(depth);
    let snapshot = analysis::DocumentSnapshot::new(analysis::Revision(0), source.clone());
    let syntax = &snapshot.syntax;
    assert_eq!(syntax.symbols().len(), 1, "row {row}: one declaration");
    assert_eq!(
        syntax.declaration_sites().len(),
        1,
        "row {row}: one declaration site"
    );
    assert!(
        !syntax.declaration_sites()[0].uncertain,
        "row {row}: definite declaration body"
    );
    assert!(
        syntax.diagnostics().is_empty(),
        "row {row}: no syntax diagnostics"
    );
    assert!(
        syntax.scopes().len() >= depth + 1,
        "row {row}: definition scope plus one scope per lambda"
    );
    assert!(
        syntax.bindings().len() >= depth + 1,
        "row {row}: parameter plus one binder per lambda"
    );
    let lexical_depth = syntax
        .scopes()
        .iter()
        .map(|scope| {
            let mut chain = 1_usize;
            let mut parent = scope.parent;
            while let Some(id) = parent {
                chain += 1;
                parent = syntax.scopes()[id.0].parent;
            }
            chain
        })
        .max()
        .must_be("nested lexical scope chain");
    assert_eq!(lexical_depth, depth + 1, "row {row}: nesting depth");
    record_scope_diagnostic(
        row,
        &format!(
            "kind=nested_scope depth={depth} source_bytes={} source_sha256={} scopes={} lexical_depth={lexical_depth} bindings={} symbols={}",
            source.len(),
            sha256_hex(source.as_bytes()),
            syntax.scopes().len(),
            syntax.bindings().len(),
            syntax.symbols().len(),
        ),
    );
    source
}

struct PublishedViewFixture {
    _directory: tempfile::TempDir,
    database: workspace::WorkspaceDb,
    roots: Vec<workspace::FileId>,
    root_uris: Vec<url::Url>,
    dependency_uris: Vec<url::Url>,
    root_snapshot: std::sync::Weak<analysis::DocumentSnapshot>,
    dependency_snapshot: std::sync::Weak<analysis::DocumentSnapshot>,
    retained: Option<std::sync::Arc<workspace::SemanticView>>,
}

fn dependency_source(declarations: usize) -> String {
    let mut source = String::from("def target(value: U32) -> U32:\n  value\n");
    for declaration in 0..declarations {
        let _ = writeln!(
            source,
            "def unused_{declaration}(value: U32) -> U32:\n  value"
        );
    }
    source
}

fn published_root_source(dependencies: usize) -> String {
    let mut source = String::new();
    for index in 0..dependencies {
        let _ = writeln!(source, "import ./d{index:03}.bend as D{index:03}");
    }
    source.push_str("def main(value: U32) -> U32:\n  ");
    for index in 0..dependencies {
        let _ = write!(source, "D{index:03}.target!(");
    }
    source.push_str("value");
    for _ in 0..dependencies {
        source.push(')');
    }
    source.push('\n');
    source
}

fn published_view_fixture(
    row: &str,
    dependencies: usize,
    declarations: usize,
    roots: usize,
    retain_reader: bool,
) -> PublishedViewFixture {
    let directory = tempfile::tempdir().must_be("published view fixture directory");
    let mut database = workspace::WorkspaceDb::default();
    let mut dependency_uris = Vec::with_capacity(dependencies);
    let dependency_source_text = dependency_source(declarations);
    for index in 0..dependencies {
        let path = directory.path().join(format!("d{index:03}.bend"));
        fs::write(&path, &dependency_source_text).must_be("write published dependency source");
        dependency_uris.push(url::Url::from_file_path(&path).must_be("published dependency URI"));
    }
    let root_source = published_root_source(dependencies);
    let mut root_ids = Vec::with_capacity(roots);
    let mut root_uris = Vec::with_capacity(roots);
    let mut root_snapshot = None;
    for index in 0..roots {
        let path = directory.path().join(format!("entry_{index:03}.bend"));
        let uri = url::Url::from_file_path(&path).must_be("published entry URI");
        let root = database.set_open_document(
            workspace::Document::new(
                uri.clone(),
                "bend".into(),
                analysis::Revision(1),
                root_source.clone(),
            ),
            Some(path),
        );
        if index == 0 {
            let document = database
                .cached_document(&uri)
                .must_be("open published root document");
            root_snapshot = Some(std::sync::Arc::downgrade(&document.snapshot));
        }
        root_ids.push(root);
        root_uris.push(uri);
    }
    // Dependencies are real disk-loaded graph ownership, not open overlays, so
    // closing the last root can actually orphan and release them.
    database.load_reachable(&root_ids);
    let dependency_snapshot = {
        let document = database
            .cached_document(&dependency_uris[0])
            .must_be("loaded published dependency");
        std::sync::Arc::downgrade(&document.snapshot)
    };
    let mut views = Vec::with_capacity(roots);
    for (index, root) in root_ids.iter().copied().enumerate() {
        let view = std::sync::Arc::new(
            database
                .capture_semantic_view(root, None)
                .must_be("captured published view")
                .build(),
        );
        assert!(
            database.publish_semantic_view(std::sync::Arc::clone(&view), None),
            "row {row}: published view {index} must validate"
        );
        assert!(
            database.semantic_view(root).is_some(),
            "row {row}: published view {index} must be reachable"
        );
        views.push(view);
    }
    let view = &views[0];
    assert_eq!(
        view.documents().len(),
        dependencies + 1,
        "row {row}: one document per dependency and entry"
    );
    let expected_declarations = dependencies * (declarations + 1) + 1;
    assert_eq!(
        view.declarations().len(),
        expected_declarations,
        "row {row}: every declaration site is published"
    );
    assert!(view.issues().is_empty(), "row {row}: published issues");
    for other in &views[1..] {
        assert_eq!(
            other.documents().len(),
            view.documents().len(),
            "row {row}: consistent root documents"
        );
        assert_eq!(
            other.declarations().len(),
            view.declarations().len(),
            "row {row}: consistent root declarations"
        );
    }
    let documents = view.documents().len();
    let view_declarations = view.declarations().len();
    let entities = view.entities().len();
    let calls = view.calls().len();
    let source_bytes: usize = view
        .documents()
        .iter()
        .map(|document| document.text.len())
        .sum();
    let retained = retain_reader.then_some(std::sync::Arc::clone(&views[0]));
    record_scope_diagnostic(
        row,
        &format!(
            "kind=published_view_close_release dependencies={dependencies} declarations_per_dependency={declarations} roots={roots} views_published={roots} files={} documents={documents} view_declarations={view_declarations} entities={entities} calls={calls} source_bytes={source_bytes} root_source_sha256={} dependency_source_sha256={} retained_reader={} cold_build=outside_measurement",
            dependencies + 1,
            sha256_hex(root_source.as_bytes()),
            sha256_hex(dependency_source_text.as_bytes()),
            u8::from(retain_reader),
        ),
    );
    drop(views);
    PublishedViewFixture {
        _directory: directory,
        database,
        roots: root_ids,
        root_uris,
        dependency_uris,
        root_snapshot: root_snapshot.must_be("root snapshot witness"),
        dependency_snapshot,
        retained,
    }
}

#[library_benchmark]
#[bench::depth_8(verified_nested_scope_source(8, "nested_depth_8"))]
#[bench::depth_32(verified_nested_scope_source(32, "nested_depth_32"))]
#[bench::depth_128(verified_nested_scope_source(128, "nested_depth_128"))]
fn cold_snapshot_nested_scope(source: String) -> analysis::DocumentSnapshot {
    std::hint::black_box(analysis::DocumentSnapshot::new(
        analysis::Revision(0),
        source,
    ))
}

#[library_benchmark]
#[bench::declarations_100(published_view_fixture("close_declarations_100", 1, 100, 1, false))]
#[bench::declarations_1000(published_view_fixture(
    "close_declarations_1000",
    1,
    1_000,
    1,
    false
))]
#[bench::declarations_10000(published_view_fixture(
    "close_declarations_10000",
    1,
    10_000,
    1,
    false
))]
#[bench::files_2(published_view_fixture("close_files_2", 2, 100, 1, false))]
#[bench::files_8(published_view_fixture("close_files_8", 8, 100, 1, false))]
#[bench::files_32(published_view_fixture("close_files_32", 32, 100, 1, false))]
#[bench::views_1(published_view_fixture("close_views_1", 1, 100, 1, false))]
#[bench::views_2(published_view_fixture("close_views_2", 1, 100, 2, false))]
#[bench::views_8(published_view_fixture("close_views_8", 1, 100, 8, false))]
fn published_semantic_view_close_orphan_release(
    mut fixture: PublishedViewFixture,
) -> PublishedViewFixture {
    for uri in &fixture.root_uris {
        let closed = fixture
            .database
            .close_document(uri)
            .must_be("close published view root");
        assert!(
            fixture.roots.contains(&closed),
            "close must report the opened root identity"
        );
    }
    // The close transaction marks the now-unreachable graph retired; the
    // lifecycle boundary that releases that database ownership is the same
    // empty reachability pass used after a real close in production.
    fixture.database.load_reachable(&[]);
    for root in &fixture.roots {
        assert!(
            fixture.database.semantic_view(*root).is_none(),
            "closing the last root must drop the published view"
        );
    }
    assert!(
        fixture.database.indexed_documents().is_empty(),
        "closing the last root must drop graph ownership"
    );
    for uri in fixture.root_uris.iter().chain(&fixture.dependency_uris) {
        assert!(
            fixture.database.cached_document(uri).is_none(),
            "closed root and orphaned dependency payloads must be released"
        );
    }
    assert!(
        fixture.root_snapshot.upgrade().is_none(),
        "closed root snapshot must be deallocated"
    );
    assert!(
        fixture.dependency_snapshot.upgrade().is_none(),
        "orphaned dependency snapshot must be deallocated"
    );
    assert!(fixture.retained.is_none(), "no retained reader in this row");
    std::hint::black_box(fixture)
}

#[library_benchmark]
#[bench::retained_10000(published_view_fixture(
    "close_retained_reader_10000",
    1,
    10_000,
    1,
    true
))]
fn published_semantic_view_close_orphan_release_retained_reader(
    mut fixture: PublishedViewFixture,
) -> PublishedViewFixture {
    for uri in &fixture.root_uris {
        fixture
            .database
            .close_document(uri)
            .must_be("close published view root with retained reader");
    }
    fixture.database.load_reachable(&[]);
    for root in &fixture.roots {
        assert!(
            fixture.database.semantic_view(*root).is_none(),
            "database view ownership must drop while a reader is retained"
        );
    }
    assert!(
        fixture.database.indexed_documents().is_empty(),
        "database graph ownership must drop while a reader is retained"
    );
    for uri in fixture.root_uris.iter().chain(&fixture.dependency_uris) {
        assert!(
            fixture.database.cached_document(uri).is_none(),
            "database payload ownership must drop while a reader is retained"
        );
    }
    assert!(
        fixture.root_snapshot.upgrade().is_some(),
        "the retained reader must keep the closed root contribution alive"
    );
    assert!(
        fixture.dependency_snapshot.upgrade().is_some(),
        "the retained reader must keep the orphaned graph contribution alive"
    );
    let retained = fixture.retained.as_ref().must_be("retained reader");
    assert_eq!(
        std::sync::Arc::strong_count(retained),
        1,
        "the retained reader is the last owner; its teardown stays outside the measured body"
    );
    std::hint::black_box(fixture)
}

library_benchmark_group!(
    name = analysis_scope_diagnostics;
    benchmarks = cold_snapshot_nested_scope, published_semantic_view_close_orphan_release,
        published_semantic_view_close_orphan_release_retained_reader
);

library_benchmark_group!(
    name = analysis_hot_paths;
    benchmarks = cold_snapshot_build_small, cold_snapshot_build_medium, cold_snapshot_build_large,
        semantic_tokens_warm, completion_warm, scoped_completion_warm, identifier_ranges_warm, references_warm,
        call_hierarchy_warm, inlay_hints_warm,
        ascii_position_conversion, ascii_offset_conversion,
        unicode_position_conversion, unicode_offset_conversion,
        folding_100_lines, folding_1000_lines, folding_10000_lines,
        workspace_initial_build, workspace_incremental_invalidation,
        workspace_references, workspace_burst_revision_invalidation, workspace_orphan_graph_release,
        constructor_definition_warm, constructor_completion_warm, parameter_annotation_warm,
        cold_completion_context_build, pattern_completion_context_warm,
        import_completion_candidates_warm, workspace_symbols_warm, workspace_symbol_response_warm,
        cold_snapshot_build_metadata, declaration_metadata_warm,
        declaration_documentation_warm, declaration_hover_warm, signature_help_warm,
        semantic_view_cold, semantic_views_preparsed_update, semantic_resolve_warm,
        semantic_references_warm, semantic_incoming_calls_warm, semantic_outgoing_calls_warm,
        semantic_view_reuse_warm, semantic_view_capture, semantic_shared_source_ownership,
        semantic_completion_warm, semantic_rename_preflight_warm,
        semantic_sparse_view_reuse_warm, semantic_sparse_relevant_views_warm
);

main!(library_benchmark_groups = analysis_scope_diagnostics, analysis_hot_paths);

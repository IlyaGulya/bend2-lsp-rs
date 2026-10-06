use std::{
    fmt::Write as _,
    fs,
    path::PathBuf,
    sync::{Arc, LazyLock},
};

use bend2_lsp::{
    analysis::{DocumentSnapshot, Revision},
    workspace::{Document, WorkspaceDb},
};

use super::Must;

const TARGET: &str = "def identity(value):\n  value\ndef alternate(value):\n  identity(value)\ndef local(value):\n  identity(value)\n";
const CLIENT: &str = "import ./nested/../target.bend as A\nimport ./target.bend as B\ndef helper(value):\n  value\ndef client(value):\n  A.identity(A.alternate(value))\n  B.identity(value)\n  A.identity\n  helper(value)\ndef second(value):\n  A.alternate(\"😀\", B.identity(value))\n  B.alternate(value)\n  helper\n";
const UNRELATED: &str = "def identity(value):\n  value\ndef unrelated(value):\n  identity(value)\n";

pub(super) struct Sources {
    pub directory: tempfile::TempDir,
    pub documents: Vec<(Document, PathBuf)>,
}

impl Sources {
    pub fn new(files: usize, matches: usize) -> Self {
        let directory = tempfile::tempdir().must_be("decomposition workspace");
        let target = Arc::new(DocumentSnapshot::new(Revision(1), TARGET.into()));
        let client = Arc::new(DocumentSnapshot::new(Revision(1), CLIENT.into()));
        let unrelated = Arc::new(DocumentSnapshot::new(Revision(1), UNRELATED.into()));
        let documents = (0..files)
            .map(|index| {
                let (name, snapshot) = if index == 0 {
                    ("target.bend".into(), Arc::clone(&target))
                } else if index <= matches {
                    (format!("client{index:05}.bend"), Arc::clone(&client))
                } else {
                    (format!("unrelated{index:05}.bend"), Arc::clone(&unrelated))
                };
                let path = directory.path().join(name);
                let uri = url::Url::from_file_path(&path).must_be("fixture URI");
                (Document::with_snapshot(uri, "bend".into(), snapshot), path)
            })
            .collect();
        Self {
            directory,
            documents,
        }
    }

    pub fn build(self) -> Fixture {
        let mut database = WorkspaceDb::default();
        for (document, path) in self.documents {
            database.set_open_document(document, Some(path));
        }
        Fixture {
            directory: self.directory,
            database,
        }
    }
}

pub(super) struct Fixture {
    pub directory: tempfile::TempDir,
    pub database: WorkspaceDb,
}

impl Fixture {
    pub fn uri(&self, relative: &str) -> url::Url {
        url::Url::from_file_path(self.directory.path().join(relative)).must_be("fixture symbol URI")
    }
}

pub(super) struct ColdSources {
    pub directory: tempfile::TempDir,
    pub documents: Vec<(url::Url, PathBuf, String)>,
}

impl ColdSources {
    pub fn new(files: usize, matches: usize) -> Self {
        let sources = Sources::new(files, matches);
        Self {
            directory: sources.directory,
            documents: sources
                .documents
                .into_iter()
                .map(|(document, path)| (document.uri, path, document.snapshot.text.clone()))
                .collect(),
        }
    }
}

pub(super) struct InitialSources {
    pub directory: tempfile::TempDir,
    pub root_uri: url::Url,
    pub root_path: PathBuf,
    pub root_text: String,
    pub database: WorkspaceDb,
}

impl InitialSources {
    pub fn new() -> Self {
        let sources = Sources::new(99, 98);
        fs::create_dir(sources.directory.path().join("nested"))
            .must_be("normalized import directory");
        let mut root_text = String::new();
        for (index, (document, path)) in sources.documents.iter().enumerate() {
            fs::write(path, &document.snapshot.text).must_be("initial workspace source");
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .must_be("fixture filename");
            writeln!(root_text, "import ./{filename} as Module{index}")
                .must_be("initial root import");
        }
        root_text.push_str("def main(value):\n  Module0.identity(value)\n");
        let root_path = sources.directory.path().join("root.bend");
        fs::write(&root_path, &root_text).must_be("initial root source");
        let root_uri = url::Url::from_file_path(&root_path).must_be("initial root URI");
        Self {
            directory: sources.directory,
            root_uri,
            root_path,
            root_text,
            database: WorkspaceDb::default(),
        }
    }
}

pub(super) static SPARSE_100: LazyLock<Fixture> = LazyLock::new(|| Sources::new(100, 3).build());
pub(super) static SPARSE_1000: LazyLock<Fixture> = LazyLock::new(|| Sources::new(1_000, 3).build());
pub(super) static SPARSE_10000: LazyLock<Fixture> =
    LazyLock::new(|| Sources::new(10_000, 3).build());
pub(super) static MATCHED_100: LazyLock<Fixture> = LazyLock::new(|| Sources::new(100, 99).build());
pub(super) static MATCHED_1000: LazyLock<Fixture> =
    LazyLock::new(|| Sources::new(1_000, 999).build());
pub(super) static MATCHED_10000: LazyLock<Fixture> =
    LazyLock::new(|| Sources::new(10_000, 9_999).build());

pub(super) fn cases() -> [(&'static str, &'static LazyLock<Fixture>); 6] {
    [
        ("sparse_100", &SPARSE_100),
        ("sparse_1000", &SPARSE_1000),
        ("sparse_10000", &SPARSE_10000),
        ("matched_100", &MATCHED_100),
        ("matched_1000", &MATCHED_1000),
        ("matched_10000", &MATCHED_10000),
    ]
}

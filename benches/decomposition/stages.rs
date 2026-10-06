use bend2_lsp::{
    analysis::Revision,
    workspace::{Document, WorkspaceDb},
};
use tower_lsp::lsp_types::{Location, WorkspaceEdit};

use super::{
    consumers::{self, CallGroup, ReferenceView},
    fixtures::{ColdSources, Fixture, InitialSources},
};

#[inline(never)]
pub(super) fn initial_build(mut sources: InitialSources) -> InitialSources {
    let root = sources.database.set_open_document(
        Document::new(
            sources.root_uri.clone(),
            "bend".into(),
            Revision(1),
            std::mem::take(&mut sources.root_text),
        ),
        Some(sources.root_path.clone()),
    );
    sources.database.load_reachable(&[root]);
    sources
}

#[inline(never)]
pub(super) fn cold_workspace_build(sources: ColdSources) -> Fixture {
    let mut database = WorkspaceDb::default();
    for (uri, path, text) in sources.documents {
        database.set_open_document(
            Document::new(uri, "bend".into(), Revision(1), text),
            Some(path),
        );
    }
    Fixture {
        directory: sources.directory,
        database,
    }
}

#[cfg(feature = "decomp-identity")]
#[inline(never)]
pub(super) fn cold_semantic_build(sources: super::fixtures::Sources) -> Fixture {
    sources.build()
}

pub(super) struct PreparedReferences {
    pub fixture: &'static Fixture,
    pub view: ReferenceView,
}

pub(super) fn prepare_references(fixture: &'static Fixture) -> PreparedReferences {
    PreparedReferences {
        fixture,
        view: consumers::reference_lookup(fixture),
    }
}

#[inline(never)]
pub(super) fn reference_lookup(fixture: &'static Fixture) -> ReferenceView {
    consumers::reference_lookup(fixture)
}

#[inline(never)]
pub(super) fn reference_materialize(input: &PreparedReferences) -> Vec<Location> {
    consumers::reference_materialize(input.fixture, &input.view)
}

#[inline(never)]
pub(super) fn incoming(fixture: &Fixture) -> Vec<CallGroup> {
    consumers::incoming(fixture)
}

#[inline(never)]
pub(super) fn outgoing(fixture: &Fixture) -> Vec<CallGroup> {
    consumers::outgoing(fixture)
}

#[inline(never)]
pub(super) fn rename(fixture: &Fixture) -> WorkspaceEdit {
    consumers::rename(fixture)
}

//! Test-only JSONL adapter over the same immutable semantic query API as LSP.
use std::{
    error::Error,
    io::{self, BufRead, Write},
    path::{Component, Path},
    sync::{Arc, LazyLock},
};

use bend2_lsp::{
    analysis::{DeclarationKind, DocumentSnapshot, ReferenceRole, Revision},
    workspace::{Document, SemanticBase, SemanticSite, SemanticTarget, SemanticView, WorkspaceDb},
};
use serde_json::{Value, json};
use url::Url;

// Production reuses the immutable Base snapshot as well. Each request still
// creates its own workspace and entry-specific semantic view.
static BASE_SNAPSHOT: LazyLock<Arc<DocumentSnapshot>> = LazyLock::new(|| {
    Arc::new(DocumentSnapshot::new(
        Revision(1),
        include_str!("../tests/fixtures/semantic/compiler/base.bend").to_owned(),
    ))
});
static BASE_PROVENANCE: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::from(include_str!("../tests/fixtures/semantic/compiler/pin.json")));

fn logical_file(document: &Document, root: &Path) -> Result<String, Box<dyn Error>> {
    Ok(if document.uri.scheme() == "bend-base" {
        "@Base.bend".to_owned()
    } else {
        document
            .uri
            .to_file_path()
            .map_err(|()| "semantic URI is not a file")?
            .strip_prefix(root)?
            .to_str()
            .ok_or("non-UTF8 fixture path")?
            .to_owned()
    })
}

fn source_site(
    view: &SemanticView,
    site: SemanticSite,
    root: &Path,
) -> Result<Value, Box<dyn Error>> {
    let document = view
        .document(site.file)
        .ok_or("semantic target has no document")?;
    Ok(json!({"file":logical_file(document,root)?,"start":site.range.start,"end":site.range.end}))
}

fn build_view(request: &Value, root: &Path) -> Result<(SemanticView, Value), Box<dyn Error>> {
    let started = std::time::Instant::now();
    let root_name = request["root"].as_str().ok_or("missing root")?;
    let files = request["files"].as_object().ok_or("missing files")?;
    if files.is_empty() || files.len() > 4 {
        return Err("invalid fixture file count".into());
    }
    let mut db = WorkspaceDb::default();
    let mut total = 0;
    for (name, source) in files {
        let relative = Path::new(name);
        if !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
            || relative
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("bend")
        {
            return Err("unsafe fixture path".into());
        }
        let source = source.as_str().ok_or("source is not text")?;
        total += source.len();
        if source.len() > 16_384 || total > 65_536 {
            return Err("fixture source limit".into());
        }
        let path = root.join(relative);
        let parent = path.parent().ok_or("missing fixture parent")?;
        std::fs::create_dir_all(parent)?;
        let disk_source = request
            .get("disk_files")
            .and_then(|disk| disk.get(name))
            .and_then(Value::as_str)
            .unwrap_or(source);
        if disk_source.len() > 16_384 {
            return Err("disk fixture source limit".into());
        }
        std::fs::write(&path, disk_source)?;
        let uri = Url::from_file_path(&path).map_err(|()| "invalid fixture URI")?;
        let revision = request["revisions"][name]
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .ok_or("missing or invalid revision")?;
        db.set_open_document(
            Document::new(uri, "bend".into(), Revision(revision), source.into()),
            Some(path),
        );
    }
    if !files.contains_key(root_name) {
        return Err("root is absent from fixture".into());
    }
    let root_uri = Url::from_file_path(root.join(root_name)).map_err(|()| "invalid root URI")?;
    let root_id = db.file_id_by_uri(&root_uri).ok_or("root is not indexed")?;
    db.load_reachable(&[root_id]);
    let snapshot_seconds = started.elapsed().as_secs_f64();
    let capture_started = std::time::Instant::now();
    let captured = db
        .capture_semantic_view(root_id, None)
        .ok_or("semantic view not ready")?;
    let capture_seconds = capture_started.elapsed().as_secs_f64();
    let base_started = std::time::Instant::now();
    let base = if captured.requires_base() {
        Some(SemanticBase {
            document: Document::with_snapshot(
                Url::parse("bend-base:///Base.bend")?,
                "bend".into(),
                Arc::clone(LazyLock::force(&BASE_SNAPSHOT)),
            ),
            generation: 1,
            provenance: Arc::clone(LazyLock::force(&BASE_PROVENANCE)),
            source_lease: None,
        })
    } else {
        None
    };
    let base_seconds = base_started.elapsed().as_secs_f64();
    let semantic_started = std::time::Instant::now();
    let input = if let Some(base) = base {
        db.capture_semantic_view(root_id, Some(base))
            .ok_or("semantic view not ready")?
    } else {
        captured
    };
    let view = input.build();
    Ok((
        view,
        json!({"fixture_io_and_snapshot_seconds":snapshot_seconds,"base_snapshot_seconds":base_seconds,"semantic_view_seconds":capture_seconds+semantic_started.elapsed().as_secs_f64()}),
    ))
}

fn declaration_records(view: &SemanticView, root: &Path) -> Result<Vec<Value>, Box<dyn Error>> {
    let mut declarations = Vec::new();
    for declaration in view.declarations() {
        let document = view
            .document(declaration.site.file)
            .ok_or("declaration document missing")?;
        if document.uri.scheme() == "bend-base" {
            continue;
        }
        let target = view
            .target_site(declaration.target)
            .map(|site| source_site(view, site, root))
            .transpose()?;
        let kind = declaration_kind(declaration.kind);
        let name = document
            .text
            .get(declaration.site.range.start..declaration.site.range.end)
            .ok_or("declaration outside source")?;
        let entity = match declaration.target {
            SemanticTarget::Global(id) => view.entity(id),
            _ => None,
        };
        let signature_owner = entity
            .map(|entity| source_site(view, entity.signature_owner, root))
            .transpose()?;
        let body_owner = entity
            .and_then(|entity| entity.body_owner)
            .map(|site| source_site(view, site, root))
            .transpose()?;
        declarations.push(json!({"site":source_site(view,declaration.site,root)?,"kind":kind,"target":target,"name":name,
            "signatureOwner":signature_owner,"bodyOwner":body_owner}));
    }
    declarations.sort_by_cached_key(Value::to_string);
    Ok(declarations)
}

fn occurrence_records(view: &SemanticView, root: &Path) -> Result<Vec<Value>, Box<dyn Error>> {
    let mut occurrences = Vec::new();
    for document in view.documents() {
        if document.uri.scheme() == "bend-base" {
            continue;
        }
        let file = view
            .file_id(&document.uri)
            .ok_or("view document has no identity")?;
        for occurrence in view.occurrences(file) {
            let target = view
                .target_site(occurrence.target)
                .map(|site| source_site(view, site, root))
                .transpose()?;
            let role = match occurrence.role {
                ReferenceRole::Value => {
                    if occurrence.in_type {
                        "type"
                    } else {
                        "value"
                    }
                }
                ReferenceRole::Type => "type",
                ReferenceRole::Constructor => "constructor",
                ReferenceRole::Call => {
                    if occurrence.in_type {
                        "type"
                    } else {
                        "call"
                    }
                }
                ReferenceRole::Offload => "offload",
                ReferenceRole::Binding => "binding",
            };
            let (binding, status) = match occurrence.target {
                SemanticTarget::Local(_) => ("local", "resolved"),
                SemanticTarget::Global(_) => ("global", "resolved"),
                SemanticTarget::Unresolved => ("global", "unresolved"),
                SemanticTarget::Ambiguous => ("global", "uncertain"),
            };
            let status = if occurrence.uncertain {
                "uncertain"
            } else {
                status
            };
            let site = SemanticSite {
                range: occurrence.full_range,
                ..occurrence.site
            };
            let enclosing = occurrence
                .enclosing
                .and_then(|id| view.entity(id))
                .map(|entity| source_site(view, entity.declaration, root))
                .transpose()?;
            occurrences.push(json!({"site":source_site(view,site,root)?,"role":role,"target":target,"binding":binding,"status":status,
                "enclosing":enclosing}));
        }
    }
    occurrences.sort_by_cached_key(Value::to_string);
    Ok(occurrences)
}

const fn declaration_kind(kind: DeclarationKind) -> &'static str {
    match kind {
        DeclarationKind::Def => "def",
        DeclarationKind::Law => "law",
        DeclarationKind::Type => "type",
        DeclarationKind::Constructor => "constructor",
    }
}

fn base_records(
    request: &Value,
    view: &SemanticView,
    root: &Path,
) -> Result<Vec<Value>, Box<dyn Error>> {
    let mut records = Vec::new();
    if let Some(names) = request.get("inspect_base") {
        let names = names.as_array().ok_or("inspect_base must be an array")?;
        if names.len() > 16 {
            return Err("too many inspected Base names".into());
        }
        for name in names {
            let name = name.as_str().ok_or("Base name must be text")?;
            if name.is_empty() || name.len() > 256 {
                return Err("invalid inspected Base name".into());
            }
            for entity in view
                .entities()
                .iter()
                .filter(|entity| entity.base && entity.name == name)
            {
                let body_owner = entity
                    .body_owner
                    .map(|site| source_site(view, site, root))
                    .transpose()?;
                records.push(json!({
                    "site": source_site(view, entity.declaration, root)?,
                    "name": entity.name,
                    "kind": declaration_kind(entity.kind),
                    "target": source_site(view, entity.declaration, root)?,
                    "signatureOwner": source_site(view, entity.signature_owner, root)?,
                    "bodyOwner": body_owner,
                }));
            }
        }
    }
    records.sort_by_cached_key(Value::to_string);
    Ok(records)
}

fn query(request: &Value) -> Result<Value, Box<dyn Error>> {
    if request["schema"] != 1 {
        return Err("unsupported request schema".into());
    }
    let id = request["id"].as_str().ok_or("missing request id")?;
    let root_name = request["root"].as_str().ok_or("missing root")?;
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (view, mut metrics) = build_view(request, root)?;
    let started = std::time::Instant::now();
    let declarations = declaration_records(&view, root)?;
    let occurrences = occurrence_records(&view, root)?;
    let base_entities = base_records(request, &view, root)?;
    let mut revisions = serde_json::Map::new();
    for document in view.documents() {
        if document.uri.scheme() != "bend-base" {
            revisions.insert(logical_file(document, root)?, json!(document.revision.0));
        }
    }
    metrics["normalization_seconds"] = json!(started.elapsed().as_secs_f64());
    Ok(
        json!({"schema":1,"id":id,"root":root_name,"outcome":"valid","declarations":declarations,"occurrences":occurrences,"base_entities":base_entities,"revisions":revisions,"metrics":metrics}),
    )
}

fn main() -> Result<(), Box<dyn Error>> {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    for line in stdin.lock().lines() {
        let line = line?;
        if line.len() > 300_000 {
            return Err("request exceeds input bound".into());
        }
        let request: Value = serde_json::from_str(&line)?;
        let reply = match query(&request) {
            Ok(value) => value,
            Err(error) => {
                json!({"schema":1,"id":request["id"],"root":request["root"],"outcome":"oracle-error","error":error.to_string()})
            }
        };
        serde_json::to_writer(&mut stdout, &reply)?;
        writeln!(stdout)?;
        stdout.flush()?;
    }
    Ok(())
}

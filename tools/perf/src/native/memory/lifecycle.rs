use super::super::{identity, root};
use crate::{
    ToolResult, common, latency,
    transport::{LspProcess, file_uri},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, ffi::OsString, fs, path::Path, time::Duration};

// DHAT resolves all allocation backtraces after the runtime exits and before
// creating its profile. Match the existing native example process bound without
// extending JSON-RPC request deadlines or default latency shutdown deadlines.
const PROFILE_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) fn collect(binary: &Path, profile: &Path, output: &Path) -> ToolResult<u32> {
    let large = fs::read_to_string(root().join("benches/fixtures/analyzer_large.bend"))?;
    let documents = [
        ("small", latency::SMALL_SOURCE.to_owned()),
        ("dep", latency::DEPENDENCY_SOURCE.to_owned()),
        ("importer", latency::IMPORTER_SOURCE.to_owned()),
        (
            "large",
            format!(
                "def memory_fixture: U32\n  1\n{}{large}",
                latency::ADT_SOURCE
            ),
        ),
    ];
    let temporary = tempfile::Builder::new()
        .prefix("bend-native-dhat-")
        .tempdir()?;
    let workspace = temporary.path().canonicalize()?;
    let mut uris = BTreeMap::new();
    let mut source_inputs = BTreeMap::new();
    for (name, source) in &documents {
        let filename = format!("{name}.bend");
        let path = workspace.join(&filename);
        fs::write(&path, source)?;
        let artifact = output.join(&filename);
        fs::write(&artifact, source)?;
        uris.insert(*name, file_uri(&path)?);
        source_inputs.insert(*name, identity(&artifact)?);
    }
    let environment = [(
        OsString::from("BEND2_LSP_DHAT_FILE"),
        profile.as_os_str().to_owned(),
    )];
    let mut client = LspProcess::spawn(binary, &workspace, &environment)?;
    let pid = client.pid();
    let mut semantics = json!({"pid": pid, "cwd": workspace, "uris": uris, "settings": client.settings(),
        "initialized": false, "graceful_shutdown": false, "opened_revisions": {}, "responses": {}, "source_inputs": source_inputs});
    common::write_json(&output.join("semantics.json"), &semantics)?;
    let outcome = protocol(&mut client, &documents, &uris, output, &mut semantics);
    semantics["shutdown"] = serde_json::to_value(client.shutdown_evidence())?;
    if let Err(error) = &outcome {
        semantics["error"] = json!(error.to_string());
    }
    common::write_json(&output.join("semantics.json"), &semantics)?;
    fs::write(
        output.join("stderr.txt"),
        client
            .stderr_tail()
            .unwrap_or_else(|error| format!("stderr unavailable: {error}")),
    )?;
    outcome?;
    Ok(pid)
}

fn uri<'a>(uris: &'a BTreeMap<&str, String>, name: &str) -> ToolResult<&'a str> {
    uris.get(name)
        .map(String::as_str)
        .ok_or_else(|| format!("Missing profile document URI: {name}").into())
}

fn response(
    client: &mut LspProcess,
    semantics: &mut Value,
    name: &str,
    method: &str,
    params: Value,
    notification: Option<Value>,
) -> ToolResult<Value> {
    let (value, _) = client.request(method, params, notification)?;
    semantics["responses"][name] = value.clone();
    Ok(value)
}

fn protocol(
    client: &mut LspProcess,
    documents: &[(&str, String)],
    uris: &BTreeMap<&str, String>,
    output: &Path,
    semantics: &mut Value,
) -> ToolResult<()> {
    client.initialize()?;
    semantics["initialized"] = json!(true);
    for (name, source) in documents {
        let uri = uri(uris, name)?;
        client.notify(
            "textDocument/didOpen",
            latency::open_message(uri, source, 1)["params"].clone(),
        )?;
        client.wait_diagnostics(uri, Some(1))?;
        semantics["opened_revisions"][name] = client
            .diagnostics(uri)
            .ok_or("Missing observed open diagnostics")?
            .clone();
        common::write_json(&output.join("semantics.json"), semantics)?;
    }
    let value = response(
        client,
        semantics,
        "small_hover",
        "textDocument/hover",
        latency::position(uri(uris, "small")?, 3, 4),
        None,
    )?;
    latency::require_hover(&value, latency::SMALL_SIGNATURE, None)?;
    let value = response(
        client,
        semantics,
        "large_hover",
        "textDocument/hover",
        latency::position(uri(uris, "large")?, 0, 5),
        None,
    )?;
    latency::require_hover(&value, "def memory_fixture: U32", None)?;
    let importer_position = latency::position(uri(uris, "importer")?, 2, 8);
    let value = response(
        client,
        semantics,
        "importer_definition",
        "textDocument/definition",
        importer_position.clone(),
        None,
    )?;
    latency::require_definition(&value, uri(uris, "dep")?)?;
    let value = response(
        client,
        semantics,
        "dependency_hover_before_edit",
        "textDocument/hover",
        importer_position.clone(),
        None,
    )?;
    latency::require_hover(&value, "def clamp(x: U32) -> U32", None)?;
    let changed = latency::DEPENDENCY_SOURCE.replace("U32", "U64");
    let revised_source = output.join("dep-revision-2.bend");
    fs::write(&revised_source, &changed)?;
    semantics["source_inputs"]["dep-revision-2"] = identity(&revised_source)?;
    let value = response(
        client,
        semantics,
        "dependency_hover_after_edit",
        "textDocument/hover",
        importer_position,
        Some(latency::change_message(uri(uris, "dep")?, &changed, 2)),
    )?;
    latency::require_hover(
        &value,
        "def clamp(x: U64) -> U64",
        Some("def clamp(x: U32) -> U32"),
    )?;
    client.wait_diagnostics(uri(uris, "dep")?, Some(2))?;
    semantics["dependency_revision_2_diagnostics"] = client
        .diagnostics(uri(uris, "dep")?)
        .ok_or("Missing observed dependency revision diagnostics")?
        .clone();
    client.finish_profiled(PROFILE_FINALIZATION_TIMEOUT)?;
    semantics["graceful_shutdown"] = json!(true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_lifecycle_requires_new_signature_and_exact_definition() -> ToolResult<()> {
        let before = json!({"contents": {"value": "def clamp(x: U32) -> U32"}});
        let after = json!({"contents": {"value": "def clamp(x: U64) -> U64"}});
        latency::require_hover(&before, "def clamp(x: U32) -> U32", None)?;
        latency::require_hover(
            &after,
            "def clamp(x: U64) -> U64",
            Some("def clamp(x: U32) -> U32"),
        )?;
        assert!(
            latency::require_hover(
                &before,
                "def clamp(x: U64) -> U64",
                Some("def clamp(x: U32) -> U32")
            )
            .is_err()
        );
        let stale =
            json!({"contents": {"value": "def clamp(x: U64) -> U64\ndef clamp(x: U32) -> U32"}});
        assert!(
            latency::require_hover(
                &stale,
                "def clamp(x: U64) -> U64",
                Some("def clamp(x: U32) -> U32")
            )
            .is_err()
        );
        let location = json!({"uri": "file:///dep.bend", "range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 9}}});
        latency::require_definition(&location, "file:///dep.bend")?;
        assert!(latency::require_definition(&location, "file:///importer.bend").is_err());
        Ok(())
    }
}

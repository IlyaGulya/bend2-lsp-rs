use serde_json::Value;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticRelatedInformation, NumberOrString};

// Capacities where exposed; URL strings and JSON object entries are lower
// bounds because those types do not expose their allocation capacity. These
// are backing-buffer estimates, never allocator-live or resident byte counts.
pub(in crate::server) fn diagnostic_backing_bytes(diagnostic: &Diagnostic) -> usize {
    diagnostic.message.capacity()
        .saturating_add(diagnostic.source.as_ref().map_or(0, String::capacity))
        .saturating_add(match &diagnostic.code {
            Some(NumberOrString::String(code)) => code.capacity(),
            Some(NumberOrString::Number(_)) | None => 0,
        })
        .saturating_add(diagnostic.code_description.as_ref().map_or(0, |description| description.href.as_str().len()))
        .saturating_add(diagnostic.tags.as_ref().map_or(0, |tags| tags.capacity() * size_of::<tower_lsp::lsp_types::DiagnosticTag>()))
        .saturating_add(diagnostic.related_information.as_ref().map_or(0, |related| {
            related.iter().fold(related.capacity() * size_of::<DiagnosticRelatedInformation>(), |bytes, information| {
                bytes.saturating_add(information.message.capacity())
                    .saturating_add(information.location.uri.as_str().len())
            })
        }))
        .saturating_add(diagnostic.data.as_ref().map_or(0, json_backing_bytes))
}

fn json_backing_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.capacity(),
        Value::Array(values) => values.iter().fold(values.capacity() * size_of::<Value>(), |bytes, value| {
            bytes.saturating_add(json_backing_bytes(value))
        }),
        Value::Object(values) => values.iter().fold(values.len() * size_of::<(String, Value)>(), |bytes, (key, value)| {
            bytes.saturating_add(key.capacity()).saturating_add(json_backing_bytes(value))
        }),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

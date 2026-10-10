use super::{identity, read_json};
use crate::ToolResult;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

const TOTALS: [&str; 6] = ["tb", "tbk", "gb", "gbk", "eb", "ebk"];

fn integer(data: &Value, key: &str) -> ToolResult<u64> {
    data[key]
        .as_u64()
        .ok_or_else(|| format!("Invalid nonnegative integer DHAT field: {key}").into())
}

fn validate_header(data: &Value, pid: u32, command: &[String]) -> ToolResult<()> {
    if !data.is_object()
        || data["dhatFileVersion"].as_u64() != Some(2)
        || data["mode"] != "rust-heap"
    {
        return Err("Not a DHAT v2 Rust heap profile".into());
    }
    if data["pid"].as_u64() != Some(u64::from(pid)) || data["cmd"] != command.join(" ") {
        return Err("DHAT process provenance differs".into());
    }
    if data["verb"] != "Allocated" || data["bklt"] != true || data["bkacc"] != false {
        return Err("Invalid DHAT heap profile header".into());
    }
    if data["tu"] != "µs" || data["Mtu"] != "s" {
        return Err("Unexpected DHAT time units".into());
    }
    if integer(data, "tg")? > integer(data, "te")? {
        return Err("Invalid DHAT profile lifetime".into());
    }
    integer(data, "tuth")?;
    Ok(())
}

fn allocation_totals(data: &Value, frame_count: usize) -> ToolResult<BTreeMap<&'static str, u64>> {
    let points = data["pps"]
        .as_array()
        .ok_or("Missing DHAT allocation points")?;
    if points.is_empty() {
        return Err("Missing DHAT allocation points".into());
    }
    let mut totals = BTreeMap::from(TOTALS.map(|key| (key, 0_u64)));
    for point in points {
        if !point.is_object() {
            return Err("Invalid DHAT allocation point".into());
        }
        for key in TOTALS.into_iter().chain(["mb", "mbk", "tl"]) {
            let value = integer(point, key)?;
            if let Some(total) = totals.get_mut(key) {
                *total = total
                    .checked_add(value)
                    .ok_or("DHAT allocation total overflow")?;
            }
        }
        for (total, peak, observed) in [
            ("tb", "mb", "gb"),
            ("tb", "mb", "eb"),
            ("tbk", "mbk", "gbk"),
            ("tbk", "mbk", "ebk"),
        ] {
            if integer(point, total)? < integer(point, peak)?
                || integer(point, peak)? < integer(point, observed)?
            {
                return Err("Inconsistent DHAT allocation sizes".into());
            }
        }
        let indices = point["fs"].as_array().ok_or("Invalid DHAT backtrace")?;
        if indices.is_empty()
            || indices.iter().any(|index| {
                index
                    .as_u64()
                    .and_then(|number| usize::try_from(number).ok())
                    .is_none_or(|index| index >= frame_count)
            })
        {
            return Err("Invalid DHAT backtrace".into());
        }
    }
    if totals["tb"] == 0 || totals["tbk"] == 0 || totals["gb"] == 0 {
        return Err("Profile has no meaningful heap allocations".into());
    }
    Ok(totals)
}

fn validate(data: &Value, pid: u32, command: &[String]) -> ToolResult<Value> {
    validate_header(data, pid, command)?;
    let frames = data["ftbl"]
        .as_array()
        .ok_or("Malformed DHAT frame table")?;
    if frames.is_empty() || frames.iter().any(|frame| !frame.is_string()) {
        return Err("Malformed DHAT frame table".into());
    }
    let application = Path::new(command.first().ok_or("Missing DHAT command")?)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or("Invalid DHAT application filename")?
        .replace('-', "_");
    if !matches!(
        application.as_str(),
        "bend2_lsp" | "line_index_profile" | "folding_allocations"
    ) || !frames.iter().any(|frame| {
        frame
            .as_str()
            .is_some_and(|frame| frame.contains(&application))
    }) {
        return Err("Profile has no symbolized application frames".into());
    }
    let totals = allocation_totals(data, frames.len())?;
    Ok(json!({
        "total_allocated_bytes": totals["tb"], "total_allocated_blocks": totals["tbk"],
        "global_peak_live_bytes": totals["gb"], "global_peak_live_blocks": totals["gbk"],
        "end_live_bytes": totals["eb"], "end_live_blocks": totals["ebk"],
        "allocation_points": data["pps"].as_array().ok_or("Missing allocation points")?.len(),
    }))
}

pub(super) fn validate_profile(path: &Path, pid: u32, command: &[String]) -> ToolResult<Value> {
    let mut summary = validate(&read_json(path)?, pid, command)?;
    summary["profile"] = identity(path)?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        json!({"dhatFileVersion": 2, "mode": "rust-heap", "pid": 42, "cmd": "bend2-lsp", "verb": "Allocated", "bklt": true, "bkacc": false, "tu": "µs", "Mtu": "s", "te": 10, "tg": 5, "tuth": 0,
            "ftbl": ["[root]", "bend2_lsp::server::initialize (src/server.rs:123)"],
            "pps": [{"tb": 100, "tbk": 10, "mb": 90, "mbk": 9, "gb": 80, "gbk": 8, "eb": 20, "ebk": 2, "tl": 5, "fs": [1]}]})
    }

    #[test]
    fn valid_raw_heap_profile_preserves_distinct_total_peak_and_end() -> ToolResult<()> {
        let summary = validate(&fixture(), 42, &["bend2-lsp".to_owned()])?;
        assert_eq!(summary["total_allocated_bytes"], 100);
        assert_eq!(summary["global_peak_live_bytes"], 80);
        assert_eq!(summary["end_live_bytes"], 20);
        assert_eq!(summary["allocation_points"], 1);
        Ok(())
    }

    #[test]
    fn rejects_wrong_process_schema_symbols_and_allocation_invariants() {
        for (pointer, value) in [
            ("/dhatFileVersion", json!(1)),
            ("/mode", json!("heap")),
            ("/pid", json!(43)),
            ("/cmd", json!("line_index_profile")),
            ("/bklt", json!(false)),
            ("/bkacc", json!(true)),
            ("/verb", json!("Freed")),
            ("/tu", json!("ns")),
            ("/te", json!(4)),
            ("/tuth", json!(-1)),
            ("/ftbl", json!([])),
            ("/ftbl/1", json!("0x123456")),
            ("/pps", json!([])),
            ("/pps/0/tb", json!(true)),
            ("/pps/0/tbk", json!(1.5)),
            ("/pps/0/mb", json!(101)),
            ("/pps/0/eb", json!(91)),
            ("/pps/0/fs", json!([2])),
            ("/pps/0/fs", json!([])),
            ("/pps/0/tl", json!(-1)),
        ] {
            let mut data = fixture();
            if let Some(field) = data.pointer_mut(pointer) {
                *field = value;
            } else {
                panic!("Missing fixture field {pointer}");
            }
            assert!(
                validate(&data, 42, &["bend2-lsp".to_owned()]).is_err(),
                "Accepted malformed field {pointer}"
            );
        }
    }

    #[test]
    fn raw_profile_identity_is_hashed_and_strict_json_is_required() -> ToolResult<()> {
        let temp = tempfile::tempdir()?;
        let profile = temp.path().join("dhat-heap.json");
        crate::common::write_json(&profile, &fixture())?;
        let summary = validate_profile(&profile, 42, &["bend2-lsp".to_owned()])?;
        assert_eq!(summary["profile"], identity(&profile)?);
        std::fs::write(&profile, "{\"te\":NaN}")?;
        assert!(validate_profile(&profile, 42, &["bend2-lsp".to_owned()]).is_err());
        Ok(())
    }
}

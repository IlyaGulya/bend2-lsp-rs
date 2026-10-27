//! Deterministic Callgrind control sharding across physical runners.
//!
//! One logical job keeps the original fresh-process dispatch, pair order,
//! counters and thresholds. Its frozen executed-workload manifest is partitioned
//! into whole original function units: within each executable, the distinct
//! sorted `function_name`s are enumerated and unit `n` belongs to physical shard
//! `n % count`. All case IDs of a selected function travel with their function,
//! so descriptor case vectors are never filtered or reindexed and every workload
//! series still comes from exactly one physical runner.
use super::{BENCHMARKS, Identity, require};
use crate::{ToolResult, callgrind::sha256_bytes};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const METHOD: &str = "sorted-function-unit-index-modulo-v1";
pub(super) const MAX_COUNT: u64 = 4;

pub(super) type Selection = BTreeMap<&'static str, BTreeSet<Identity>>;

fn groups(identities: &BTreeSet<Identity>) -> BTreeMap<&str, BTreeSet<Identity>> {
    let mut units: BTreeMap<&str, BTreeSet<Identity>> = BTreeMap::new();
    for identity in identities {
        units
            .entry(identity.0.as_str())
            .or_default()
            .insert(identity.clone());
    }
    units
}

/// Select one shard from a complete manifest without reinterpreting its scope.
pub(super) fn selection(full: &Selection, count: u64, index: u64) -> ToolResult<Selection> {
    require(
        (1..=MAX_COUNT).contains(&count),
        "shard count must be between 1 and 4",
    )?;
    require(index < count, "shard index must be less than shard count")?;
    require(
        BENCHMARKS
            .iter()
            .all(|benchmark| full.contains_key(*benchmark)),
        "frozen manifest must contain every benchmark",
    )?;
    Ok(full
        .iter()
        .map(|(benchmark, identities)| {
            let selected = groups(identities)
                .into_iter()
                .enumerate()
                .filter(|(position, _)| u64::try_from(*position).is_ok_and(|p| p % count == index))
                .flat_map(|(_, (_, cases))| cases)
                .collect();
            (*benchmark, selected)
        })
        .collect())
}

fn identity_manifest(full: &Selection) -> Value {
    let mut benchmarks = Map::new();
    for benchmark in BENCHMARKS {
        let rows: Vec<Value> = full[benchmark]
            .iter()
            .map(|(name, id)| json!([name, id]))
            .collect();
        benchmarks.insert((*benchmark).to_owned(), Value::Array(rows));
    }
    Value::Object(benchmarks)
}

/// Declare the complete frozen manifest a shard was selected from. The digest is
/// canonical JSON over structured identity arrays, so embedded newlines, empty
/// strings, and `null` versus `""` cannot collide.
pub(super) fn manifest(full: &Selection) -> ToolResult<Value> {
    let identities = identity_manifest(full);
    let digest = sha256_bytes(&serde_json::to_vec(&identities)?);
    let benchmarks: Map<_, _> = BENCHMARKS
        .iter()
        .map(|benchmark| ((*benchmark).to_owned(), json!(full[*benchmark].len())))
        .collect();
    Ok(json!({
        "method": METHOD,
        "benchmarks": benchmarks,
        "workloads": full.values().map(BTreeSet::len).sum::<usize>(),
        "sha256": digest,
        "identities": identities,
    }))
}

/// Declare the selected whole function units and every original case ID.
pub(super) fn units(selected: &Selection) -> Value {
    let mut benchmarks = Map::new();
    for benchmark in BENCHMARKS {
        let rows: Vec<Value> = groups(&selected[benchmark])
            .into_iter()
            .map(|(name, cases)| {
                json!({
                    "function_name": name,
                    "ids": cases.iter().map(|(_, id)| json!(id)).collect::<Vec<_>>(),
                })
            })
            .collect();
        benchmarks.insert((*benchmark).to_owned(), Value::Array(rows));
    }
    Value::Object(benchmarks)
}

pub(super) fn count(selection: &Selection) -> usize {
    selection.values().map(BTreeSet::len).sum()
}

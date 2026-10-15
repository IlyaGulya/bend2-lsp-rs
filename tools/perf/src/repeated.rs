//! Qualified fixed-sample policy shared by controls and source comparisons.
use crate::{
    ToolResult,
    callgrind::{self, Identity},
    policy,
};
use serde_json::{Value, json};
use std::{cmp::Ordering, collections::BTreeMap};

fn text<'a>(value: &'a Value, key: &str) -> ToolResult<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("missing {key}").into())
}

pub(crate) const PAIRS: usize = 105;
pub(crate) const METHOD: &str = "paired-median-105-v1";

#[derive(Clone, Copy)]
struct Residual {
    numerator: i128,
    denominator: u64,
}

impl Residual {
    fn value(self) -> Value {
        json!({"numerator": self.numerator, "denominator": self.denominator})
    }

    fn compare(self, other: Self) -> Ordering {
        match self.numerator.signum().cmp(&other.numerator.signum()) {
            Ordering::Equal => {
                let order = unsigned_ratio_cmp(
                    self.numerator.unsigned_abs(),
                    u128::from(self.denominator),
                    other.numerator.unsigned_abs(),
                    u128::from(other.denominator),
                );
                if self.numerator < 0 {
                    order.reverse()
                } else {
                    order
                }
            }
            order => order,
        }
    }
}

// Continued-fraction comparison avoids cross-products: a residual magnitude can
// exceed u64::MAX, so even u128 cross multiplication is not generally safe.
fn unsigned_ratio_cmp(
    mut left_numerator: u128,
    mut left_denominator: u128,
    mut right_numerator: u128,
    mut right_denominator: u128,
) -> Ordering {
    let mut reversed = false;
    loop {
        let order = (left_numerator / left_denominator).cmp(&(right_numerator / right_denominator));
        if order != Ordering::Equal {
            return if reversed { order.reverse() } else { order };
        }
        let left_remainder = left_numerator % left_denominator;
        let right_remainder = right_numerator % right_denominator;
        if left_remainder == 0 || right_remainder == 0 {
            let order = left_remainder.cmp(&right_remainder);
            return if reversed { order.reverse() } else { order };
        }
        (left_numerator, left_denominator) = (left_denominator, left_remainder);
        (right_numerator, right_denominator) = (right_denominator, right_remainder);
        reversed = !reversed;
    }
}

// For B~Binomial(105, 1/2), choose the greatest k with P(B<k)<=1/20.
// The one-based endpoints are k and n+1-k. All coefficients, intermediate
// products, and sums at this predeclared n fit u128. No normal approximation.
fn bounds() -> (usize, usize, u128, u128) {
    let total = 1_u128 << PAIRS;
    let (mut coefficient, mut cumulative, mut rank, mut tail) = (1_u128, 0_u128, 0, 0);
    for index in 0..=PAIRS / 2 {
        cumulative += coefficient;
        if cumulative * 20 > total {
            break;
        }
        rank = index + 1;
        tail = cumulative;
        coefficient = coefficient * (PAIRS - index) as u128 / (index + 1) as u128;
    }
    (rank, PAIRS + 1 - rank, total - tail, total)
}

type GroupKey = (String, String, Identity, String);

pub(crate) fn comparisons(raw: &[Value]) -> ToolResult<Vec<Value>> {
    let mut groups: BTreeMap<GroupKey, Vec<&Value>> = BTreeMap::new();
    for row in raw {
        groups
            .entry((
                text(row, "job_id")?.to_owned(),
                text(row, "variant")?.to_owned(),
                callgrind::identity(row)?,
                text(row, "metric")?.to_owned(),
            ))
            .or_default()
            .push(row);
    }
    let (lower_rank, upper_rank, confidence, total) = bounds();
    let mut output = Vec::with_capacity(groups.len());
    for ((job, variant, identity, metric), rows) in groups {
        let mut residuals = Vec::with_capacity(rows.len());
        for row in &rows {
            let baseline = row["baseline"].as_u64().ok_or("baseline missing")?;
            let candidate = row["candidate"].as_u64().ok_or("candidate missing")?;
            residuals.push(Residual {
                numerator: i128::from(candidate)
                    - i128::from(baseline)
                    - i128::from(policy::active_allowance(&metric, baseline)?),
                denominator: baseline.max(1),
            });
        }
        residuals.sort_unstable_by(|a, b| a.compare(*b));
        let complete = residuals.len() == PAIRS;
        let (median, lower, upper, classification) = if complete {
            let lower = residuals[lower_rank - 1];
            let upper = residuals[upper_rank - 1];
            (
                residuals[PAIRS / 2].value(),
                lower.value(),
                upper.value(),
                if upper.numerator <= 0 {
                    "passed"
                } else if lower.numerator > 0 {
                    "regression"
                } else {
                    "inconclusive"
                },
            )
        } else {
            (
                Value::Null,
                Value::Null,
                Value::Null,
                "insufficient_evidence",
            )
        };
        output.push(json!({"job_id": job, "role": rows[0]["role"], "variant": variant, "function_name": identity.0, "id": identity.1, "metric": metric, "samples": rows.len(), "classification": classification, "median": median, "lower": lower, "upper": upper, "lower_rank": lower_rank, "upper_rank": upper_rank, "one_sided_confidence": {"numerator": confidence, "denominator": total}, "raw_exceedances": rows.iter().filter(|row| row["current_passed"] == false).count()}));
    }
    Ok(output)
}

use super::data::{Artifact, Document, Report, TargetEvidence};
use crate::ToolResult;
use serde_json::Value;
use std::{collections::BTreeSet, fmt::Write as _};

pub(super) fn escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }
    output
}

fn href(path: &str) -> String {
    let mut output = String::from("./");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            // Writing into a String cannot fail.
            let _ = write!(output, "%{byte:02X}");
        }
    }
    output
}

fn status(value: &str) -> String {
    let class = match value {
        "complete" => "complete",
        "failed" => "failed",
        "regression" => "regression",
        "changed-scope" => "changed-scope",
        _ => "incomplete",
    };
    format!("<span class=\"status {class}\">{}</span>", escape(value))
}

fn text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn number(value: &Value, divisor: f64) -> String {
    value.as_f64().map_or_else(
        || "unavailable".into(),
        |value| format!("{:.3}", value / divisor),
    )
}

fn table_start(output: &mut String, caption: &str, headings: &[&str]) -> ToolResult<()> {
    write!(
        output,
        "<div class=\"table-wrap\" tabindex=\"0\" role=\"region\" aria-label=\"{}\"><table><caption>{}</caption><thead><tr>",
        escape(caption),
        escape(caption)
    )?;
    for heading in headings {
        write!(output, "<th scope=\"col\">{}</th>", escape(heading))?;
    }
    output.push_str("</tr></thead><tbody>");
    Ok(())
}
fn table_end(output: &mut String) {
    output.push_str("</tbody></table></div>");
}

fn json_details(output: &mut String, title: &str, value: &Value) -> ToolResult<()> {
    write!(
        output,
        "<details><summary>{}</summary><pre>{}</pre></details>",
        escape(title),
        escape(&serde_json::to_string_pretty(value)?)
    )?;
    Ok(())
}

fn issues(output: &mut String, title: &str, values: &[String]) -> ToolResult<()> {
    if values.is_empty() {
        return Ok(());
    }
    write!(
        output,
        "<div class=\"issues\"><h3>{}</h3><ul>",
        escape(title)
    )?;
    for value in values {
        write!(output, "<li>{}</li>", escape(value))?;
    }
    output.push_str("</ul></div>");
    Ok(())
}

pub(super) fn html(report: &Report) -> ToolResult<String> {
    let mut output = String::from(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'\"><title>Bend 2 performance evidence</title><style>",
    );
    output.push_str(include_str!("style.css"));
    output.push_str("</style></head><body><a class=\"skip\" href=\"#evidence\">Skip to evidence</a><header><h1>Performance evidence</h1><p>Bend 2 LSP · Native-target collection, semantic coverage and profile artifacts.</p>");
    write!(
        output,
        "<div class=\"verdict\"><strong>Run status: {}</strong><span>Missing evidence is not a passing result. Changed scope is not a regression.</span></div>",
        status(&report.status)
    )?;
    write!(
        output,
        "<p>Coverage scope: {}.</p>",
        escape(report.coverage_scope)
    )?;
    write!(
        output,
        "<p>{}</p><nav aria-label=\"Report navigation\"><a href=\"./unified-report.json\">Unified JSON</a><a href=\"./summary.md\">Markdown summary</a>",
        escape(report.policy)
    )?;
    for (index, target) in report.targets.iter().enumerate() {
        write!(
            output,
            "<a href=\"#target-{index}\">{}</a>",
            escape(&target.target)
        )?;
    }
    output.push_str("</nav></header><main id=\"evidence\">");
    issues(
        &mut output,
        "Run validation / hosted execution failed",
        &report.errors,
    )?;
    issues(
        &mut output,
        "Run expected coverage incomplete or absent",
        &report.issues,
    )?;
    if !report.run.is_null() {
        json_details(
            &mut output,
            "Exact run provenance and expected targets",
            &report.run,
        )?;
    }
    table_start(
        &mut output,
        "Target coverage — no cross-platform numerical comparisons",
        &[
            "Native target",
            "Evidence status",
            "Collectors",
            "Failures / missing",
        ],
    )?;
    for (index, target) in report.targets.iter().enumerate() {
        write!(
            output,
            "<tr><td><a href=\"#target-{index}\"><code>{}</code></a></td><td>{}</td><td>{}</td><td>{} / {}</td></tr>",
            escape(&target.target),
            status(&target.status),
            target
                .documents
                .iter()
                .filter(|document| document.kind != "discovery-raw")
                .count(),
            target.errors.len(),
            target.missing.len()
        )?;
    }
    table_end(&mut output);
    if report.targets.is_empty() {
        output.push_str("<p>No hosted artifact bundle was found. Download the selected workflow run’s artifacts, preserving their directory layout, then generate this report again.</p>");
    }
    for (index, target) in report.targets.iter().enumerate() {
        render_target(&mut output, target, index)?;
    }
    output.push_str("</main><footer><p>Self-contained summary: no network requests, scripts, remote fonts or external assets. Original traces remain unchanged. Viewer commands are instructions, not executable links. Report generation and opening do not measure performance.</p></footer></body></html>\n");
    Ok(output)
}

fn render_target(output: &mut String, target: &TargetEvidence, index: usize) -> ToolResult<()> {
    write!(
        output,
        "<section class=\"target\" id=\"target-{index}\"><h2>{}</h2><p>Evidence status: {}</p>",
        escape(&target.target),
        status(&target.status)
    )?;
    issues(output, "Collection / validation failed", &target.errors)?;
    issues(
        output,
        "Expected evidence incomplete or absent",
        &target.missing,
    )?;
    issues(
        output,
        "Changed semantic scope — do not compare as equal work",
        &target.changed_scope,
    )?;
    issues(output, "Performance gate regression", &target.regressions)?;
    table_start(
        output,
        "Correctness, collector coverage and gate evidence",
        &["Collector", "Status", "Original report"],
    )?;
    for document in target
        .documents
        .iter()
        .filter(|document| document.kind != "discovery-raw")
    {
        write!(
            output,
            "<tr><td>{}</td><td>{}</td><td><a href=\"{}\">{}</a></td></tr>",
            escape(&document.kind),
            status(&document.status),
            href(&document.path),
            escape(&document.path)
        )?;
    }
    table_end(output);
    callgrind_status(output, target)?;
    for document in &target.documents {
        match document.kind.as_str() {
            "latency" => latency(output, document)?,
            "discovery" => discovery(output, document)?,
            "memory" => heap(output, document)?,
            "profile" => profile(output, document)?,
            "callgrind" => {
                json_details(output, "Original Callgrind gate evidence", &document.data)?;
            }
            _ => {}
        }
    }
    output.push_str("<div class=\"document\"><h3>Artifacts and binary / source provenance</h3>");
    artifact_links(output, &target.artifacts)?;
    json_details(
        output,
        "Verified artifact manifest, source revisions, file identities and build/collector status",
        &target.provenance,
    )?;
    output.push_str("</div></section>");
    Ok(())
}

fn latency(output: &mut String, document: &Document) -> ToolResult<()> {
    output.push_str("<section class=\"document\"><h3>End-to-end request latency</h3><p class=\"note\">Report-only; no numerical latency gate. Times are milliseconds. Aggregates are medians of nearest-rank per-round percentiles, not pooled requests. Deltas are candidate minus baseline.</p>");
    table_start(
        output,
        "Paired latency comparisons",
        &[
            "Workload",
            "Base p50 / p95 ms",
            "Candidate p50 / p95 ms",
            "p50 / p95 change %",
        ],
    )?;
    if let Some(workloads) = document.data["workloads"].as_object() {
        for (name, value) in workloads {
            write!(
                output,
                "<tr><td>{}</td><td class=\"numeric\">{} / {}</td><td class=\"numeric\">{} / {}</td><td class=\"numeric\">{} / {}</td></tr>",
                escape(name),
                number(&value["baseline"]["p50_ns"], 1e6),
                number(&value["baseline"]["p95_ns"], 1e6),
                number(&value["candidate"]["p50_ns"], 1e6),
                number(&value["candidate"]["p95_ns"], 1e6),
                number(&value["delta"]["p50_percent"], 1.0),
                number(&value["delta"]["p95_percent"], 1.0)
            )?;
        }
    }
    table_end(output);
    if let Some(workloads) = document.data["workloads"].as_object() {
        for (name, value) in workloads {
            write!(
                output,
                "<details><summary>{} — round distributions</summary>",
                escape(name)
            )?;
            table_start(
                output,
                "Ordered paired rounds (ms)",
                &[
                    "Round",
                    "Base p50",
                    "Candidate p50",
                    "Base p95",
                    "Candidate p95",
                ],
            )?;
            let left = value["baseline"]["rounds"].as_array();
            let right = value["candidate"]["rounds"].as_array();
            let count = left.map_or(0, Vec::len).max(right.map_or(0, Vec::len));
            for index in 0..count {
                let a = left
                    .and_then(|values| values.get(index))
                    .unwrap_or(&Value::Null);
                let b = right
                    .and_then(|values| values.get(index))
                    .unwrap_or(&Value::Null);
                write!(
                    output,
                    "<tr><td>{}</td><td class=\"numeric\">{}</td><td class=\"numeric\">{}</td><td class=\"numeric\">{}</td><td class=\"numeric\">{}</td></tr>",
                    index + 1,
                    number(&a["p50_ns"], 1e6),
                    number(&b["p50_ns"], 1e6),
                    number(&a["p95_ns"], 1e6),
                    number(&b["p95_ns"], 1e6)
                )?;
            }
            table_end(output);
            output.push_str("</details>");
        }
    }
    json_details(
        output,
        "Latency binary identities, workload digest and paired metadata",
        &document.data,
    )?;
    output.push_str("</section>");
    Ok(())
}

fn discovery(output: &mut String, document: &Document) -> ToolResult<()> {
    output.push_str("<section class=\"document\"><h3>Workspace discovery and process memory</h3><p class=\"note\">Cold readiness includes semantic validation and polling. Protocol initialization and complete disk discovery are different phases. Baseline absence or incomplete symbols/references means changed work, never a comparable speedup. Fresh processes do not flush OS caches.</p>");
    if let Some(datasets) = document.data["datasets"].as_object() {
        for (count, data) in datasets {
            write!(output, "<h4>{} files</h4>", escape(count))?;
            table_start(
                output,
                "Discovery semantic coverage",
                &["Variant", "Complete rounds", "Semantic observations"],
            )?;
            for variant in ["baseline", "candidate"] {
                write!(
                    output,
                    "<tr><td>{variant}</td><td>{}</td><td>",
                    escape(&text(&data[variant]["completed_rounds"]))
                )?;
                json_details(
                    output,
                    "Exact symbols, references, readiness and root-removal answers",
                    &data[variant]["semantic_observations"],
                )?;
                output.push_str("</td></tr>");
            }
            table_end(output);
            paired_metrics(
                output,
                data,
                "cold_medians_ns",
                "Cold observations (ms)",
                1e6,
            )?;
            table_start(
                output,
                "Warm request latency (ms); check semantic scope before interpreting",
                &["Workload", "Base p50 / p95", "Candidate p50 / p95"],
            )?;
            let mut names = BTreeSet::new();
            for variant in ["baseline", "candidate"] {
                if let Some(workloads) = data[variant]["workloads"].as_object() {
                    names.extend(workloads.keys());
                }
            }
            for name in names {
                write!(
                    output,
                    "<tr><td>{}</td><td class=\"numeric\">{} / {}</td><td class=\"numeric\">{} / {}</td></tr>",
                    escape(name),
                    number(&data["baseline"]["workloads"][name]["p50_ns"], 1e6),
                    number(&data["baseline"]["workloads"][name]["p95_ns"], 1e6),
                    number(&data["candidate"]["workloads"][name]["p50_ns"], 1e6),
                    number(&data["candidate"]["workloads"][name]["p95_ns"], 1e6)
                )?;
            }
            table_end(output);
            paired_metrics(
                output,
                data,
                "memory_medians_bytes",
                "Process memory lifecycle medians (MiB)",
                1_048_576.0,
            )?;
            for variant in ["baseline", "candidate"] {
                for observation in data[variant]["memory_observations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    write!(
                        output,
                        "<details><summary>{variant} · memory round {} · {}</summary>",
                        escape(&text(&observation["round"])),
                        escape(&text(&observation["status"]))
                    )?;
                    timeline(output, &observation["samples"], &observation["checkpoints"])?;
                    json_details(
                        output,
                        "Phase checkpoints, native metrics, descriptions and observation errors",
                        observation,
                    )?;
                    output.push_str("</details>");
                }
                json_details(
                    output,
                    &format!("{variant} warm per-round distributions and initial completion"),
                    &data[variant],
                )?;
            }
        }
    }
    output.push_str("<p class=\"note\">RSS is actual resident process memory, not allocated heap bytes. Sampled peaks are lower bounds; kernel high-watermarks cover process lifetime, not phase deltas. Immediate and settled root-removal measurements do not promise that allocators return pages to the OS. Missing native metrics remain unavailable, never zero.</p>");
    json_details(
        output,
        "Discovery provenance and native memory definitions",
        &document.data["metadata"],
    )?;
    output.push_str("</section>");
    Ok(())
}

fn paired_metrics(
    output: &mut String,
    data: &Value,
    field: &str,
    caption: &str,
    divisor: f64,
) -> ToolResult<()> {
    let mut names = BTreeSet::new();
    for variant in ["baseline", "candidate"] {
        if let Some(fields) = data[variant][field].as_object() {
            names.extend(fields.keys());
        }
    }
    table_start(output, caption, &["Observation", "Baseline", "Candidate"])?;
    for name in names {
        write!(
            output,
            "<tr><td>{}</td><td class=\"numeric\">{}</td><td class=\"numeric\">{}</td></tr>",
            escape(name),
            number(&data["baseline"][field][name], divisor),
            number(&data["candidate"][field][name], divisor)
        )?;
    }
    table_end(output);
    Ok(())
}

fn timeline(output: &mut String, samples: &Value, checkpoints: &Value) -> ToolResult<()> {
    let (sample_end, max_bytes, count) = samples
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|sample| {
            Some((
                sample["elapsed_ns"].as_f64()?,
                sample["rss_bytes"].as_f64()?,
            ))
        })
        .fold(
            (0.0_f64, 0.0_f64, 0_usize),
            |(end, peak, count), (time, bytes)| (end.max(time), peak.max(bytes), count + 1),
        );
    let max_time = checkpoints
        .as_object()
        .into_iter()
        .flat_map(|values| values.values())
        .filter_map(|point| point["elapsed_ns"].as_f64())
        .fold(sample_end, f64::max);
    if count == 0 {
        output.push_str("<p>Resident timeline unavailable. See observation errors and native metrics below.</p>");
        return Ok(());
    }
    output.push_str("<figure><svg viewBox=\"0 0 900 260\" role=\"img\" aria-label=\"Observed RSS over elapsed process time with phase checkpoint markers\"><path d=\"M60 20V215H875\" fill=\"none\" stroke=\"#526367\"/>");
    write!(
        output,
        "<text x=\"65\" y=\"18\" fill=\"#202d30\" font-size=\"13\">Peak observed {:.2} MiB</text><text x=\"65\" y=\"247\" fill=\"#202d30\" font-size=\"13\">0 seconds</text><text x=\"750\" y=\"247\" fill=\"#202d30\" font-size=\"13\">{:.3} seconds</text>",
        max_bytes / 1_048_576.0,
        max_time / 1e9
    )?;
    // Draw separate segments across missing samples: never interpolate through observer errors.
    let mut segment = String::new();
    for sample in samples.as_array().into_iter().flatten() {
        if let (Some(time), Some(bytes)) =
            (sample["elapsed_ns"].as_f64(), sample["rss_bytes"].as_f64())
        {
            let x = 60.0 + time / max_time.max(1.0) * 810.0;
            let y = 215.0 - bytes / max_bytes.max(1.0) * 185.0;
            write!(segment, "{x:.2},{y:.2} ")?;
        } else if !segment.is_empty() {
            write!(
                output,
                "<polyline points=\"{segment}\" fill=\"none\" stroke=\"#075f64\" stroke-width=\"2\"/>"
            )?;
            segment.clear();
        }
    }
    if !segment.is_empty() {
        write!(
            output,
            "<polyline points=\"{segment}\" fill=\"none\" stroke=\"#075f64\" stroke-width=\"2\"/>"
        )?;
    }
    if let Some(phases) = checkpoints.as_object() {
        for (name, point) in phases {
            if let Some(time) = point["elapsed_ns"].as_f64() {
                let x = 60.0 + time / max_time.max(1.0) * 810.0;
                write!(
                    output,
                    "<path d=\"M{x:.2} 25V215\" stroke=\"#87958e\" stroke-dasharray=\"4 4\"><title>{}: {:.3}s</title></path>",
                    escape(name),
                    time / 1e9
                )?;
            }
        }
    }
    output.push_str("</svg><figcaption>Observed resident memory. Phase markers are recorded checkpoints; dashed lines are not collection barriers. Missing samples break the line.</figcaption></figure>");
    table_start(
        output,
        "Process phase checkpoints and native metrics",
        &[
            "Phase",
            "Elapsed ms",
            "RSS MiB",
            "Kernel high-watermark MiB",
            "Other native metrics",
        ],
    )?;
    if let Some(phases) = checkpoints.as_object() {
        for (name, point) in phases {
            write!(
                output,
                "<tr><td>{}</td><td class=\"numeric\">{}</td><td class=\"numeric\">{}</td><td class=\"numeric\">{}</td><td>",
                escape(name),
                number(&point["elapsed_ns"], 1e6),
                number(&point["rss_bytes"], 1_048_576.0),
                number(&point["kernel_high_watermark_bytes"], 1_048_576.0)
            )?;
            json_details(output, "Native metric values, units and definitions", point)?;
            output.push_str("</td></tr>");
        }
    }
    table_end(output);
    Ok(())
}

fn heap(output: &mut String, document: &Document) -> ToolResult<()> {
    output.push_str("<section class=\"document\"><h3>DHAT allocation evidence</h3><p class=\"note\">All thirteen candidate scenarios use separate symbolized optimized builds. Allocator events are not RSS and are never timing evidence. Global peak-live bytes are simultaneous allocations at the global peak, not the sum of each allocation point’s independent maximum.</p>");
    heap_table(
        output,
        document.data["profiles"]
            .as_array()
            .map_or(&[], Vec::as_slice),
    )?;
    artifact_links(output, &document.links)?;
    output.push_str("<p class=\"note\">Open a downloaded dhat-heap.json in the DHAT viewer (Valgrind dh_view.html, installed separately). The report itself needs no viewer or network access.</p>");
    json_details(
        output,
        "DHAT binary/symbol identities, build commands and semantic checks",
        &document.data,
    )?;
    output.push_str("</section>");
    Ok(())
}

fn heap_table(output: &mut String, profiles: &[Value]) -> ToolResult<()> {
    table_start(
        output,
        "Heap totals and live allocation lifecycle",
        &[
            "Scenario",
            "Status",
            "Allocated bytes / blocks",
            "Peak live bytes / blocks",
            "End live bytes / blocks",
        ],
    )?;
    for profile in profiles {
        write!(
            output,
            "<tr><td>{}</td><td>{}</td><td class=\"numeric\">{} / {}</td><td class=\"numeric\">{} / {}</td><td class=\"numeric\">{} / {}</td></tr>",
            escape(&text(&profile["name"])),
            status(profile["status"].as_str().unwrap_or("unknown")),
            number(&profile["total_allocated_bytes"], 1.0),
            number(&profile["total_allocated_blocks"], 1.0),
            number(&profile["global_peak_live_bytes"], 1.0),
            number(&profile["global_peak_live_blocks"], 1.0),
            number(&profile["end_live_bytes"], 1.0),
            number(&profile["end_live_blocks"], 1.0)
        )?;
    }
    table_end(output);
    Ok(())
}

fn profile(output: &mut String, document: &Document) -> ToolResult<()> {
    let data = &document.data;
    write!(
        output,
        "<section class=\"document\"><h3>{} · {}</h3><p>Profiler status: {}. Separate diagnostic process, not clean latency evidence.</p>",
        escape(&text(&data["scenario"])),
        escape(&text(&data["backend"])),
        status(&document.status)
    )?;
    if !data["capture_scope"].is_null() {
        write!(
            output,
            "<p>Capture scope: {}. Target PID: {}. Filter this PID in system-wide traces.</p>",
            escape(&text(&data["capture_scope"])),
            escape(&text(&data["target_pid"]))
        )?;
    }
    artifact_links(output, &document.links)?;
    if data["heap_summary"].is_object() {
        let mut value = data["heap_summary"].clone();
        value["name"] = data["scenario"].clone();
        value["status"] = data["status"].clone();
        heap_table(output, &[value])?;
    }
    if data["backend"] == "samply"
        && matches!(
            data["scenario"].as_str(),
            Some("discovery-10" | "discovery-1000" | "discovery-10000" | "latency")
        )
        && super::data::TARGETS.contains(&data["target"].as_str().unwrap_or_default())
    {
        write!(
            output,
            "<h4>Open the CPU profile</h4><p>Use the downloaded run directory. The runner verifies the selected trace and supplies its packaged symbols to samply; this opens a viewer, not a new measurement.</p><pre>cargo perf open &lt;downloaded-run-directory&gt; --cpu --scenario {} --target {}</pre>",
            escape(&text(&data["scenario"])),
            escape(&text(&data["target"])),
        )?;
    } else if let Some(args) = data["profiler"]["viewer_command"]
        .as_array()
        .filter(|args| !args.is_empty())
    {
        output.push_str("<h4>Open the original trace</h4><p>Run the recorded viewer command on a machine with the named tool installed. Replace hosted paths with the downloaded trace’s local path; no command is run by this page.</p><pre>");
        for (index, arg) in args.iter().enumerate() {
            if index > 0 {
                output.push(' ');
            }
            output.push_str(&escape(&format!("{:?}", arg.as_str().unwrap_or_default())));
        }
        output.push_str("</pre>");
    } else {
        let instruction = match data["backend"].as_str() {
            Some("samply") => {
                "Install samply, then run: samply load <downloaded-profile.json.gz>. This starts the compatible local profile viewer; a static file link is not a viewer URL."
            }
            Some("perf") => {
                "On Linux with perf installed, run: perf report -i <downloaded-perf.data>. Keep matching binary/debug symbols available for symbolization."
            }
            Some("xctrace") => {
                "On macOS with Xcode installed, open the downloaded .trace bundle in Instruments. Preserve its directory layout."
            }
            Some("wpr") => {
                "On Windows with Windows Performance Analyzer installed, open the downloaded .etl file in WPA."
            }
            Some("dhat") => {
                "Use Valgrind’s dh_view.html DHAT viewer and load the downloaded dhat-heap.json file."
            }
            _ => {
                "No supported viewer command was recorded. Inspect the original trace manifest and install the named profiler; this report does not claim the trace has been opened."
            }
        };
        write!(output, "<p>{}</p>", escape(instruction))?;
    }
    table_start(
        output,
        "Recorded profiling phase markers",
        &["Phase", "Elapsed ms"],
    )?;
    for phase in data["phases"].as_array().into_iter().flatten() {
        write!(
            output,
            "<tr><td>{}</td><td class=\"numeric\">{}</td></tr>",
            escape(&text(&phase["name"])),
            number(&phase["elapsed_ns"], 1e6)
        )?;
    }
    table_end(output);
    json_details(
        output,
        "Profile provenance, symbol identities, tool versions, commands and semantic result",
        data,
    )?;
    output.push_str("</section>");
    Ok(())
}

fn artifact_links(output: &mut String, artifacts: &[Artifact]) -> ToolResult<()> {
    if artifacts.is_empty() {
        output.push_str("<p>No verified artifact files available.</p>");
        return Ok(());
    }
    output.push_str("<ul class=\"artifacts\">");
    for artifact in artifacts {
        write!(
            output,
            "<li><a href=\"{}\" download>{}</a> <span class=\"note\">{} bytes · {}</span><code>SHA256 {}</code></li>",
            href(&artifact.path),
            escape(&artifact.path),
            artifact.size,
            escape(&artifact.kind),
            escape(&artifact.sha256)
        )?;
    }
    output.push_str("</ul>");
    Ok(())
}

fn md(value: &str) -> String {
    // Markdown is also an offline artifact; prohibit raw HTML and table/link injection.
    escape(value)
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('`', "\\`")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace(['\n', '\r'], " ")
}

pub(super) fn markdown(report: &Report) -> ToolResult<String> {
    let mut output = format!(
        "# Bend 2 performance evidence\n\nRun status: **{}**.\n\n{}\n\n[Offline HTML](./index.html) · [Unified JSON](./unified-report.json)\n\n",
        md(&report.status),
        report.policy
    );
    writeln!(output, "Coverage scope: {}.\n", report.coverage_scope)?;
    for issue in &report.issues {
        writeln!(output, "- Coverage/provenance: {}", md(issue))?;
    }
    for error in &report.errors {
        writeln!(
            output,
            "- Failed provenance/hosted execution: {}",
            md(error)
        )?;
    }
    for target in &report.targets {
        write!(
            output,
            "\n## {}\n\nEvidence status: **{}**.\n\n",
            md(&target.target),
            md(&target.status)
        )?;
        for (label, values) in [
            ("Failed", &target.errors),
            ("Incomplete/absent", &target.missing),
            ("Changed scope", &target.changed_scope),
            ("Regression", &target.regressions),
        ] {
            for value in values {
                writeln!(output, "- {label}: {}", md(value))?;
            }
        }
        for field in ["base_sha", "candidate_sha", "repository", "request_id"] {
            if !target.provenance[field].is_null() {
                writeln!(
                    output,
                    "- {field}: {}",
                    md(&text(&target.provenance[field]))
                )?;
            }
        }
        markdown_callgrind(&mut output, target)?;
        for document in &target.documents {
            if document.kind == "discovery-raw" {
                continue;
            }
            markdown_document(&mut output, document)?;
        }
        output.push_str("\n### Verified original artifacts\n\n");
        for artifact in &target.artifacts {
            writeln!(
                output,
                "- [{}]({}) — {} bytes; SHA256 {}",
                md(&artifact.path),
                href(&artifact.path),
                artifact.size,
                artifact.sha256
            )?;
        }
    }
    Ok(output)
}

fn markdown_document(output: &mut String, document: &Document) -> ToolResult<()> {
    write!(
        output,
        "\n### {} — {}\n\n[Original evidence]({})\n\n",
        md(&document.kind),
        md(&document.status),
        href(&document.path)
    )?;
    match document.kind.as_str() {
        "latency" => markdown_latency(output, &document.data)?,
        "discovery" => markdown_discovery(output, &document.data)?,
        "memory" => {
            output.push_str("Candidate-only DHAT, not RSS or latency evidence.\n\n| Scenario | Status | Total bytes | Global peak live bytes | End live bytes |\n|---|---|---:|---:|---:|\n");
            for profile in document.data["profiles"].as_array().into_iter().flatten() {
                writeln!(
                    output,
                    "| {} | {} | {} | {} | {} |",
                    md(&text(&profile["name"])),
                    md(&text(&profile["status"])),
                    number(&profile["total_allocated_bytes"], 1.0),
                    number(&profile["global_peak_live_bytes"], 1.0),
                    number(&profile["end_live_bytes"], 1.0)
                )?;
            }
        }
        "profile" => {
            writeln!(
                output,
                "- Scenario: {}. Backend: {}.",
                md(&text(&document.data["scenario"])),
                md(&text(&document.data["backend"]))
            )?;
            if !document.data["profiler"]["viewer_command"].is_null() {
                writeln!(
                    output,
                    "- Recorded viewer argument vector (replace hosted paths with local downloaded paths): {}",
                    md(&text(&document.data["profiler"]["viewer_command"]))
                )?;
            }
        }
        _ => {}
    }
    for artifact in &document.links {
        writeln!(
            output,
            "- [Download {}]({}) — SHA256 {}",
            md(&artifact.path),
            href(&artifact.path),
            artifact.sha256
        )?;
    }
    Ok(())
}

fn markdown_latency(output: &mut String, data: &Value) -> ToolResult<()> {
    output.push_str("| Workload | Base p50/p95 ms | Candidate p50/p95 ms | p50/p95 change % |\n|---|---:|---:|---:|\n");
    if let Some(workloads) = data["workloads"].as_object() {
        for (name, value) in workloads {
            writeln!(
                output,
                "| {} | {} / {} | {} / {} | {} / {} |",
                md(name),
                number(&value["baseline"]["p50_ns"], 1e6),
                number(&value["baseline"]["p95_ns"], 1e6),
                number(&value["candidate"]["p50_ns"], 1e6),
                number(&value["candidate"]["p95_ns"], 1e6),
                number(&value["delta"]["p50_percent"], 1.0),
                number(&value["delta"]["p95_percent"], 1.0)
            )?;
        }
    }
    output.push_str("\nMedians of round percentiles, not pooled requests. Ordered round distributions are retained in HTML and JSON. Report-only numerical changes.\n");
    Ok(())
}

fn markdown_discovery(output: &mut String, data: &Value) -> ToolResult<()> {
    if let Some(datasets) = data["datasets"].as_object() {
        for (count, data) in datasets {
            writeln!(
                output,
                "- {} files: completed rounds baseline {}, candidate {}.",
                md(count),
                md(&text(&data["baseline"]["completed_rounds"])),
                md(&text(&data["candidate"]["completed_rounds"]))
            )?;
            for (field, unit, divisor) in [
                ("cold_medians_ns", "ms", 1e6),
                ("memory_medians_bytes", "MiB", 1_048_576.0),
            ] {
                write!(
                    output,
                    "\n| Observation ({unit}) | Baseline | Candidate |\n|---|---:|---:|\n"
                )?;
                let mut names = BTreeSet::new();
                for variant in ["baseline", "candidate"] {
                    if let Some(fields) = data[variant][field].as_object() {
                        names.extend(fields.keys());
                    }
                }
                for name in names {
                    writeln!(
                        output,
                        "| {} | {} | {} |",
                        md(name),
                        number(&data["baseline"][field][name], divisor),
                        number(&data["candidate"][field][name], divisor)
                    )?;
                }
            }
        }
    }
    output.push_str("\nRSS is not allocator bytes. Sampled peak is a lower bound; high-watermarks cover process lifetime. Phase timeline, native definitions and semantic answers are in offline HTML / JSON.\n");
    Ok(())
}

fn callgrind_status(output: &mut String, target: &TargetEvidence) -> ToolResult<()> {
    if !target
        .documents
        .iter()
        .any(|document| document.kind == "callgrind")
    {
        output.push_str("<p class=\"note\">Callgrind gate evidence is not collected in this bundle. No Callgrind pass is inferred; existing workflow gates remain separate and unchanged.</p>");
        return Ok(());
    }
    output.push_str("<section class=\"document\"><h3>Canonical Callgrind gates</h3><p class=\"note\">Original workflow gate outcomes, separate from report-only native measurements. Thresholds and metric policy are not reinterpreted by this report. A failed workflow step is not automatically classified as a numerical regression.</p>");
    let manifest = if target.provenance["mode"] == "callgrind" {
        &target.provenance
    } else {
        &target.provenance["callgrind_manifest"]
    };
    if let Some(statuses) = manifest["statuses"].as_object() {
        table_start(
            output,
            "Recorded Callgrind workflow outcomes",
            &["Step / gate", "Original outcome"],
        )?;
        for (name, value) in statuses {
            let outcome = value
                .as_str()
                .or_else(|| value["status"].as_str())
                .unwrap_or("unknown");
            write!(
                output,
                "<tr><td>{}</td><td>{}</td></tr>",
                escape(name),
                escape(outcome)
            )?;
        }
        table_end(output);
    } else {
        output.push_str("<p>Gate execution outcomes were not recorded. Original raw gate evidence is retained below; no passing gate verdict is inferred.</p>");
    }
    output.push_str("</section>");
    Ok(())
}

fn markdown_callgrind(output: &mut String, target: &TargetEvidence) -> ToolResult<()> {
    if !target
        .documents
        .iter()
        .any(|document| document.kind == "callgrind")
    {
        output.push_str(
            "\nCallgrind gates: not collected in this bundle; no passing verdict inferred.\n",
        );
        return Ok(());
    }
    output.push_str(
        "\nCanonical Callgrind workflow outcomes (separate from report-only native numbers):\n",
    );
    let manifest = if target.provenance["mode"] == "callgrind" {
        &target.provenance
    } else {
        &target.provenance["callgrind_manifest"]
    };
    if let Some(statuses) = manifest["statuses"].as_object() {
        for (name, value) in statuses {
            writeln!(output, "- {}: {}", md(name), md(&text(value)))?;
        }
    } else {
        output.push_str("\nGate execution outcomes unavailable; see original raw evidence. No passing verdict inferred.\n");
    }
    Ok(())
}

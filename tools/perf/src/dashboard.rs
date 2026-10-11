use crate::{ToolResult, common};
use clap::Parser;
use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
};

mod data;
mod render;
#[cfg(test)]
mod tests;

#[derive(Parser)]
#[command(
    no_binary_name = true,
    about = "Generate an offline performance report from hosted artifacts; never collect measurements"
)]
struct DashboardArgs {
    root: PathBuf,
    /// Assess only the uploaded target bundle; global matrix coverage remains unevaluated.
    #[arg(long)]
    target_only: bool,
}

pub(crate) fn run(args: &[String]) -> ToolResult<()> {
    let args = match DashboardArgs::try_parse_from(args) {
        Ok(args) => args,
        Err(error) if !error.use_stderr() => {
            error.print()?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    publish(&args.root, args.target_only)?.result()
}

fn publish(root: &Path, target_only: bool) -> ToolResult<data::Report> {
    let root = root.canonicalize()?;
    if !root.is_dir() {
        return Err("Performance report root must be a directory".into());
    }
    let report = data::collect_scoped(&root, target_only)?;
    let html = render::html(&report)?;
    let markdown = render::markdown(&report)?;
    for name in ["index.html", "unified-report.json", "summary.md"] {
        let path = root.join(name);
        if path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(format!("Refusing to replace symlink report output: {name}").into());
        }
    }
    common::write_json(
        &root.join("unified-report.json"),
        &serde_json::to_value(&report)?,
    )?;
    write_text(&root.join("index.html"), &html)?;
    write_text(&root.join("summary.md"), &markdown)?;
    Ok(report)
}

fn write_text(path: &Path, text: &str) -> ToolResult<()> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().ok_or("Missing output directory")?)?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub(crate) fn generate(root: &Path) -> ToolResult<()> {
    let report = publish(root, false)?;
    report.result()
}

pub(crate) fn cpu_profile_manifest(
    root: &Path,
    target_name: &str,
    scenario: Option<&str>,
) -> ToolResult<(PathBuf, String)> {
    let root = root.canonicalize()?;
    let report = publish(&root, false)?;
    if !report.errors.is_empty() {
        return Err("Aggregate source/provenance validation failed; inspect index.html before opening a profile".into());
    }
    let mut selected = None;
    for target in &report.targets {
        if target.target != target_name || !target.errors.is_empty() {
            continue;
        }
        for document in &target.documents {
            if document.kind != "profile"
                || document.status != "complete"
                || document.data["backend"] != "samply"
                || scenario.is_some_and(|name| document.data["scenario"] != name)
            {
                continue;
            }
            if selected.replace(document.path.as_str()).is_some() {
                return Err(
                    "Several CPU profiles match; select exactly one with --scenario".into(),
                );
            }
        }
    }
    let relative =
        selected.ok_or("No validated complete CPU profile matches the selected target/scenario")?;
    let path = data::confined(&root, &root, relative)?;
    Ok((path, report.status))
}

pub(crate) fn open(path: &Path) -> ToolResult<()> {
    let root = if path.is_dir() {
        path
    } else if path.file_name().is_some_and(|name| name == "index.html") {
        path.parent().ok_or("Report HTML has no parent directory")?
    } else {
        return Err("Open expects a downloaded run directory or its index.html".into());
    };
    // Even failed/incomplete evidence deserves a readable diagnostic surface.
    let report = publish(root, false)?;
    let file = root.canonicalize()?.join("index.html");
    let url = url::Url::from_file_path(&file).map_err(|()| "Cannot form report file URL")?;
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(target_os = "windows") {
        let mut command = Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else {
        Command::new("xdg-open")
    };
    let status = command.arg(url.as_str()).status().map_err(|error| {
        format!(
            "Cannot open report (open {} in a browser): {error}",
            file.display()
        )
    })?;
    if !status.success() {
        return Err(format!(
            "System browser opener failed; open {} manually",
            file.display()
        )
        .into());
    }
    report.result()
}

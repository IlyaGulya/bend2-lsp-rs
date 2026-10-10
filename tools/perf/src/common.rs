use crate::ToolResult;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
};

pub(crate) fn require_ci() -> ToolResult<()> {
    if std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true") {
        return Err("Performance measurements require hosted CI; local correctness/report checks remain allowed".into());
    }
    Ok(())
}

pub(crate) fn native_target() -> ToolResult<&'static str> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        _ => return Err("Unsupported native performance target".into()),
    };
    if let Ok(expected) = std::env::var("PERF_TARGET")
        && expected != target
    {
        return Err(format!("Expected native target {expected}, running {target}").into());
    }
    Ok(target)
}

pub(crate) fn sha256_file(path: &Path) -> ToolResult<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16384];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn write_json(path: &Path, value: &serde_json::Value) -> ToolResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    Ok(())
}

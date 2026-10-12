use crate::{ToolResult, common};
use clap::Parser;
use serde_json::{Value, json};
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) const SAMPLY_VERSION: &str = "0.13.1";
pub(crate) const SAMPLY_SOURCE: &str =
    "https://github.com/mstange/samply/releases/tag/samply-v0.13.1";
pub(crate) const ETL_READER_VERSION: &str =
    "TraceEvent 3.2.8 / .NET SDK 10.0.401 / runtime 10.0.12";

pub(crate) fn samply_source() -> &'static str {
    if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "https://github.com/mstange/samply/tree/da75c28f367454c621e690eeb4e44ec2ebb29a78 + scripts/patches/samply-windows-arm64.patch (modified ARM64 importer; checksum-bound tool provenance)"
    } else {
        SAMPLY_SOURCE
    }
}

fn validate_samply_provenance(provenance: &Value, executable_sha256: &str) -> ToolResult<()> {
    use sha2::{Digest, Sha256};
    let patch_sha256 = format!(
        "{:x}",
        Sha256::digest(include_bytes!(
            "../../../scripts/patches/samply-windows-arm64.patch"
        ))
    );
    for (field, expected) in [
        ("format_version", json!(1)),
        (
            "upstream_revision",
            json!("da75c28f367454c621e690eeb4e44ec2ebb29a78"),
        ),
        (
            "patch_path",
            json!("scripts/patches/samply-windows-arm64.patch"),
        ),
        ("patch_sha256", json!(patch_sha256)),
        ("target", json!("aarch64-pc-windows-msvc")),
        ("executable_sha256", json!(executable_sha256)),
    ] {
        if provenance[field] != expected {
            return Err(format!("Patched ARM64 samply provenance mismatch: {field}").into());
        }
    }
    Ok(())
}

fn validate_etl_reader_provenance(provenance: &Value, executable_sha256: &str) -> ToolResult<()> {
    use sha2::{Digest, Sha256};
    const SOURCES: [(&str, &[u8]); 6] = [
        (
            "Program.cs",
            include_bytes!("../windows-etl-reader/Program.cs"),
        ),
        (
            "Bend2EtlReader.csproj",
            include_bytes!("../windows-etl-reader/Bend2EtlReader.csproj"),
        ),
        (
            "Directory.Build.props",
            include_bytes!("../windows-etl-reader/Directory.Build.props"),
        ),
        (
            "global.json",
            include_bytes!("../windows-etl-reader/global.json"),
        ),
        (
            "NuGet.config",
            include_bytes!("../windows-etl-reader/NuGet.config"),
        ),
        (
            "packages.lock.json",
            include_bytes!("../windows-etl-reader/packages.lock.json"),
        ),
    ];
    let (target, sdk_hash) = if cfg!(target_arch = "aarch64") {
        (
            "win-arm64",
            "8272eaab6f06ad658b1976e19d88beed287a601f968b71d5c26b75d10587cf087665c599d2e136a911002f955c97f59aa8692581bbf6b8e7af5f82604c810256",
        )
    } else {
        (
            "win-x64",
            "24b670ad3d923bfcf47df6c3b034152398b42f6dbc388e10d783aee1cfb5e5817d399fc0ae2a12cfa822a55e61d34830ccb15c50ef6efee437ab874bb7c79430",
        )
    };
    if provenance["format_version"] != 1 {
        return Err("Unsupported native ETL reader provenance".into());
    }
    for (field, expected) in [
        ("sdk_version", "10.0.401"),
        ("runtime_version", "10.0.12"),
        ("traceevent_version", "3.2.8"),
        ("sdk_archive_sha512", sdk_hash),
        ("target", target),
        ("executable_sha256", executable_sha256),
    ] {
        if provenance[field].as_str() != Some(expected) {
            return Err(format!("Native ETL reader provenance mismatch: {field}").into());
        }
    }
    for (name, bytes) in SOURCES {
        let expected = format!("{:x}", Sha256::digest(bytes));
        if provenance["sources"][name].as_str() != Some(expected.as_str()) {
            return Err(format!("Native ETL reader source mismatch: {name}").into());
        }
    }
    Ok(())
}

#[derive(Parser)]
struct Arguments {
    #[arg(long, default_value = "hosted", value_parser = ["hosted", "cpu", "heap", "native"])]
    backend: String,
    #[arg(long, default_value = "cpu", value_parser = ["cpu", "heap"])]
    native_kind: String,
    #[arg(long)]
    binary: Option<PathBuf>,
    #[arg(long, conflicts_with = "human")]
    json: bool,
    #[arg(long)]
    human: bool,
}

pub(crate) fn output(program: &str, args: &[&str]) -> ToolResult<String> {
    let result = Command::new(program).args(args).output()?;
    if !result.status.success() {
        return Err(format!(
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    let mut text = String::from_utf8(result.stdout)?;
    text.push_str(&String::from_utf8(result.stderr)?);
    Ok(text.trim().to_owned())
}

fn executable(program: &str) -> ToolResult<PathBuf> {
    let paths = env::var_os("PATH").ok_or("PATH is not defined")?;
    for directory in env::split_paths(&paths) {
        for suffix in if cfg!(windows) {
            &["", ".exe"][..]
        } else {
            &[""][..]
        } {
            let path = directory.join(format!("{program}{suffix}"));
            if path.is_file() {
                return Ok(path.canonicalize()?);
            }
        }
    }
    Err(format!("{program} is not installed on PATH").into())
}

pub(crate) fn tool_identity(program: &str, version: &str) -> ToolResult<Value> {
    let path = executable(program)?;
    let sha256 = common::sha256_file(&path)?;
    let mut identity =
        json!({"program": program, "path": path, "sha256": sha256, "version": version});
    if program == "bend2-etl-reader"
        || (program == "samply" && cfg!(all(target_os = "windows", target_arch = "aarch64")))
    {
        let mut provenance_path = path.as_os_str().to_os_string();
        provenance_path.push(".bend-perf-source.json");
        let provenance: Value = serde_json::from_reader(fs::File::open(provenance_path)?)?;
        if program == "samply" {
            validate_samply_provenance(&provenance, &sha256)?;
            identity["source"] = json!(samply_source());
        } else {
            validate_etl_reader_provenance(&provenance, &sha256)?;
            identity["source"] = json!("Microsoft TraceEvent: full-EOF target-relevant ETL export");
        }
        identity["source_provenance"] = provenance;
    }
    Ok(identity)
}

fn check(name: &str, result: ToolResult<String>, remediation: &str) -> Value {
    match result {
        Ok(detail) => json!({"name": name, "status": "ready", "detail": detail}),
        Err(error) => {
            json!({"name": name, "status": "unavailable", "detail": error.to_string(), "remediation": remediation})
        }
    }
}

fn linux_permissions() -> ToolResult<String> {
    let paranoid = fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")?;
    let value: i32 = paranoid.trim().parse()?;
    let uid = output("id", &["-u"])?;
    if value > 1 && uid != "0" {
        return Err(format!(
            "perf_event_paranoid={value}, uid={uid}; attach sampling requires perf-event access"
        )
        .into());
    }
    Ok(format!(
        "perf_event_paranoid={value}; uid={uid}; actual attach must also pass kernel/security policy"
    ))
}

fn mac_permissions() -> ToolResult<String> {
    let status = output("DevToolsSecurity", &["-status"])?;
    if !status.to_ascii_lowercase().contains("enabled") {
        return Err(format!("Developer tool authorization is not enabled: {status}").into());
    }
    Ok(format!(
        "{status}; target must permit task_for_pid; hardened runtime/SIP targets may still reject attach"
    ))
}

fn windows_permissions() -> ToolResult<String> {
    let admin = output(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
        ],
    )?;
    if admin.trim() != "True" {
        return Err(
            "ETW kernel/heap recording requires an explicitly elevated Administrator session"
                .into(),
        );
    }
    Ok("Explicitly elevated Administrator token available for ETW recording".to_owned())
}

pub(crate) fn kernel_logger_running(text: &str) -> bool {
    text.contains("NT Kernel Logger") || text.contains("NTKernelLogger")
}

fn dhat_binary(binary: Option<&Path>) -> ToolResult<String> {
    const MARKER: &[u8] = b"BEND2_LSP_DHAT_FILE";
    let binary =
        binary.ok_or("Heap doctor requires --binary pointing to a --features dhat-heap build")?;
    let mut file = fs::File::open(binary)?;
    let mut buffer = [0_u8; 16_384];
    let mut retained = 0;
    loop {
        let count = file.read(&mut buffer[retained..])?;
        if count == 0 {
            return Err("Binary lacks the DHAT-enabled application environment hook".into());
        }
        let length = retained + count;
        if buffer[..length]
            .windows(MARKER.len())
            .any(|window| window == MARKER)
        {
            return Ok(format!(
                "DHAT application hook present in {}; collection still verifies allocator output, PID and symbols",
                binary.display()
            ));
        }
        retained = MARKER.len() - 1;
        if length < retained {
            retained = length;
        }
        buffer.copy_within(length - retained..length, 0);
    }
}

fn hosted_checks(checks: &mut Vec<Value>) {
    for (name, args, remedy) in [
        (
            "cargo",
            vec!["--version"],
            "Install the repository's pinned Rust toolchain with rustup.",
        ),
        (
            "rustc",
            vec!["-vV"],
            "Install the repository's pinned Rust toolchain with rustup.",
        ),
        (
            "gh",
            vec!["--version"],
            "Install GitHub CLI from https://cli.github.com/.",
        ),
    ] {
        checks.push(check(name, output(name, &args), remedy));
    }
    checks.push(check(
        "github-authentication",
        output("gh", &["auth", "status"]),
        "Run gh auth login explicitly; the doctor never signs in or changes tokens.",
    ));
    checks.push(check(
        "github-repository",
        output("gh", &["repo", "view", "--json", "nameWithOwner"]),
        "Run from the repository checkout with a configured GitHub remote and read access.",
    ));
}

fn linux_checks(checks: &mut Vec<Value>, backend: &str, native_kind: &str) {
    if backend == "native" {
        checks.push(check("perf", output("perf", &["--version"]), "Install the Linux perf package matching the running kernel (linux-tools/linux-perf), including user-space callchain support."));
        if native_kind == "heap" {
            checks.push(check(
                "native-heap",
                Err("Linux perf is a CPU diagnostic backend, not an allocator profiler".into()),
                "Use --heap with a DHAT build for allocator evidence on Linux.",
            ));
        }
    }
    checks.push(check("perf-event-permissions", linux_permissions(), "An operator must explicitly permit perf events on the hosted runner (for example kernel.perf_event_paranoid=1 or lower). This command never runs sudo or modifies kernel settings."));
}

fn mac_checks(checks: &mut Vec<Value>, backend: &str, native_kind: &str) {
    checks.push(check(
        "xcode",
        output("xcodebuild", &["-version"]),
        "Install/select a full Xcode release; Command Line Tools alone do not supply Instruments.",
    ));
    let templates = output("xcrun", &["xctrace", "list", "templates"]).and_then(|text| {
        let template = if native_kind == "heap" {
            "Allocations"
        } else {
            "Time Profiler"
        };
        if text.contains(template) {
            Ok(text)
        } else {
            Err(format!("Required Instruments template {template} is absent").into())
        }
    });
    checks.push(check("instruments-templates", templates, "Install full Xcode with Time Profiler and Allocations templates and select it using xcode-select explicitly."));
    checks.push(check("task-port-authorization", mac_permissions(), "An operator must enable developer-tool authorization explicitly (DevToolsSecurity -enable), and authorize the runner account; no implicit privilege changes are made."));
    if backend == "cpu" {
        let entitlement = executable("samply").and_then(|path| {
            let text = output(
                "codesign",
                &[
                    "--display",
                    "--entitlements",
                    ":-",
                    path.to_str().ok_or("Non-Unicode samply path")?,
                ],
            )?;
            if text.contains("com.apple.security.cs.debugger") {
                Ok(text)
            } else {
                Err("samply has no debugger entitlement".into())
            }
        });
        checks.push(check("samply-attach-entitlement", entitlement, "Run the pinned install's explicit samply setup -y once to code-sign its debugger entitlement; doctor never changes signing."));
    }
}

fn windows_checks(checks: &mut Vec<Value>, backend: &str, native_kind: &str) {
    checks.push(check("administrator-token", windows_permissions(), "Use a deliberately elevated hosted runner session for ETW. No UAC prompt/elevation is performed."));
    let profiles = output("wpr", &["-profiles"]).and_then(|text| {
        let expected = if native_kind == "heap" { "Heap" } else { "CPU" };
        if text.contains(expected) {
            Ok(text)
        } else {
            Err(format!("WPR profile {expected} unavailable").into())
        }
    });
    checks.push(check("wpr", profiles, "Use the native Windows Performance Recorder (included in Windows) with CPU/Heap profiles; install Windows ADK Performance Toolkit for analysis."));
    let reader = tool_identity("bend2-etl-reader", ETL_READER_VERSION)
        .and_then(|identity| serde_json::to_string(&identity).map_err(Into::into));
    checks.push(check("etl-reader", reader, "Run the pinned hosted profiling bootstrap; the reader requires the exact checksum-bound source/package/runtime provenance. No installer is run by doctor."));
    if backend == "cpu" {
        checks.push(check("xperf", output("xperf", &["-help"]), "Install Windows ADK Performance Toolkit xperf; samply's Windows ETW backend requires it."));
        let idle = output("xperf", &["-loggers"]).and_then(|text| {
            if kernel_logger_running(&text) {
                Err("An unowned NT Kernel Logger recording is already active".into())
            } else {
                Ok(text)
            }
        });
        checks.push(check("kernel-etw-session-ownership", idle, "Stop the existing recording through its owner before collecting samply; this tool never overwrites/cancels another recording."));
    }
}

pub(crate) fn prerequisites(backend: &str, native_kind: &str, binary: Option<&Path>) -> Value {
    let mut checks = vec![check(
        "native-target",
        common::native_target().map(str::to_owned),
        "Use a supported native Linux/macOS/Windows x64/ARM64 target; PERF_TARGET must match the actual host.",
    )];
    if backend == "hosted" {
        hosted_checks(&mut checks);
    } else if backend == "heap" {
        checks.push(check("dhat-build", dhat_binary(binary), "Build the LSP with --features dhat-heap and DEBUG=1, STRIP=none, then pass --binary; no local profile is run by doctor."));
    } else {
        if backend == "cpu" {
            let version = output("samply", &["--version"]).and_then(|text| {
                if text.split_whitespace().any(|word| word == SAMPLY_VERSION) {
                    if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
                        tool_identity("samply", text.trim())?;
                    }
                    Ok(text)
                } else {
                    Err(format!("Pinned samply {SAMPLY_VERSION} required; found {text}").into())
                }
            });
            checks.push(check("samply", version, &format!("Install samply {SAMPLY_VERSION} from {}; Windows ARM64 additionally requires the checksum-bound importer patch and binary provenance sidecar; no unpinned installer is run by this command.", samply_source())));
        }
        match env::consts::OS {
            "linux" => linux_checks(&mut checks, backend, native_kind),
            "macos" => mac_checks(&mut checks, backend, native_kind),
            "windows" => windows_checks(&mut checks, backend, native_kind),
            _ => checks.push(check(
                "platform",
                Err("Unsupported profiler platform".into()),
                "Use a supported Linux/macOS/Windows x64/ARM64 hosted target.",
            )),
        }
    }
    let ready = checks.iter().all(|item| item["status"] == "ready");
    json!({"format_version": 1, "status": if ready {"ready"} else {"unavailable"}, "backend": backend, "native_kind": native_kind, "os": env::consts::OS, "architecture": env::consts::ARCH, "checks": checks, "policy": "Doctor performs prerequisite inspection only; all performance measurement runs in hosted CI. No implicit administration, installation or authentication."})
}

pub(crate) fn require_backend(
    backend: &str,
    native_kind: &str,
    binary: Option<&Path>,
) -> ToolResult<Value> {
    let report = prerequisites(backend, native_kind, binary);
    if report["status"] != "ready" {
        return Err(format!(
            "Profiler prerequisites unavailable: {}",
            serde_json::to_string(&report)?
        )
        .into());
    }
    Ok(report)
}

pub(crate) fn run(args: &[String]) -> ToolResult<()> {
    let args = Arguments::try_parse_from(
        std::iter::once("doctor".to_owned()).chain(args.iter().cloned()),
    )?;
    let report = prerequisites(&args.backend, &args.native_kind, args.binary.as_deref());
    if !args.json && args.human {
        println!(
            "Performance doctor: {} ({} / {})",
            report["status"],
            env::consts::OS,
            env::consts::ARCH
        );
        for item in report["checks"].as_array().ok_or("Missing doctor checks")? {
            println!("{}: {} — {}", item["name"], item["status"], item["detail"]);
            if let Some(remediation) = item["remediation"].as_str() {
                println!("  Remediation: {remediation}");
            }
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    if report["status"] != "ready" {
        return Err(
            "Requested performance prerequisites are unavailable; see doctor remediation".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn patched_sampler_provenance_binds_revision_patch_target_and_executable() {
        let binary_sha256 = "b5e74cea20fdf49338da233ce186aad9e7a26c11f72df6c70ff80d68c1cc3c1b";
        let provenance = json!({
            "format_version": 1,
            "upstream_revision": "da75c28f367454c621e690eeb4e44ec2ebb29a78",
            "patch_path": "scripts/patches/samply-windows-arm64.patch",
            "patch_sha256": format!("{:x}", Sha256::digest(include_bytes!(
                "../../../scripts/patches/samply-windows-arm64.patch"
            ))),
            "target": "aarch64-pc-windows-msvc",
            "executable_sha256": binary_sha256,
        });
        validate_samply_provenance(&provenance, binary_sha256).unwrap();
        for field in [
            "format_version",
            "upstream_revision",
            "patch_path",
            "patch_sha256",
            "target",
            "executable_sha256",
        ] {
            let mut changed = provenance.clone();
            changed[field] = json!("unrelated source");
            assert!(
                validate_samply_provenance(&changed, binary_sha256).is_err(),
                "{field}"
            );
        }
        assert!(validate_samply_provenance(&provenance, "different binary bytes").is_err());
        assert!(validate_samply_provenance(&Value::Null, binary_sha256).is_err());
    }
}

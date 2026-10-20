use serde::Deserialize;
use std::path::Path;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::server) struct LintConfig {
    pub(super) enabled: bool,
    pub(super) path: String,
    pub(super) arguments: Vec<String>,
    pub(super) dangerous_code_actions: bool,
    pub(super) timeout_ms: u64,
}

impl Default for LintConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: String::new(),
            arguments: Vec::new(),
            dangerous_code_actions: false,
            timeout_ms: 10_000,
        }
    }
}

impl LintConfig {
    pub(super) fn from_settings(settings: &serde_json::Value) -> Result<Self, String> {
        if settings.pointer("/bend2-lsp/lint").is_some() {
            return Err("Misplaced bend2-lsp.lint settings: move them to bend.lint and explicitly set enabled to true to permit external analysis".into());
        }
        let Some(value) = settings.pointer("/bend/lint") else {
            return Ok(Self::default());
        };
        let config: Self = serde_json::from_value(value.clone())
            .map_err(|error| format!("Invalid bend.lint settings: {error}"))?;
        if config.enabled {
            if !Path::new(&config.path).is_absolute() {
                return Err("bend.lint.path must be an explicit absolute executable path; bend-lint remains disabled".into());
            }
            let name = Path::new(&config.path)
                .file_stem()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or_default();
            if matches!(name, "bunx" | "npx" | "npm" | "pnpm" | "yarn") {
                return Err("bend.lint.path must not invoke a package manager; install the analyzer yourself".into());
            }
            if Path::new(&config.path)
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
                })
            {
                return Err("bend.lint.path must be a directly executable program, not a shell launcher; use an absolute Bun executable and lint.arguments for the installed script".into());
            }
            if config
                .arguments
                .iter()
                .any(|argument| argument.starts_with("--fix") || argument == "--worker")
            {
                return Err(
                    "bend.lint.arguments must not enable fixes or internal worker mode".into(),
                );
            }
            if !(1..=120_000).contains(&config.timeout_ms) {
                return Err("bend.lint.timeoutMs must be between 1 and 120000".into());
            }
            let metadata = std::fs::metadata(&config.path)
                .map_err(|error| format!("Selected bend.lint.path '{}' is unavailable: {error}. Install the tool yourself and select its executable", config.path))?;
            if !metadata.is_file() {
                return Err(format!(
                    "Selected bend.lint.path '{}' is not an executable file",
                    config.path
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    return Err(format!(
                        "Selected bend.lint.path '{}' is not executable; select a directly executable tool",
                        config.path
                    ));
                }
            }
            for argument in &config.arguments {
                let script = Path::new(argument);
                if script.is_absolute()
                    && script
                        .extension()
                        .and_then(std::ffi::OsStr::to_str)
                        .is_some_and(|extension| matches!(extension, "ts" | "js" | "mjs" | "cjs"))
                    && !script.is_file()
                {
                    return Err(format!(
                        "Selected analyzer script '{argument}' does not exist; correct bend.lint.arguments"
                    ));
                }
            }
        }
        Ok(config)
    }
}

pub(super) fn installed_identity(config: &LintConfig) -> String {
    use std::io::Read;
    let installed = std::iter::once(&config.path)
        .chain(&config.arguments)
        .map(Path::new)
        .filter(|path| path.is_absolute() && path.file_name().is_some_and(|name| name == "seam.ts"))
        .find_map(|script| {
            let package = script.parent()?.parent()?.parent()?.join("package.json");
            let mut text = String::new();
            std::fs::File::open(package)
                .ok()?
                .take(64 * 1024)
                .read_to_string(&mut text)
                .ok()?;
            let metadata: serde_json::Value = serde_json::from_str(&text).ok()?;
            (metadata["name"] == "bend-lint").then(|| {
                serde_json::json!({
                    "version":metadata["version"], "bendRelease":metadata["bendRelease"],
                    "bendWindows":metadata["bendWindows"],
                })
            })
        });
    installed.map_or_else(
        || "Tool/compiler versions unknown: upstream JSON contains no version identity".into(),
        |metadata| {
            format!("Installed bend-lint package metadata (not executable attestation): {metadata}")
        },
    )
}

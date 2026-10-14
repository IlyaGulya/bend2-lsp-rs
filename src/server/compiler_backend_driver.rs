use super::compiler_backend::{
    BackendSource, CheckOutcome, CompilerIdentity, Request, decode_check, decode_compatibility,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{
    fs::{self, File},
    io::{self, Read as _},
    path::PathBuf,
    process::Stdio,
};
use tokio::process::Command;
use url::Url;

const RESULT_LIMIT: u64 = 8 * 1024 * 1024;
const ASSETS: [(&str, &str); 4] = [
    ("preload.ts", include_str!("compiler_backend/preload.ts")),
    (
        "patch_plan.js",
        include_str!("compiler_backend/patch_plan.js"),
    ),
    (
        "recovery_runtime.js",
        include_str!("compiler_backend/recovery_runtime.js"),
    ),
    (
        "registry.json",
        include_str!("compiler_backend/registry.json"),
    ),
];

pub(super) struct NativeRun {
    assets: PathBuf,
    assets_guard: Option<tempfile::TempDir>,
    request: PathBuf,
    result: PathBuf,
    request_id: String,
    preload_options: String,
    root: Option<PathBuf>,
    sources: Vec<BackendSource>,
}

impl NativeRun {
    pub(super) fn compatibility() -> io::Result<Self> {
        let run = Self::stage(None, Vec::new())?;
        let request = Request::compatibility(&run.request_id)?;
        serde_json::to_writer(File::create(&run.request)?, &request)?;
        Ok(run)
    }

    pub(super) fn check(root: PathBuf, sources: Vec<BackendSource>) -> io::Result<Self> {
        let run = Self::stage(Some(root), sources)?;
        let root = run
            .root
            .as_deref()
            .ok_or_else(|| io::Error::other("native check has no root"))?;
        let request = Request::recover(&run.request_id, root, &run.sources)?;
        serde_json::to_writer(File::create(&run.request)?, &request)?;
        Ok(run)
    }

    fn stage(root: Option<PathBuf>, sources: Vec<BackendSource>) -> io::Result<Self> {
        // Inline preload imports a percent-encoded file URL. Bun's environment
        // parser ignores quoted paths; keep cwd and compiler lookup unchanged.
        let guard = tempfile::Builder::new().prefix("bend2-native-").tempdir()?;
        let assets = guard.path().to_path_buf();
        for (name, contents) in ASSETS {
            fs::write(assets.join(name), contents)?;
        }
        let preload = Url::from_file_path(assets.join("preload.ts"))
            .map_err(|()| io::Error::other("native preload has no absolute file URL"))?;
        // File URLs contain no raw quotes, backslashes, or control characters.
        let mut bootstrap = String::with_capacity(preload.as_str().len() + 17);
        bootstrap.push_str("await import(\"");
        bootstrap.push_str(preload.as_str());
        bootstrap.push_str("\");");
        let prefix = "--preload data:text/javascript;base64,";
        let encoded_length = base64::encoded_len(bootstrap.len(), true)
            .ok_or_else(|| io::Error::other("native preload URL is too long"))?;
        let mut preload_options = String::with_capacity(prefix.len() + encoded_length);
        preload_options.push_str(prefix);
        STANDARD.encode_string(bootstrap, &mut preload_options);
        let request_id = assets
            .to_str()
            .ok_or_else(|| io::Error::other("native staging path is not UTF-8"))?
            .to_owned();
        Ok(Self {
            request: assets.join("request.json"),
            result: assets.join("result.json"),
            assets,
            preload_options,
            assets_guard: Some(guard),
            request_id,
            root,
            sources,
        })
    }

    pub(super) fn take_assets(&mut self) -> io::Result<tempfile::TempDir> {
        self.assets_guard
            .take()
            .ok_or_else(|| io::Error::other("native assets have already been transferred"))
    }

    pub(super) fn configure(&self, command: &mut Command) {
        for key in [
            "BEND_RECOVERY_REQUEST",
            "BEND_RECOVERY_STAGING",
            "BEND_RECOVERY_RESULT",
            "BEND_RECOVERY_MODE",
            "BEND_RECOVERY_ROOT",
            "BEND_RECOVERY_PATCH_FAULT",
        ] {
            command.env_remove(key);
        }
        command
            .stdin(Stdio::null())
            .env("BUN_OPTIONS", &self.preload_options)
            .env("BEND_RECOVERY_REQUEST", &self.request)
            .env("BEND_RECOVERY_STAGING", &self.assets)
            .env("BEND_RECOVERY_RESULT", &self.result);
    }

    pub(super) fn compatibility_result(
        &self,
        exit_code: Option<i32>,
    ) -> io::Result<Option<CompilerIdentity>> {
        let bytes = match self.read_result() {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound && exit_code == Some(0) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        decode_compatibility(&bytes, &self.request_id, exit_code).map(Some)
    }

    pub(super) fn check_result(
        &self,
        identity: &CompilerIdentity,
        exit_code: Option<i32>,
    ) -> io::Result<CheckOutcome> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| io::Error::other("native check has no root"))?;
        decode_check(
            &self.read_result()?,
            &self.request_id,
            root,
            identity,
            &self.sources,
            exit_code,
        )
    }

    fn read_result(&self) -> io::Result<Vec<u8>> {
        let metadata = fs::symlink_metadata(&self.result)?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::other("native result is not a regular file"));
        }
        if metadata.len() > RESULT_LIMIT {
            return Err(io::Error::other("native result exceeds the 8 MiB limit"));
        }
        let capacity = usize::try_from(metadata.len()).map_err(io::Error::other)?;
        let mut bytes = Vec::with_capacity(capacity);
        let mut file = File::open(&self.result)?;
        (&mut file).take(RESULT_LIMIT).read_to_end(&mut bytes)?;
        let mut extra = [0];
        if file.read(&mut extra)? != 0 {
            return Err(io::Error::other("native result exceeds the 8 MiB limit"));
        }
        Ok(bytes)
    }
}

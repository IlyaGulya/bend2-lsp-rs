use super::{BYTE_LIMIT, CONFIG_NAME, ENTRY_LIMIT, FILE_LIMIT, Identities, RULES_DIRECTORY};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    io::Read,
    path::{Path, PathBuf},
};

// One saved tree walk: file digests, the lexical spellings that reached each
// canonical file or directory, and the paths inside `bend-lint-rules/`.
#[derive(Default)]
pub(super) struct DiskFiles {
    pub(super) identities: Identities,
    pub(super) aliases: BTreeMap<PathBuf, PathBuf>,
    pub(super) rule_paths: HashSet<PathBuf>,
    bytes: usize,
}

impl DiskFiles {
    fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf> {
        let canonical = fs::canonicalize(path)?;
        if let Some(previous) = self.aliases.get(path) {
            if previous != &canonical {
                return Err(io::Error::other(
                    "bend-lint source alias changed during capture",
                ));
            }
        } else if canonical != path {
            self.aliases.insert(path.to_path_buf(), canonical.clone());
        }
        Ok(canonical)
    }

    fn read(&mut self, path: &Path) -> io::Result<PathBuf> {
        let path = self.canonicalize(path)?;
        if self.identities.contains_key(&path) {
            return Ok(path);
        }
        if self.identities.len() >= FILE_LIMIT {
            return Err(io::Error::other("bend-lint snapshot exceeds 4096 files"));
        }
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() {
            return Err(io::Error::other(
                "bend-lint requires regular saved source files",
            ));
        }
        let size = usize::try_from(metadata.len()).map_err(io::Error::other)?;
        if self.bytes.saturating_add(size) > BYTE_LIMIT {
            return Err(io::Error::other("bend-lint snapshot exceeds 32 MiB"));
        }
        let mut reader = fs::File::open(&path)?
            .take(u64::try_from(BYTE_LIMIT - self.bytes + 1).map_err(io::Error::other)?);
        let mut hasher = Sha256::new();
        let mut buffer = [0; 8192];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            self.bytes += count;
            hasher.update(&buffer[..count]);
        }
        if self.bytes > BYTE_LIMIT {
            return Err(io::Error::other("bend-lint snapshot exceeds 32 MiB"));
        }
        self.identities
            .insert(path.clone(), hasher.finalize().into());
        Ok(path)
    }
}

pub(super) fn project_files(root: &Path) -> io::Result<DiskFiles> {
    let mut files = DiskFiles::default();
    let mut pending = vec![(root.to_path_buf(), false)];
    let mut visited = HashSet::new();
    let mut entries = 0;
    while let Some((directory, under_rules)) = pending.pop() {
        let directory = files.canonicalize(&directory)?;
        if !visited.insert((directory.clone(), under_rules)) {
            continue;
        }
        if visited.len() > FILE_LIMIT {
            return Err(io::Error::other(
                "bend-lint snapshot exceeds 4096 directories",
            ));
        }
        for entry in fs::read_dir(&directory)? {
            entries += 1;
            if entries > ENTRY_LIMIT {
                return Err(io::Error::other(
                    "bend-lint snapshot exceeds 32768 directory entries",
                ));
            }
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if matches!(
                name.to_str(),
                Some("node_modules" | ".git" | "target" | ".cache")
            ) {
                continue;
            }
            let metadata = fs::metadata(&path)?;
            if metadata.is_dir() {
                if pending.len() + visited.len() >= FILE_LIMIT {
                    return Err(io::Error::other(
                        "bend-lint snapshot exceeds 4096 directories",
                    ));
                }
                pending.push((path, under_rules || name == RULES_DIRECTORY));
            } else if path
                .extension()
                .is_some_and(|extension| extension == "bend")
                || name == CONFIG_NAME
                || under_rules
            {
                let canonical = files.read(&path)?;
                if under_rules {
                    files.rule_paths.insert(canonical);
                }
            }
        }
    }
    Ok(files)
}

pub(super) fn is_bend(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "bend")
}

pub(super) fn read_text(path: &Path, limit: usize) -> io::Result<String> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "bend-lint requires regular saved source files",
        ));
    }
    let size = usize::try_from(metadata.len()).map_err(io::Error::other)?;
    if size > limit {
        return Err(io::Error::other(
            "bend-lint source text exceeds the 32 MiB snapshot limit",
        ));
    }
    let mut reader =
        fs::File::open(path)?.take(u64::try_from(limit).map_err(io::Error::other)? + 1);
    let mut text = String::with_capacity(size);
    reader.read_to_string(&mut text)?;
    if text.len() != size {
        return Err(io::Error::other(
            "bend-lint source changed while it was captured",
        ));
    }
    Ok(text)
}

pub(super) fn read_source(path: &Path, bytes: &mut usize) -> io::Result<(PathBuf, String)> {
    let canonical = fs::canonicalize(path)?;
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "bend-lint requires regular saved source files",
        ));
    }
    let size = usize::try_from(metadata.len()).map_err(io::Error::other)?;
    let remaining = BYTE_LIMIT.checked_sub(*bytes).ok_or(io::Error::other(
        "bend-lint snapshot exceeds 32 MiB of source text",
    ))?;
    if size > remaining {
        return Err(io::Error::other(
            "bend-lint snapshot exceeds 32 MiB of source text",
        ));
    }
    let text = read_text(&canonical, remaining)?;
    *bytes += text.len();
    Ok((canonical, text))
}

pub(super) fn file_identity(path: &Path) -> io::Result<[u8; 32]> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "bend-lint requires regular saved source files",
        ));
    }
    let size = usize::try_from(metadata.len()).map_err(io::Error::other)?;
    if size > BYTE_LIMIT {
        return Err(io::Error::other("bend-lint snapshot exceeds 32 MiB"));
    }
    let mut reader =
        fs::File::open(path)?.take(u64::try_from(BYTE_LIMIT).map_err(io::Error::other)? + 1);
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().into())
}

pub(super) fn disk_matches(path: &Path, text: &str) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() || usize::try_from(metadata.len()).ok() != Some(text.len()) {
        return false;
    }
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut buffer = [0; 8192];
    for chunk in text.as_bytes().chunks(buffer.len()) {
        if file.read_exact(&mut buffer[..chunk.len()]).is_err() || &buffer[..chunk.len()] != chunk {
            return false;
        }
    }
    file.read(&mut buffer[..1]).is_ok_and(|count| count == 0)
}

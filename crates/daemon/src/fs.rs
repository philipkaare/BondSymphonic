//! Worktree file service: `fs.list_dir` / `fs.read_file` / `fs.write_file`,
//! with strict path containment so a workspace's client can never read or
//! write outside its worktree.

use bondsymphonic_proto::*;
use std::path::{Component, Path, PathBuf};

pub const MAX_READ: usize = 4 * 1024 * 1024;

/// Resolves `rel` against `root`, rejecting absolute paths, `..` components
/// and NUL bytes, then canonicalizing the deepest existing ancestor to
/// defeat symlink escapes (the result must still start with the canonical
/// root).
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, RpcError> {
    if rel.contains('\0') {
        return Err(RpcError::invalid_params("path contains NUL"));
    }
    let p = Path::new(rel);
    if p.is_absolute()
        || rel.starts_with('/')
        || rel.starts_with('\\')
        || p.components()
            .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
    {
        return Err(RpcError::invalid_params(
            "path must be relative to the worktree",
        ));
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(RpcError::invalid_params("path may not contain '..'"));
    }
    let joined = root.join(p);
    let root_c = root.canonicalize().map_err(|e| RpcError::io(&e))?;
    // Canonicalize the deepest existing ancestor to defeat symlink escapes.
    let mut probe = joined.clone();
    let mut tail = Vec::new();
    while !probe.exists() {
        tail.push(
            probe
                .file_name()
                .map(|s| s.to_os_string())
                .unwrap_or_default(),
        );
        if !probe.pop() {
            break;
        }
    }
    let mut canon = probe.canonicalize().map_err(|e| RpcError::io(&e))?;
    for t in tail.into_iter().rev() {
        canon.push(t);
    }
    if !canon.starts_with(&root_c) {
        return Err(RpcError::invalid_params("path escapes the worktree"));
    }
    Ok(canon)
}

/// Lists the directory at `rel`, directories first, then case-insensitive by
/// name. Skips `.git`. `status` is always `Unchanged` at this milestone.
pub fn list_dir(root: &Path, rel: &str) -> Result<ListDirResult, RpcError> {
    let dir = resolve(root, rel)?;
    if !dir.is_dir() {
        return Err(RpcError::invalid_params("not a directory"));
    }
    let mut entries: Vec<FileEntry> = std::fs::read_dir(&dir)
        .map_err(|e| RpcError::io(&e))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                return None;
            }
            let md = e.metadata().ok()?;
            Some(FileEntry {
                name,
                is_dir: md.is_dir(),
                size: if md.is_dir() { 0 } else { md.len() },
                status: FileStatus::Unchanged,
            })
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(ListDirResult { entries })
}

/// Reads the file at `rel`. UTF-8 content above [`MAX_READ`] is truncated at
/// the last valid char boundary; non-UTF-8 content is reported as binary
/// with empty content.
pub fn read_file(root: &Path, rel: &str) -> Result<ReadFileResult, RpcError> {
    let path = resolve(root, rel)?;
    if !path.exists() {
        return Err(RpcError::not_found(rel));
    }
    let bytes = std::fs::read(&path).map_err(|e| RpcError::io(&e))?;
    let truncated = bytes.len() > MAX_READ;
    let slice = if truncated {
        &bytes[..MAX_READ]
    } else {
        &bytes[..]
    };
    match std::str::from_utf8(slice) {
        Ok(s) => Ok(ReadFileResult {
            content: s.to_string(),
            encoding: "utf-8".into(),
            truncated,
        }),
        Err(e)
            if truncated
                && e.valid_up_to() > 0
                && slice[..e.valid_up_to()].len() + 4 >= slice.len() =>
        {
            Ok(ReadFileResult {
                content: std::str::from_utf8(&slice[..e.valid_up_to()])
                    .unwrap()
                    .to_string(),
                encoding: "utf-8".into(),
                truncated: true,
            })
        }
        Err(_) => Ok(ReadFileResult {
            content: String::new(),
            encoding: "binary".into(),
            truncated,
        }),
    }
}

/// Writes `content` to `rel` via a temp file + rename, creating parent
/// directories as needed and preserving the existing file's mode on unix.
pub fn write_file(root: &Path, rel: &str, content: &str) -> Result<Empty, RpcError> {
    let path = resolve(root, rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RpcError::io(&e))?;
    }
    let tmp = path.with_extension(format!(
        "{}.bs-tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp, content).map_err(|e| RpcError::io(&e))?;
    #[cfg(unix)]
    if let Ok(md) = std::fs::metadata(&path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &tmp,
            std::fs::Permissions::from_mode(md.permissions().mode()),
        );
    }
    std::fs::rename(&tmp, &path).map_err(|e| RpcError::io(&e))?;
    Ok(Empty {})
}

//! Lazily-populated cache of directory listings for the file tree view.
//! This module must never import Qt types.

use bondsymphonic_proto::FileEntry;
use std::collections::HashMap;

/// Caches directory listings by their repo-relative path ("" is the root).
pub struct FileTree {
    root_loaded: bool,
    dirs: HashMap<String, Vec<FileEntry>>,
}

impl FileTree {
    pub fn new() -> Self {
        FileTree {
            root_loaded: false,
            dirs: HashMap::new(),
        }
    }

    /// Stores (or replaces) the listing for `path`.
    pub fn set_dir(&mut self, path: &str, entries: Vec<FileEntry>) {
        if path.is_empty() {
            self.root_loaded = true;
        }
        self.dirs.insert(path.to_owned(), entries);
    }

    pub fn get_dir(&self, path: &str) -> Option<&[FileEntry]> {
        self.dirs.get(path).map(Vec::as_slice)
    }

    pub fn is_loaded(&self, path: &str) -> bool {
        if path.is_empty() {
            self.root_loaded
        } else {
            self.dirs.contains_key(path)
        }
    }

    /// Drops the cached listing for `path` and every descendant directory.
    pub fn invalidate(&mut self, path: &str) {
        if path.is_empty() {
            self.root_loaded = false;
        }
        self.dirs
            .retain(|k, _| k != path && !Self::is_descendant(k, path));
    }

    fn is_descendant(candidate: &str, dir: &str) -> bool {
        if dir.is_empty() {
            !candidate.is_empty()
        } else {
            candidate.starts_with(dir) && candidate[dir.len()..].starts_with('/')
        }
    }

    /// Joins a directory path and a child name into a repo-relative path.
    pub fn child_path(dir: &str, name: &str) -> String {
        if dir.is_empty() {
            name.to_owned()
        } else {
            format!("{dir}/{name}")
        }
    }

    pub fn entries_json(entries: &[FileEntry]) -> String {
        serde_json::to_string(entries).unwrap_or_default()
    }
}

impl Default for FileTree {
    fn default() -> Self {
        Self::new()
    }
}

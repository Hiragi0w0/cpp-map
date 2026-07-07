//! On-disk index model (`.ai-context/index.json`).

use crate::parse::Symbol;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const INDEX_DIR: &str = ".ai-context";
pub const INDEX_FILE: &str = "index.json";
pub const INDEX_VERSION: u32 = 4;

#[derive(Serialize, Deserialize, Debug)]
pub struct Index {
    pub version: u32,
    pub generated_at: u64,
    pub project_type: String, // "cppbuilder" | "cpp"
    pub root: String,
    pub project_files: Vec<String>,
    /// project-relative path (forward slashes) -> entry
    pub files: BTreeMap<String, FileEntry>,
    pub ignored_count: u32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct FileEntry {
    pub kind: String, // cpp | header | cbproj
    pub role: String,
    /// includes resolved to files inside the project (relative paths)
    pub includes: Vec<String>,
    /// includes that could not be resolved inside the project (as written)
    pub external_includes: Vec<String>,
    pub symbols: Vec<Symbol>,
    // --- raw facts kept so incremental refresh can re-derive roles/includes
    //     without re-parsing unchanged files ---
    pub raw_includes: Vec<String>,
    pub has_dfm_pragma: bool,
    pub has_vcl_app_init: bool,
    pub mtime_ms: u64,
    pub size: u64,
}

pub fn index_path(project: &Path) -> PathBuf {
    project.join(INDEX_DIR).join(INDEX_FILE)
}

pub fn save(project: &Path, index: &Index) -> Result<PathBuf, String> {
    let dir = project.join(INDEX_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = index_path(project);
    let json = serde_json::to_string(index).map_err(|e| e.to_string())?;
    fs::write(&path, json).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

pub fn load(project: &Path) -> Result<Index, String> {
    let path = index_path(project);
    let data = fs::read_to_string(&path).map_err(|_| {
        format!(
            "index_not_found: run `cpp-map scan {}` first",
            project.display()
        )
    })?;
    let index: Index =
        serde_json::from_str(&data).map_err(|e| format!("index_corrupt: {e} (re-run scan)"))?;
    if index.version != INDEX_VERSION {
        return Err(format!(
            "index_stale: index was built by another tool version (v{}); run `cpp-map scan {}`",
            index.version,
            project.display()
        ));
    }
    Ok(index)
}

/// Same-stem counterpart(s): MainForm.cpp <-> MainForm.h. Same directory wins;
/// otherwise any file elsewhere with the same stem and the opposite kind.
pub fn same_stem_counterparts(index: &Index, rel: &str) -> Vec<String> {
    let entry = match index.files.get(rel) {
        Some(e) => e,
        None => return Vec::new(),
    };
    let want = match entry.kind.as_str() {
        "cpp" => "header",
        "header" => "cpp",
        _ => return Vec::new(),
    };
    let (dir, stem) = split_dir_stem(rel);
    let mut same_dir = Vec::new();
    let mut other_dir = Vec::new();
    for (path, e) in &index.files {
        if path == rel || e.kind != want {
            continue;
        }
        let (d, s) = split_dir_stem(path);
        if !s.eq_ignore_ascii_case(&stem) {
            continue;
        }
        if d == dir {
            same_dir.push(path.clone());
        } else {
            other_dir.push(path.clone());
        }
    }
    if !same_dir.is_empty() {
        same_dir
    } else {
        other_dir
    }
}

pub fn split_dir_stem(rel: &str) -> (String, String) {
    let (dir, file) = match rel.rfind('/') {
        Some(p) => (&rel[..p], &rel[p + 1..]),
        None => ("", rel),
    };
    let stem = match file.rfind('.') {
        Some(p) => &file[..p],
        None => file,
    };
    (dir.to_string(), stem.to_string())
}

/// Resolve a user-supplied `--file` argument against the index:
/// exact relative path (either slash style) or unique basename match.
pub fn resolve_file_arg(index: &Index, arg: &str) -> Result<String, String> {
    let norm = arg.replace('\\', "/");
    let norm = norm.trim_start_matches("./").to_string();
    if index.files.contains_key(&norm) {
        return Ok(norm);
    }
    // case-insensitive exact path
    if let Some(k) = index.files.keys().find(|k| k.eq_ignore_ascii_case(&norm)) {
        return Ok(k.clone());
    }
    // basename match
    let base = norm.rsplit('/').next().unwrap_or(&norm).to_lowercase();
    let hits: Vec<&String> = index
        .files
        .keys()
        .filter(|k| k.rsplit('/').next().unwrap_or(k).to_lowercase() == base)
        .collect();
    match hits.len() {
        0 => Err(format!("file_not_in_index: {arg}")),
        1 => Ok(hits[0].clone()),
        _ => Err(format!(
            "ambiguous_file: {arg} matches [{}]",
            hits.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

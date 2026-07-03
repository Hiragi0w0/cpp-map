//! Project walker, index builder, and mtime-based incremental refresh.

use crate::index::{FileEntry, INDEX_VERSION, Index, same_stem_counterparts, split_dir_stem};
use crate::parse::parse_source;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".vs",
    "__history",
    "debug",
    "release",
    "win32",
    "win64",
    "build",
    "dist",
    "backup",
    "tmp",
    "temp",
    ".ai-context",
];

fn kind_of(ext: &str) -> Option<&'static str> {
    match ext {
        "cpp" | "cc" | "cxx" => Some("cpp"),
        "h" | "hpp" => Some("header"),
        "cbproj" => Some("cbproj"),
        _ => None,
    }
}

struct WalkedFile {
    rel: String,
    kind: &'static str,
    mtime_ms: u64,
    size: u64,
}

pub struct ScanResult {
    pub index: Index,
    pub counts: BTreeMap<String, u32>,
}

/// Full scan: parse every target file and build a fresh index.
pub fn scan(project: &Path) -> Result<ScanResult, String> {
    let (walked, ignored) = walk_project(project)?;
    let mut files: BTreeMap<String, FileEntry> = BTreeMap::new();
    for w in &walked {
        files.insert(w.rel.clone(), parse_entry(project, w)?);
    }
    finalize(&mut files);
    let index = build_index(project, files, ignored);
    let counts = count_summary(&index);
    Ok(ScanResult { index, counts })
}

/// Load the index and bring it up to date by re-parsing only files whose
/// mtime/size changed (plus additions/removals). A stale-version or corrupt
/// index is rebuilt from scratch; a missing index stays an error so the very
/// first `scan` remains explicit.
pub fn ensure_fresh(project: &Path) -> Result<Index, String> {
    let mut index = match crate::index::load(project) {
        Ok(i) => i,
        Err(e) if e.starts_with("index_stale") || e.starts_with("index_corrupt") => {
            let result = scan(project)?;
            crate::index::save(project, &result.index)?;
            return Ok(result.index);
        }
        Err(e) => return Err(e),
    };

    let (walked, ignored) = walk_project(project)?;
    let mut changed = false;

    let current: BTreeMap<&str, &WalkedFile> = walked.iter().map(|w| (w.rel.as_str(), w)).collect();
    let removed: Vec<String> = index
        .files
        .keys()
        .filter(|k| !current.contains_key(k.as_str()))
        .cloned()
        .collect();
    for k in removed {
        index.files.remove(&k);
        changed = true;
    }
    for w in &walked {
        let fresh = index
            .files
            .get(&w.rel)
            .is_some_and(|e| e.mtime_ms == w.mtime_ms && e.size == w.size);
        if !fresh {
            index.files.insert(w.rel.clone(), parse_entry(project, w)?);
            changed = true;
        }
    }
    if index.ignored_count != ignored {
        index.ignored_count = ignored;
        changed = true;
    }

    if changed {
        finalize(&mut index.files);
        index.project_files = index
            .files
            .iter()
            .filter(|(_, e)| e.kind == "cbproj")
            .map(|(k, _)| k.clone())
            .collect();
        index.project_type = if index.project_files.is_empty() {
            "cpp"
        } else {
            "cppbuilder"
        }
        .into();
        index.generated_at = now_secs();
        crate::index::save(project, &index)?;
    }
    Ok(index)
}

fn build_index(project: &Path, files: BTreeMap<String, FileEntry>, ignored: u32) -> Index {
    let project_files: Vec<String> = files
        .iter()
        .filter(|(_, e)| e.kind == "cbproj")
        .map(|(k, _)| k.clone())
        .collect();
    Index {
        version: INDEX_VERSION,
        generated_at: now_secs(),
        project_type: if project_files.is_empty() {
            "cpp"
        } else {
            "cppbuilder"
        }
        .into(),
        root: project.to_string_lossy().replace('\\', "/"),
        project_files,
        files,
        ignored_count: ignored,
    }
}

fn count_summary(index: &Index) -> BTreeMap<String, u32> {
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for e in index.files.values() {
        *counts.entry(e.kind.clone()).or_insert(0) += 1;
    }
    counts.entry("cpp".into()).or_insert(0);
    counts.entry("header".into()).or_insert(0);
    counts.entry("cbproj".into()).or_insert(0);
    counts.insert("ignored".into(), index.ignored_count);
    counts
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse one file into an entry holding its raw facts. Derived data
/// (resolved includes, role) is filled in by `finalize`.
fn parse_entry(project: &Path, w: &WalkedFile) -> Result<FileEntry, String> {
    let mut entry = FileEntry {
        kind: w.kind.into(),
        role: if w.kind == "cbproj" {
            "configuration"
        } else {
            "unknown"
        }
        .into(),
        includes: Vec::new(),
        external_includes: Vec::new(),
        symbols: Vec::new(),
        raw_includes: Vec::new(),
        has_dfm_pragma: false,
        has_vcl_app_init: false,
        mtime_ms: w.mtime_ms,
        size: w.size,
    };
    if w.kind != "cbproj" {
        let abs = project.join(w.rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        let text = read_text(&abs)?;
        let parsed = parse_source(&text);
        entry.symbols = parsed.symbols;
        entry.raw_includes = parsed.includes;
        entry.has_dfm_pragma = parsed.has_dfm_pragma;
        entry.has_vcl_app_init = parsed.has_vcl_app_init;
    }
    Ok(entry)
}

/// Derive resolved includes and roles for the whole file set (cheap,
/// in-memory; runs after any change to any file).
fn finalize(entries: &mut BTreeMap<String, FileEntry>) {
    resolve_includes(entries);
    assign_roles(entries);
}

fn walk_project(project: &Path) -> Result<(Vec<WalkedFile>, u32), String> {
    if !project.is_dir() {
        return Err(format!("not_a_directory: {}", project.display()));
    }
    let mut files: Vec<WalkedFile> = Vec::new();
    let mut ignored: u32 = 0;
    walk(project, project, &mut files, &mut ignored)?;
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok((files, ignored))
}

fn walk(
    root: &Path,
    dir: &Path,
    files: &mut Vec<WalkedFile>,
    ignored: &mut u32,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if EXCLUDED_DIRS.iter().any(|d| name.eq_ignore_ascii_case(d)) {
                continue;
            }
            walk(root, &path, files, ignored)?;
        } else {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            match kind_of(&ext) {
                Some(kind) => {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let meta = entry
                        .metadata()
                        .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
                    let mtime_ms = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    files.push(WalkedFile {
                        rel,
                        kind,
                        mtime_ms,
                        size: meta.len(),
                    });
                }
                None => *ignored += 1, // .dfm, build artifacts, everything else
            }
        }
    }
    Ok(())
}

/// UTF-8 (with/without BOM) -> UTF-16 BOM -> Shift_JIS (cp932) -> lossy UTF-8.
pub fn read_text(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok(String::from_utf8_lossy(&bytes[3..]).into_owned());
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let (s, _, _) = encoding_rs::UTF_16LE.decode(&bytes[2..]);
        return Ok(s.into_owned());
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let (s, _, _) = encoding_rs::UTF_16BE.decode(&bytes[2..]);
        return Ok(s.into_owned());
    }
    match String::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(e) => {
            let bytes = e.into_bytes();
            let (s, _, had_errors) = encoding_rs::SHIFT_JIS.decode(&bytes);
            if had_errors {
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            } else {
                Ok(s.into_owned())
            }
        }
    }
}

fn resolve_includes(entries: &mut BTreeMap<String, FileEntry>) {
    // lower-case basename -> candidate relative paths
    let mut by_base: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for rel in entries.keys() {
        let base = rel.rsplit('/').next().unwrap_or(rel).to_lowercase();
        by_base.entry(base).or_default().push(rel.clone());
    }
    let all_paths_lower: BTreeMap<String, String> = entries
        .keys()
        .map(|k| (k.to_lowercase(), k.clone()))
        .collect();

    let mut resolutions: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
    for (rel, entry) in entries.iter() {
        let dir = split_dir_stem(rel).0;
        let mut resolved = Vec::new();
        let mut external = Vec::new();
        for raw in &entry.raw_includes {
            let norm = raw.replace('\\', "/");
            // 1) exact path relative to the includer's directory, then to the root
            let joined = if dir.is_empty() {
                norm.clone()
            } else {
                format!("{dir}/{norm}")
            };
            let hit = all_paths_lower
                .get(&joined.to_lowercase())
                .or_else(|| all_paths_lower.get(&norm.to_lowercase()));
            if let Some(h) = hit {
                resolved.push(h.clone());
                continue;
            }
            // 2) basename match: prefer the includer's own directory
            let base = norm.rsplit('/').next().unwrap_or(&norm).to_lowercase();
            match by_base.get(&base) {
                Some(cands) => {
                    let same_dir = cands.iter().find(|c| split_dir_stem(c).0 == dir);
                    resolved.push(same_dir.unwrap_or(&cands[0]).clone());
                }
                None => external.push(raw.clone()),
            }
        }
        resolved.dedup();
        resolutions.insert(rel.clone(), (resolved, external));
    }
    for (rel, (resolved, external)) in resolutions {
        if let Some(e) = entries.get_mut(&rel) {
            e.includes = resolved;
            e.external_includes = external;
        }
    }
}

fn path_has_keyword(rel_lower: &str, stem_lower: &str, keywords: &[&str]) -> bool {
    let dirs: Vec<&str> = rel_lower.split('/').collect();
    let dir_parts = &dirs[..dirs.len().saturating_sub(1)];
    keywords
        .iter()
        .any(|k| stem_lower.contains(k) || dir_parts.iter().any(|d| d.contains(k)))
}

fn assign_roles(entries: &mut BTreeMap<String, FileEntry>) {
    let mut roles: BTreeMap<String, String> = BTreeMap::new();
    for (rel, entry) in entries.iter() {
        if entry.kind == "cbproj" {
            continue;
        }
        roles.insert(rel.clone(), detect_role(rel, entry));
    }
    // headers/impls inherit their counterpart's role when their own is unknown
    let unknowns: Vec<String> = roles
        .iter()
        .filter(|(_, r)| *r == "unknown")
        .map(|(k, _)| k.clone())
        .collect();
    let view = Index {
        version: 0,
        generated_at: 0,
        project_type: String::new(),
        root: String::new(),
        project_files: Vec::new(),
        files: std::mem::take(entries),
        ignored_count: 0,
    };
    for rel in unknowns {
        for other in same_stem_counterparts(&view, &rel) {
            if let Some(r) = roles.get(&other)
                && r != "unknown"
            {
                roles.insert(rel.clone(), r.clone());
                break;
            }
        }
    }
    *entries = view.files;
    for (rel, role) in roles {
        if let Some(e) = entries.get_mut(&rel) {
            e.role = role;
        }
    }
}

fn detect_role(rel: &str, entry: &FileEntry) -> String {
    let rel_lower = rel.to_lowercase();
    let stem_lower = split_dir_stem(rel).1.to_lowercase();

    if entry.kind == "cpp"
        && (entry.has_vcl_app_init
            || entry.symbols.iter().any(|s| {
                s.kind == "function"
                    && matches!(s.name.as_str(), "main" | "WinMain" | "_tmain" | "wmain")
            }))
    {
        return "entry_point".into();
    }
    if entry.has_dfm_pragma {
        return "form_or_dialog_logic".into();
    }
    let vcl_ui_base = entry.symbols.iter().any(|s| {
        s.base_candidates.as_ref().is_some_and(|bases| {
            bases.iter().any(|b| {
                let last = b.rsplit("::").next().unwrap_or(b);
                last.starts_with('T')
                    && (last.contains("Form")
                        || last.contains("Frame")
                        || last.contains("DataModule")
                        || last.contains("Dialog"))
            })
        })
    });
    if vcl_ui_base || path_has_keyword(&rel_lower, &stem_lower, &["form", "dialog", "dlg", "frame"])
    {
        return "form_or_dialog_logic".into();
    }
    if path_has_keyword(&rel_lower, &stem_lower, &["repositor", "dao"]) {
        return "repository".into();
    }
    if path_has_keyword(
        &rel_lower,
        &stem_lower,
        &["model", "entity", "entities", "domain"],
    ) {
        return "model".into();
    }
    if path_has_keyword(&rel_lower, &stem_lower, &["config", "setting"]) {
        return "configuration".into();
    }
    if path_has_keyword(
        &rel_lower,
        &stem_lower,
        &["util", "helper", "common", "tool"],
    ) {
        return "utility".into();
    }
    if path_has_keyword(&rel_lower, &stem_lower, &["resource"]) {
        return "resource".into();
    }
    "unknown".into()
}

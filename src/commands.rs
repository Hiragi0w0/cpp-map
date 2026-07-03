//! Implementations of the query subcommands. Each returns a serde_json::Value.

use crate::index::{Index, resolve_file_arg, same_stem_counterparts, split_dir_stem};
use crate::parse::strip_comments_and_strings;
use crate::scan;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub fn cmd_scan(project: &Path) -> Result<Value, String> {
    let result = scan::scan(project)?;
    let path = crate::index::save(project, &result.index)?;
    let rel_index = path
        .strip_prefix(project)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"));
    Ok(json!({
        "status": "ok",
        "project_type": result.index.project_type,
        "index_path": rel_index,
        "file_count": result.counts,
    }))
}

pub fn cmd_overview(project: &Path) -> Result<Value, String> {
    let index = scan::ensure_fresh(project)?;

    let mut entry_candidates: Vec<String> = index
        .files
        .iter()
        .filter(|(_, e)| e.role == "entry_point" && e.kind == "cpp")
        .map(|(k, _)| k.clone())
        .collect();
    // name-pattern fallbacks: main.cpp / Project*.cpp / <cbproj-stem>.cpp
    let cbproj_stems: Vec<String> = index
        .project_files
        .iter()
        .map(|p| split_dir_stem(p).1.to_lowercase())
        .collect();
    for (path, e) in &index.files {
        if e.kind != "cpp" {
            continue;
        }
        let stem = split_dir_stem(path).1.to_lowercase();
        if (stem == "main" || stem.starts_with("project") || cbproj_stems.contains(&stem))
            && !entry_candidates.contains(path)
        {
            entry_candidates.push(path.clone());
        }
    }

    // directories ranked by number of source files
    let mut dir_counts: BTreeMap<String, u32> = BTreeMap::new();
    for (path, e) in &index.files {
        if e.kind == "cbproj" {
            continue;
        }
        let dir = split_dir_stem(path).0;
        let dir = if dir.is_empty() { ".".to_string() } else { dir };
        *dir_counts.entry(dir).or_insert(0) += 1;
    }
    let mut dirs: Vec<(String, u32)> = dir_counts.into_iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let important_directories: Vec<String> = dirs.into_iter().take(8).map(|(d, _)| d).collect();

    Ok(json!({
        "project_type": index.project_type,
        "project_files": index.project_files,
        "entry_candidates": entry_candidates,
        "important_directories": important_directories,
        "file_count": {
            "cpp": index.files.values().filter(|e| e.kind == "cpp").count(),
            "header": index.files.values().filter(|e| e.kind == "header").count(),
            "cbproj": index.files.values().filter(|e| e.kind == "cbproj").count(),
        },
        "suggested_first_commands": [
            "cpp-map files . --role entry_point",
            "cpp-map focus . <feature-keyword>",
        ],
    }))
}

fn file_summary(index: &Index, rel: &str) -> Value {
    let e = &index.files[rel];
    json!({
        "path": rel,
        "kind": e.kind,
        "role": e.role,
        "related": same_stem_counterparts(index, rel),
        "include_count": e.includes.len() + e.external_includes.len(),
        "symbol_count": e.symbols.len(),
    })
}

pub fn cmd_files(
    project: &Path,
    role: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<Value>, String> {
    let index = scan::ensure_fresh(project)?;
    let mut out: Vec<Value> = index
        .files
        .keys()
        .filter(|k| role.is_none_or(|r| index.files[*k].role == r))
        .map(|k| file_summary(&index, k))
        .collect();
    if let Some(n) = limit {
        out.truncate(n);
    }
    Ok(out)
}

pub fn cmd_symbols(project: &Path, file: &str) -> Result<Value, String> {
    let index = scan::ensure_fresh(project)?;
    let rel = resolve_file_arg(&index, file)?;
    Ok(json!({
        "file": rel,
        "symbols": index.files[&rel].symbols,
    }))
}

pub fn cmd_includes(project: &Path, file: &str) -> Result<Value, String> {
    let index = scan::ensure_fresh(project)?;
    let rel = resolve_file_arg(&index, file)?;
    let e = &index.files[&rel];
    let included_by: Vec<&String> = index
        .files
        .iter()
        .filter(|(k, other)| **k != rel && other.includes.contains(&rel))
        .map(|(k, _)| k)
        .collect();
    Ok(json!({
        "file": rel,
        "includes": e.includes,
        "external_includes": e.external_includes,
        "included_by": included_by,
    }))
}

pub fn cmd_related(project: &Path, file: &str) -> Result<Value, String> {
    let index = scan::ensure_fresh(project)?;
    let rel = resolve_file_arg(&index, file)?;
    let entry = &index.files[&rel];

    // path -> (score, reason); keep the strongest reason per file
    let mut related: BTreeMap<String, (u32, &'static str)> = BTreeMap::new();
    let add = |map: &mut BTreeMap<String, (u32, &'static str)>,
               p: String,
               score: u32,
               reason: &'static str| {
        if p == rel {
            return;
        }
        let cur = map.get(&p).map(|(s, _)| *s).unwrap_or(0);
        if score > cur {
            map.insert(p, (score, reason));
        }
    };

    for c in same_stem_counterparts(&index, &rel) {
        let reason = if index.files[&c].kind == "header" {
            "same_stem_header"
        } else {
            "same_stem_impl"
        };
        add(&mut related, c, 100, reason);
    }
    for inc in &entry.includes {
        add(&mut related, inc.clone(), 60, "included_file");
        for impl_file in same_stem_counterparts(&index, inc) {
            add(
                &mut related,
                impl_file,
                45,
                "implementation_of_included_header",
            );
        }
    }
    for (path, other) in &index.files {
        if other.includes.contains(&rel) {
            add(&mut related, path.clone(), 40, "includes_this_file");
        }
    }

    let mut list: Vec<(String, u32, &'static str)> =
        related.into_iter().map(|(p, (s, r))| (p, s, r)).collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok(json!({
        "file": rel,
        "related_files": list
            .into_iter()
            .map(|(p, s, r)| json!({ "path": p, "reason": r, "score": s }))
            .collect::<Vec<_>>(),
    }))
}

pub fn cmd_snippet(
    project: &Path,
    symbol: &str,
    file: Option<&str>,
    owner: Option<&str>,
    context: usize,
    max_lines: usize,
) -> Result<Value, String> {
    let index = scan::ensure_fresh(project)?;
    let scope: Vec<String> = match file {
        Some(f) => vec![resolve_file_arg(&index, f)?],
        None => index.files.keys().cloned().collect(),
    };

    // gather candidates; exact-case matches beat case-insensitive ones
    let mut candidates: Vec<(&str, &crate::parse::Symbol)> = Vec::new();
    for rel in &scope {
        for s in &index.files[rel].symbols {
            if s.name != symbol && !s.name.eq_ignore_ascii_case(symbol) {
                continue;
            }
            if owner.is_some_and(|o| s.owner.as_deref() != Some(o)) {
                continue;
            }
            candidates.push((rel.as_str(), s));
        }
    }
    if candidates.iter().any(|(_, s)| s.name == symbol) {
        candidates.retain(|(_, s)| s.name == symbol);
    }
    if candidates.is_empty() {
        return Err(format!(
            "symbol_not_found: {symbol}{} (hint: try `focus` to locate the feature first)",
            file.map(|f| format!(" in {f}")).unwrap_or_default()
        ));
    }

    // prefer the widest extent: a definition body beats a one-line declaration
    let span = |s: &crate::parse::Symbol| s.end_line.unwrap_or(s.line).saturating_sub(s.line);
    candidates.sort_by(|a, b| span(b.1).cmp(&span(a.1)).then(a.0.cmp(b.0)));
    let location = |(rel, s): &(&str, &crate::parse::Symbol)| {
        json!({
            "file": rel, "line": s.line, "kind": s.kind,
            "owner": s.owner, "end_line": s.end_line,
        })
    };
    if candidates.len() > 1 && span(candidates[0].1) == span(candidates[1].1) {
        return Err(format!(
            "ambiguous_symbol: {symbol} — disambiguate with --file/--owner; candidates: {}",
            serde_json::to_string(&candidates.iter().map(location).collect::<Vec<_>>()).unwrap()
        ));
    }
    let (rel, sym) = candidates[0];
    let other_locations: Vec<Value> = candidates.iter().skip(1).take(10).map(location).collect();

    // slice the live file using the indexed line numbers, guarding staleness
    let abs = project.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    let text = scan::read_text(&abs)?;
    let lines: Vec<&str> = text.lines().collect();
    let start = sym.line as usize;
    let end = sym.end_line.unwrap_or(sym.line) as usize;
    let bare = symbol.trim_start_matches('~');
    let anchor_ok = start >= 1
        && end <= lines.len()
        && lines[start - 1..end.min(start + 1)]
            .iter()
            .any(|l| l.contains(bare));
    if !anchor_ok {
        return Err(format!(
            "index_stale: {rel} changed since the last scan; run `cpp-map scan {}`",
            project.display()
        ));
    }

    let from = start.saturating_sub(context).max(1);
    let mut to = (end + context).min(lines.len());
    let truncated = to - from + 1 > max_lines;
    if truncated {
        to = from + max_lines - 1;
    }
    let mut out = json!({
        "file": rel,
        "symbol": sym.name,
        "kind": sym.kind,
        "owner": sym.owner,
        "start_line": from,
        "end_line": to,
        "truncated": truncated,
        "text": lines[from - 1..to].join("\n"),
    });
    if !other_locations.is_empty() {
        out["other_locations"] = json!(other_locations);
    }
    Ok(out)
}

pub fn cmd_refs(project: &Path, symbol: &str, limit: usize) -> Result<Value, String> {
    let sym = symbol.trim();
    if sym.is_empty() || !sym.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "invalid_symbol: expected a C++ identifier, got {symbol:?}"
        ));
    }
    let index = scan::ensure_fresh(project)?;
    let word = Regex::new(&format!(r"\b{}\b", regex::escape(sym))).map_err(|e| e.to_string())?;

    // definitions/declarations known to the index; their lines are excluded
    // from the reference list so call sites stay separate. The list is capped
    // by `limit` too — common names can have hundreds of definitions.
    let mut definitions: Vec<Value> = Vec::new();
    let mut definition_count: usize = 0;
    let mut def_lines: BTreeMap<&str, BTreeSet<u32>> = BTreeMap::new();
    for (rel, entry) in &index.files {
        for s in &entry.symbols {
            if s.name == sym {
                definition_count += 1;
                if definitions.len() < limit {
                    definitions.push(json!({
                        "file": rel, "line": s.line, "kind": s.kind, "owner": s.owner,
                    }));
                }
                def_lines.entry(rel.as_str()).or_default().insert(s.line);
            }
        }
    }

    // word-boundary search over comment/string-stripped code, so hits are
    // actual code references; line text for display comes from the raw source
    let mut references: Vec<Value> = Vec::new();
    let mut reference_count: usize = 0;
    for (rel, entry) in &index.files {
        if entry.kind == "cbproj" {
            continue;
        }
        let abs = project.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Ok(raw) = scan::read_text(&abs) else {
            continue;
        };
        let stripped = strip_comments_and_strings(&raw);
        let raw_lines: Vec<&str> = raw.lines().collect();
        for (i, line) in stripped.lines().enumerate() {
            if !word.is_match(line) {
                continue;
            }
            let lineno = (i + 1) as u32;
            if def_lines
                .get(rel.as_str())
                .is_some_and(|s| s.contains(&lineno))
            {
                continue;
            }
            reference_count += 1;
            if references.len() < limit {
                let text: String = raw_lines
                    .get(i)
                    .map(|l| l.trim().chars().take(160).collect())
                    .unwrap_or_default();
                references.push(json!({ "file": rel, "line": lineno, "text": text }));
            }
        }
    }

    Ok(json!({
        "symbol": sym,
        "definitions": definitions,
        "definition_count": definition_count,
        "references": references,
        "reference_count": reference_count,
        "truncated": reference_count > references.len() || definition_count > definitions.len(),
    }))
}

struct FocusHit {
    score: u32,
    reasons: Vec<&'static str>,
    matched_symbols: Vec<Value>,
}

pub fn cmd_focus(project: &Path, keyword: &str, limit: usize) -> Result<Value, String> {
    let index = scan::ensure_fresh(project)?;
    let kw = keyword.to_lowercase();
    if kw.trim().is_empty() {
        return Err("empty_keyword".into());
    }

    let mut hits: BTreeMap<String, FocusHit> = BTreeMap::new();
    for (rel, entry) in &index.files {
        let mut score = 0u32;
        let mut reasons: Vec<&'static str> = Vec::new();
        let (dir, stem) = split_dir_stem(rel);
        let filename = rel.rsplit('/').next().unwrap_or(rel).to_lowercase();

        if filename.contains(&kw) || stem.to_lowercase().contains(&kw) {
            score += 50;
            reasons.push("filename_match");
        }
        if dir.to_lowercase().contains(&kw) {
            score += 30;
            reasons.push("path_match");
        }

        let matched_symbols: Vec<Value> = entry
            .symbols
            .iter()
            .filter(|s| s.name.to_lowercase().contains(&kw))
            .map(|s| json!({ "name": s.name, "kind": s.kind, "line": s.line }))
            .collect();
        if !matched_symbols.is_empty() {
            score += 40;
            reasons.push("symbol_match");
        }

        if entry
            .includes
            .iter()
            .chain(entry.external_includes.iter())
            .any(|i| i.to_lowercase().contains(&kw))
        {
            score += 20;
            reasons.push("include_match");
        }

        // content match: read the file live (the index stores no source text)
        if entry.kind != "cbproj" {
            let abs = project.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Ok(text) = scan::read_text(&abs) {
                let occurrences = text.to_lowercase().matches(&kw).count();
                if occurrences > 0 {
                    // +10 for the first hit, +2 per extra occurrence, capped at +30
                    score += 10 + ((occurrences as u32 - 1) * 2).min(30);
                    reasons.push("content_match");
                }
            }
        }

        if score > 0 {
            hits.insert(
                rel.clone(),
                FocusHit {
                    score,
                    reasons,
                    matched_symbols,
                },
            );
        }
    }

    // related header/impl bonus: counterparts of matched files get +15
    let matched_paths: Vec<String> = hits.keys().cloned().collect();
    for path in &matched_paths {
        for c in same_stem_counterparts(&index, path) {
            let reason = if index.files[&c].kind == "header" {
                "related_header"
            } else {
                "related_impl"
            };
            let h = hits.entry(c).or_insert(FocusHit {
                score: 0,
                reasons: Vec::new(),
                matched_symbols: Vec::new(),
            });
            if !h.reasons.contains(&reason) {
                h.score += 15;
                h.reasons.push(reason);
            }
        }
    }

    let mut ranked: Vec<(String, FocusHit)> = hits.into_iter().collect();
    ranked.sort_by(|a, b| b.1.score.cmp(&a.1.score).then(a.0.cmp(&b.0)));
    let matched_files = ranked.len();
    ranked.truncate(limit);

    // reading order: score-descending, but list a pair's header before its impl
    let in_result: Vec<String> = ranked.iter().map(|(p, _)| p.clone()).collect();
    let mut reading_order: Vec<String> = Vec::new();
    for (path, _) in &ranked {
        if reading_order.contains(path) {
            continue;
        }
        if index.files[path].kind == "cpp" {
            for c in same_stem_counterparts(&index, path) {
                if in_result.contains(&c) && !reading_order.contains(&c) {
                    reading_order.push(c);
                }
            }
        }
        reading_order.push(path.clone());
    }

    Ok(json!({
        "query": keyword,
        "matched_files": matched_files,
        "suggested_reading_order": reading_order,
        "files": ranked
            .into_iter()
            .map(|(path, h)| {
                json!({
                    "path": path,
                    "score": h.score,
                    "reasons": h.reasons,
                    "matched_symbols": h.matched_symbols,
                    "related": same_stem_counterparts(&index, &path),
                })
            })
            .collect::<Vec<_>>(),
    }))
}

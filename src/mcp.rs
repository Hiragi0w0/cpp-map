//! MCP (Model Context Protocol) server over stdio.
//!
//! Newline-delimited JSON-RPC 2.0. Only the surface needed for a tools-only
//! server is implemented: initialize / ping / tools/list / tools/call.
//! Every tool reuses the same core functions as the CLI subcommands.

use crate::commands;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Context-compression queries for C++Builder projects. \
Typical flow: overview -> focus <feature keyword> -> related/symbols on the candidate \
files -> snippet to read just the relevant function bodies instead of whole files, and \
refs to find call sites before changing a function. Output is always compact JSON. \
After the first index exists, edited/added/removed files are re-parsed automatically on \
every query. Run `scan` first, or start the server with `--allow-auto-scan` to let \
queries create or rebuild the index automatically.";

pub fn serve(default_project: Option<PathBuf>, allow_auto_scan: bool) -> Result<(), String> {
    // Canonicalize once at startup: a pinned root should be an unambiguous,
    // existing directory, not a relative path re-resolved on every call.
    let default_project = match default_project {
        Some(p) => {
            let canonical = p
                .canonicalize()
                .map_err(|e| format!("invalid project root {}: {e}", p.display()))?;
            if !canonical.is_dir() {
                return Err(format!(
                    "project root is not a directory: {}",
                    canonical.display()
                ));
            }
            Some(canonical)
        }
        None => None,
    };

    let stdin = io::stdin();
    let mut out = io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("stdin read error: {e}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // no id to respond to
        };
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        let reply: Option<Result<Value, Value>> = match method {
            "initialize" => Some(Ok(initialize_result(&params))),
            "ping" => Some(Ok(json!({}))),
            "tools/list" => Some(Ok(json!({ "tools": tool_defs(default_project.is_some()) }))),
            "tools/call" => Some(Ok(handle_tool_call(
                &params,
                default_project.as_deref(),
                allow_auto_scan,
            ))),
            m if m.starts_with("notifications/") => None,
            _ => {
                if id.is_some() {
                    Some(Err(json!({
                        "code": -32601,
                        "message": format!("method not found: {method}"),
                    })))
                } else {
                    None
                }
            }
        };

        if let (Some(reply), Some(id)) = (reply, id) {
            let envelope = match reply {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
            };
            writeln!(out, "{envelope}").map_err(|e| format!("stdout write error: {e}"))?;
            out.flush()
                .map_err(|e| format!("stdout flush error: {e}"))?;
        }
    }
    Ok(())
}

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let version = if SUPPORTED_VERSIONS.contains(&requested) {
        requested
    } else {
        SUPPORTED_VERSIONS[0]
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "cpp-map",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn handle_tool_call(
    params: &Value,
    default_project: Option<&Path>,
    allow_auto_scan: bool,
) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match run_tool(name, &args, default_project, allow_auto_scan) {
        Ok(v) => json!({
            "content": [{ "type": "text", "text": v.to_string() }],
        }),
        Err(msg) => json!({
            "content": [{
                "type": "text",
                "text": json!({ "status": "error", "error": msg }).to_string(),
            }],
            "isError": true,
        }),
    }
}

/// Resolve the effective project root for one tool call.
///
/// - When the server was started with a pinned root (`default_project`),
///   that root is authoritative. A caller-supplied `project_path` is
///   rejected outright rather than silently overriding the pin: an AI
///   agent (or a compromised/confused one) must not be able to redirect
///   scans and index writes to an arbitrary directory just by passing an
///   argument.
/// - When the server was started unpinned, the caller-supplied path is
///   required, canonicalized (resolves `..`, symlinks, relative paths),
///   checked to exist as a directory, and rejected if it looks like a
///   filesystem root or the user's home directory rather than a project.
fn resolve_project(args: &Value, default_project: Option<&Path>) -> Result<PathBuf, String> {
    let requested = args.get("project_path").and_then(|v| v.as_str());
    match (default_project, requested) {
        (Some(_), Some(p)) => Err(format!(
            "project_path ('{p}') is fixed at server startup (pinned root); \
             per-call override is not allowed. Restart the server without a \
             pinned root if you need to target a different project."
        )),
        (Some(root), None) => Ok(root.to_path_buf()),
        (None, Some(p)) => {
            let raw = PathBuf::from(p);
            let canonical = raw
                .canonicalize()
                .map_err(|e| format!("invalid project_path '{p}': {e}"))?;
            if !canonical.is_dir() {
                return Err(format!("project_path is not a directory: {p}"));
            }
            if is_disallowed_root(&canonical) {
                return Err(format!(
                    "refusing to use {} as a project root: it looks like a \
                     filesystem root or the user's home directory, not a project",
                    canonical.display()
                ));
            }
            Ok(canonical)
        }
        (None, None) => Err(
            "missing project_path (server was started without a default project root)".to_string(),
        ),
    }
}

/// Filesystem root (`/`, `C:\`, ...) or the user's home directory itself.
/// Scanning/indexing either by mistake would walk far more than a project.
fn is_disallowed_root(path: &Path) -> bool {
    if path.parent().is_none() {
        return true;
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from);
    if let Some(home) = home {
        if let Ok(home_canon) = home.canonicalize() {
            if home_canon == path {
                return true;
            }
        }
    }
    false
}

fn run_tool(
    name: &str,
    args: &Value,
    default_project: Option<&Path>,
    allow_auto_scan: bool,
) -> Result<Value, String> {
    let project = resolve_project(args, default_project)?;
    let str_arg = |key: &str| -> Result<String, String> {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or(format!("missing required argument: {key}"))
    };
    let usize_arg = |key: &str| args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize);

    let call = || -> Result<Value, String> {
        match name {
            "scan" => commands::cmd_scan(&project),
            "overview" => commands::cmd_overview(&project),
            "files" => {
                let role = args
                    .get("role")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let files = commands::cmd_files(&project, role.as_deref(), usize_arg("limit"))?;
                Ok(json!({ "files": files }))
            }
            "symbols" => commands::cmd_symbols(&project, &str_arg("file")?),
            "includes" => commands::cmd_includes(&project, &str_arg("file")?),
            "related" => commands::cmd_related(&project, &str_arg("file")?),
            "focus" => commands::cmd_focus(
                &project,
                &str_arg("keyword")?,
                usize_arg("limit").unwrap_or(20),
            ),
            "refs" => commands::cmd_refs(
                &project,
                &str_arg("symbol")?,
                usize_arg("limit").unwrap_or(100),
            ),
            "snippet" => commands::cmd_snippet(
                &project,
                &str_arg("symbol")?,
                args.get("file").and_then(|v| v.as_str()),
                args.get("owner").and_then(|v| v.as_str()),
                usize_arg("context").unwrap_or(0),
                usize_arg("max_lines").unwrap_or(300).max(1),
            ),
            _ => Err(format!("unknown tool: {name}")),
        }
    };

    // Convenience over the CLI: rebuild the index on the fly instead of failing.
    // Opt-in only (`--allow-auto-scan`): auto-scanning silently writes
    // .ai-context/index.json into whatever directory `project` resolved to,
    // so it must not happen without the operator explicitly allowing it.
    match call() {
        Err(e)
            if allow_auto_scan
                && name != "scan"
                && (e.starts_with("index_not_found") || e.starts_with("index_stale")) =>
        {
            eprintln!("cpp-map mcp: auto-scanning {} ({e})", project.display());
            commands::cmd_scan(&project)?;
            call()
        }
        other => other,
    }
}

fn tool_defs(has_default_project: bool) -> Vec<Value> {
    let project_prop = json!({
        "type": "string",
        "description": "Project root directory (required: the server was started without one). Must be an existing directory; the filesystem root and the user's home directory are rejected."
    });
    let properties = |mut props: Value| -> Value {
        if !has_default_project {
            props
                .as_object_mut()
                .expect("tool properties must be objects")
                .insert("project_path".to_string(), project_prop.clone());
        }
        props
    };
    // `required` for tools whose only mandatory arg would be project_path
    let base_required = |mut extra: Vec<&str>| -> Value {
        let mut req: Vec<&str> = Vec::new();
        if !has_default_project {
            req.push("project_path");
        }
        req.append(&mut extra);
        json!(req)
    };

    vec![
        json!({
            "name": "scan",
            "description": "Scan the project and (re)build the internal index. Normal queries refresh edited/added/removed files automatically; call this for the first index or a forced rebuild.",
            "inputSchema": {
                "type": "object",
                "properties": properties(json!({})),
                "required": base_required(vec![]),
            },
        }),
        json!({
            "name": "overview",
            "description": "Minimal project overview: project type, .cbproj files, entry-point candidates, important directories. Call this first in a new project.",
            "inputSchema": {
                "type": "object",
                "properties": properties(json!({})),
                "required": base_required(vec![]),
            },
        }),
        json!({
            "name": "files",
            "description": "List indexed source files with role, related files, include/symbol counts. Always pass role and/or limit on large projects; the unfiltered list grows with project size.",
            "inputSchema": {
                "type": "object",
                "properties": properties(json!({
                    "role": {
                        "type": "string",
                        "description": "Filter by role",
                        "enum": ["entry_point", "form_or_dialog_logic", "model", "repository", "utility", "configuration", "resource", "unknown"],
                    },
                    "limit": { "type": "integer", "description": "Maximum number of files to return" },
                })),
                "required": base_required(vec![]),
            },
        }),
        json!({
            "name": "symbols",
            "description": "Symbol candidates (class/method/function/property) in one file, with line numbers, owners and base classes. Handles C++Builder constructs (__fastcall, __published, __property).",
            "inputSchema": {
                "type": "object",
                "properties": properties(json!({
                    "file": { "type": "string", "description": "Relative path or unique basename, e.g. MainForm.h" },
                })),
                "required": base_required(vec!["file"]),
            },
        }),
        json!({
            "name": "includes",
            "description": "Include relations of one file: project-internal includes (resolved), external includes, and reverse dependencies (included_by).",
            "inputSchema": {
                "type": "object",
                "properties": properties(json!({
                    "file": { "type": "string", "description": "Relative path or unique basename" },
                })),
                "required": base_required(vec!["file"]),
            },
        }),
        json!({
            "name": "related",
            "description": "Files related to one file (same-stem header/impl, included files, implementations of included headers, includers) with reasons and scores. Check this before reading a file.",
            "inputSchema": {
                "type": "object",
                "properties": properties(json!({
                    "file": { "type": "string", "description": "Relative path or unique basename" },
                })),
                "required": base_required(vec!["file"]),
            },
        }),
        json!({
            "name": "refs",
            "description": "Find code references to a symbol across the project (word-boundary match on comment/string-stripped source). Definitions/declarations are listed separately from call sites. Use before changing a function to see its impact.",
            "inputSchema": {
                "type": "object",
                "properties": properties(json!({
                    "symbol": { "type": "string", "description": "C++ identifier, e.g. LoadEmployees" },
                    "limit": { "type": "integer", "description": "Maximum reference locations to return (default 100)" },
                })),
                "required": base_required(vec!["symbol"]),
            },
        }),
        json!({
            "name": "snippet",
            "description": "Extract the source text of one symbol (function/method body, class definition) instead of reading the whole file. Without `file`, searches the entire project and prefers the definition over declarations (other occurrences are returned in other_locations).",
            "inputSchema": {
                "type": "object",
                "properties": properties(json!({
                    "symbol": { "type": "string", "description": "Symbol name, e.g. ButtonSaveClick or TMainForm" },
                    "file": { "type": "string", "description": "Restrict the search to one file (relative path or unique basename)" },
                    "owner": { "type": "string", "description": "Restrict to symbols owned by this class" },
                    "context": { "type": "integer", "description": "Extra lines of context before and after (default 0)" },
                    "max_lines": { "type": "integer", "description": "Hard cap on returned lines (default 300; sets truncated:true when hit)" },
                })),
                "required": base_required(vec!["symbol"]),
            },
        }),
        json!({
            "name": "focus",
            "description": "Rank project files related to a keyword (feature name, class/method name, Japanese UI text) and suggest a reading order. Use this first when locating where a feature lives. Shift_JIS sources are searchable.",
            "inputSchema": {
                "type": "object",
                "properties": properties(json!({
                    "keyword": { "type": "string", "description": "Search keyword, e.g. \"社員一覧\" or \"LoadEmployees\"" },
                    "limit": { "type": "integer", "description": "Maximum ranked files to return (default 20)" },
                })),
                "required": base_required(vec!["keyword"]),
            },
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_project_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "cpp_map_mcp_test_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before Unix epoch")
                .as_nanos()
        );
        dir.push(unique);
        fs::create_dir_all(&dir).expect("create temp project dir");
        dir.canonicalize().expect("canonicalize temp project dir")
    }

    #[test]
    fn pinned_root_rejects_project_path_override() {
        let root = temp_project_dir();
        let other = temp_project_dir();
        let args = json!({ "project_path": other.to_string_lossy() });

        let err = resolve_project(&args, Some(root.as_path())).expect_err("override must fail");

        assert!(err.contains("pinned root"));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(other);
    }

    #[test]
    fn pinned_root_uses_default_when_project_path_is_omitted() {
        let root = temp_project_dir();
        let args = json!({});

        let resolved = resolve_project(&args, Some(root.as_path())).expect("resolve pinned root");

        assert_eq!(resolved, root);
        let _ = fs::remove_dir_all(resolved);
    }

    #[test]
    fn unpinned_server_requires_project_path() {
        let args = json!({});

        let err = resolve_project(&args, None).expect_err("project_path must be required");

        assert!(err.contains("missing project_path"));
    }

    #[test]
    fn unpinned_server_canonicalizes_project_path() {
        let root = temp_project_dir();
        let args = json!({ "project_path": root.to_string_lossy() });

        let resolved = resolve_project(&args, None).expect("resolve project_path");

        assert_eq!(resolved, root);
        let _ = fs::remove_dir_all(resolved);
    }

    #[test]
    fn pinned_tool_schema_omits_project_path() {
        let tools = tool_defs(true);
        let overview = tools
            .iter()
            .find(|tool| tool.get("name").and_then(|v| v.as_str()) == Some("overview"))
            .expect("overview tool");
        let props = overview
            .pointer("/inputSchema/properties")
            .and_then(|v| v.as_object())
            .expect("properties object");

        assert!(!props.contains_key("project_path"));
    }

    #[test]
    fn unpinned_tool_schema_requires_project_path() {
        let tools = tool_defs(false);
        let overview = tools
            .iter()
            .find(|tool| tool.get("name").and_then(|v| v.as_str()) == Some("overview"))
            .expect("overview tool");
        let props = overview
            .pointer("/inputSchema/properties")
            .and_then(|v| v.as_object())
            .expect("properties object");
        let required = overview
            .pointer("/inputSchema/required")
            .and_then(|v| v.as_array())
            .expect("required array");

        assert!(props.contains_key("project_path"));
        assert!(
            required
                .iter()
                .any(|value| value.as_str() == Some("project_path"))
        );
    }
}

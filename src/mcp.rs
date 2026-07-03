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
The index refreshes itself: edited/added/removed files are re-parsed automatically on \
every query, so `scan` is only needed for the very first indexing or a forced rebuild.";

pub fn serve(default_project: Option<PathBuf>) -> Result<(), String> {
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
            "tools/call" => Some(Ok(handle_tool_call(&params, default_project.as_deref()))),
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

fn handle_tool_call(params: &Value, default_project: Option<&Path>) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match run_tool(name, &args, default_project) {
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

fn run_tool(name: &str, args: &Value, default_project: Option<&Path>) -> Result<Value, String> {
    let project: PathBuf = match args.get("project_path").and_then(|v| v.as_str()) {
        Some(p) => PathBuf::from(p),
        None => default_project
            .map(|p| p.to_path_buf())
            .ok_or("missing project_path (server was started without a default project root)")?,
    };
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
    match call() {
        Err(e)
            if name != "scan"
                && (e.starts_with("index_not_found") || e.starts_with("index_stale")) =>
        {
            commands::cmd_scan(&project)?;
            call()
        }
        other => other,
    }
}

fn tool_defs(has_default_project: bool) -> Vec<Value> {
    let project_desc = if has_default_project {
        "Project root directory. Optional; defaults to the root the server was started with."
    } else {
        "Project root directory (required: the server was started without one)."
    };
    let project_prop = json!({ "type": "string", "description": project_desc });
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
                "properties": { "project_path": project_prop },
                "required": base_required(vec![]),
            },
        }),
        json!({
            "name": "overview",
            "description": "Minimal project overview: project type, .cbproj files, entry-point candidates, important directories. Call this first in a new project.",
            "inputSchema": {
                "type": "object",
                "properties": { "project_path": project_prop },
                "required": base_required(vec![]),
            },
        }),
        json!({
            "name": "files",
            "description": "List indexed source files with role, related files, include/symbol counts. Always pass role and/or limit on large projects; the unfiltered list grows with project size.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_path": project_prop,
                    "role": {
                        "type": "string",
                        "description": "Filter by role",
                        "enum": ["entry_point", "form_or_dialog_logic", "model", "repository", "utility", "configuration", "resource", "unknown"],
                    },
                    "limit": { "type": "integer", "description": "Maximum number of files to return" },
                },
                "required": base_required(vec![]),
            },
        }),
        json!({
            "name": "symbols",
            "description": "Symbol candidates (class/method/function/property) in one file, with line numbers, owners and base classes. Handles C++Builder constructs (__fastcall, __published, __property).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_path": project_prop,
                    "file": { "type": "string", "description": "Relative path or unique basename, e.g. MainForm.h" },
                },
                "required": base_required(vec!["file"]),
            },
        }),
        json!({
            "name": "includes",
            "description": "Include relations of one file: project-internal includes (resolved), external includes, and reverse dependencies (included_by).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_path": project_prop,
                    "file": { "type": "string", "description": "Relative path or unique basename" },
                },
                "required": base_required(vec!["file"]),
            },
        }),
        json!({
            "name": "related",
            "description": "Files related to one file (same-stem header/impl, included files, implementations of included headers, includers) with reasons and scores. Check this before reading a file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_path": project_prop,
                    "file": { "type": "string", "description": "Relative path or unique basename" },
                },
                "required": base_required(vec!["file"]),
            },
        }),
        json!({
            "name": "refs",
            "description": "Find code references to a symbol across the project (word-boundary match on comment/string-stripped source). Definitions/declarations are listed separately from call sites. Use before changing a function to see its impact.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_path": project_prop,
                    "symbol": { "type": "string", "description": "C++ identifier, e.g. LoadEmployees" },
                    "limit": { "type": "integer", "description": "Maximum reference locations to return (default 100)" },
                },
                "required": base_required(vec!["symbol"]),
            },
        }),
        json!({
            "name": "snippet",
            "description": "Extract the source text of one symbol (function/method body, class definition) instead of reading the whole file. Without `file`, searches the entire project and prefers the definition over declarations (other occurrences are returned in other_locations).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_path": project_prop,
                    "symbol": { "type": "string", "description": "Symbol name, e.g. ButtonSaveClick or TMainForm" },
                    "file": { "type": "string", "description": "Restrict the search to one file (relative path or unique basename)" },
                    "owner": { "type": "string", "description": "Restrict to symbols owned by this class" },
                    "context": { "type": "integer", "description": "Extra lines of context before and after (default 0)" },
                    "max_lines": { "type": "integer", "description": "Hard cap on returned lines (default 300; sets truncated:true when hit)" },
                },
                "required": base_required(vec!["symbol"]),
            },
        }),
        json!({
            "name": "focus",
            "description": "Rank project files related to a keyword (feature name, class/method name, Japanese UI text) and suggest a reading order. Use this first when locating where a feature lives. Shift_JIS sources are searchable.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_path": project_prop,
                    "keyword": { "type": "string", "description": "Search keyword, e.g. \"社員一覧\" or \"LoadEmployees\"" },
                    "limit": { "type": "integer", "description": "Maximum ranked files to return (default 20)" },
                },
                "required": base_required(vec!["keyword"]),
            },
        }),
    ]
}

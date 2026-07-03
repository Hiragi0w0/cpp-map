mod commands;
mod index;
mod mcp;
mod parse;
mod scan;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::ExitCode;

/// AI-agent-oriented context compression CLI for C++Builder projects.
/// All output is JSON on stdout (use --pretty for humans).
#[derive(Parser)]
#[command(name = "cpp-map", version)]
struct Cli {
    /// Pretty-print the JSON output
    #[arg(long, global = true)]
    pretty: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan the project and build the internal index (.ai-context/index.json)
    Scan { project_path: PathBuf },
    /// Minimal project overview for a first look
    Overview { project_path: PathBuf },
    /// List indexed files
    Files {
        project_path: PathBuf,
        /// Filter by role (entry_point, form_or_dialog_logic, model, repository,
        /// utility, configuration, resource, unknown)
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        /// Emit one JSON object per line instead of a single document
        #[arg(long)]
        ndjson: bool,
    },
    /// Symbol candidates in one file
    Symbols {
        project_path: PathBuf,
        #[arg(long)]
        file: String,
    },
    /// Include relations of one file
    Includes {
        project_path: PathBuf,
        #[arg(long)]
        file: String,
    },
    /// Files related to one file, with reasons and scores
    Related {
        project_path: PathBuf,
        #[arg(long)]
        file: String,
    },
    /// Rank files related to a keyword (feature name, symbol, Japanese text, ...)
    Focus {
        project_path: PathBuf,
        keyword: String,
        /// Maximum number of ranked files to return
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Find code references to a symbol across the project (word-boundary match)
    Refs {
        project_path: PathBuf,
        /// Symbol name (C++ identifier), e.g. LoadEmployees
        symbol: String,
        /// Maximum number of reference locations to return
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Extract the source text of one symbol (function/method/class body)
    Snippet {
        project_path: PathBuf,
        /// Symbol name, e.g. ButtonSaveClick or TMainForm
        #[arg(long)]
        symbol: String,
        /// Restrict the search to one file (relative path or unique basename)
        #[arg(long)]
        file: Option<String>,
        /// Restrict to symbols owned by this class
        #[arg(long)]
        owner: Option<String>,
        /// Extra lines of context before and after the symbol
        #[arg(long, default_value_t = 0)]
        context: usize,
        /// Hard cap on returned lines (sets "truncated": true when hit)
        #[arg(long, default_value_t = 300)]
        max_lines: usize,
    },
    /// Run as an MCP server over stdio, exposing every command as a tool
    Mcp {
        /// Default project root; when given, tools may omit project_path
        project_path: Option<PathBuf>,
    },
}

fn emit(value: &Value, pretty: bool) {
    let s = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .expect("serializable");
    println!("{s}");
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let pretty = cli.pretty;

    let result: Result<Value, String> = match &cli.cmd {
        Cmd::Scan { project_path } => commands::cmd_scan(project_path),
        Cmd::Overview { project_path } => commands::cmd_overview(project_path),
        Cmd::Files {
            project_path,
            role,
            limit,
            ndjson,
        } => match commands::cmd_files(project_path, role.as_deref(), *limit) {
            Ok(files) => {
                if *ndjson {
                    for f in &files {
                        emit(f, false);
                    }
                    return ExitCode::SUCCESS;
                }
                Ok(json!({ "files": files }))
            }
            Err(e) => Err(e),
        },
        Cmd::Symbols { project_path, file } => commands::cmd_symbols(project_path, file),
        Cmd::Includes { project_path, file } => commands::cmd_includes(project_path, file),
        Cmd::Related { project_path, file } => commands::cmd_related(project_path, file),
        Cmd::Focus {
            project_path,
            keyword,
            limit,
        } => commands::cmd_focus(project_path, keyword, *limit),
        Cmd::Refs {
            project_path,
            symbol,
            limit,
        } => commands::cmd_refs(project_path, symbol, *limit),
        Cmd::Snippet {
            project_path,
            symbol,
            file,
            owner,
            context,
            max_lines,
        } => commands::cmd_snippet(
            project_path,
            symbol,
            file.as_deref(),
            owner.as_deref(),
            *context,
            (*max_lines).max(1),
        ),
        Cmd::Mcp { project_path } => {
            // stdout is the protocol channel: report fatal errors on stderr only
            return match mcp::serve(project_path.clone()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("cpp-map mcp: {e}");
                    ExitCode::FAILURE
                }
            };
        }
    };

    match result {
        Ok(v) => {
            emit(&v, pretty);
            ExitCode::SUCCESS
        }
        Err(msg) => {
            emit(&json!({ "status": "error", "error": msg }), pretty);
            ExitCode::FAILURE
        }
    }
}

**English** | [日本語](README.ja.md)

# cpp-map

A context-compression CLI built for AI agents. Targeting C++Builder projects,
it returns only "the files, symbols, and dependencies worth reading" as JSON,
instead of making the agent read the whole project. It also includes a
human-oriented `graph` command for dependency visualization.

Version: `1.0.1`
Author: `Hiragi0w0`  
Project edition: `2026`  
Rust edition: `2024`

This is a helper tool for lowering the "read everything first" cost when
investigating or modifying legacy C++Builder/VCL codebases with AI agents. It
scans `.cpp` / `.h` / `.hpp` / `.cc` / `.cxx` / `.cbproj` files and, instead of
the source body, returns a small JSON payload of paths, roles, include
relationships, symbol candidates, line numbers, and relevance reasons. Because
you pull the actual body only where you need it via `snippet`, it stays easy to
narrow the context handed to an AI even for huge form implementations or legacy
Shift_JIS assets.

## What it is for

- Get a rough grasp of a C++Builder project's entry points, forms, repositories, models, and utilities
- Rank the files worth reading from Japanese UI text or feature names
- Follow `.cpp` and `.h`, include targets, include sources, and same-name implementation files
- Visualize dependencies from one file as text, Mermaid, DOT, HTML, or Graphviz images
- Retrieve candidates and line numbers for functions, methods, classes, and `__property`
- Separate a symbol's definition from its references before making a change
- Run as an MCP server so an AI agent can call the same features as tools

## Design principles

- Prefer JSON output that AI agents can handle mechanically
- Do not store full source in the index; slice out only the needed body from the live file
- Use tolerant candidate extraction tuned to common C++Builder syntax rather than a full C++ AST parse
- After the first run, re-parse only changed/added/deleted files based on mtime/size
- Exclude `.dfm` and build artifacts; keep to the minimal structural information needed for code investigation

## Build

```bash
cargo build --release
# => target/release/cpp-map(.exe)
```

Requires a Rust toolchain with Rust 2024 edition support (Rust 1.85 or newer).

## Install

To fetch from GitHub and use locally, install a Rust toolchain and build.

```bash
git clone https://github.com/Hiragi0w0/cpp-map.git
cd cpp-map
cargo build --release
```

The produced executable is at `target/release/cpp-map.exe`, or
`target/release/cpp-map` on Unix-like environments.

## GitHub Releases

Pushing a `v*` tag, such as `v1.0.0`, runs the release workflow and creates a
GitHub Release. The workflow builds the Windows binary and attaches these
assets:

- `cpp-map.exe`
- `cpp-map.exe.sha256`

## Usage

```bash
# Once, first: build the index (generates only .ai-context/index.json)
cpp-map scan .

# Minimal project overview
cpp-map overview .

# File listing (role filter, count limit, NDJSON support)
cpp-map files . --role form_or_dialog_logic --limit 20
cpp-map files . --ndjson

# Symbol candidates in a specific file (class / method / function / property + line)
cpp-map symbols . --file MainForm.h

# Include relationships (resolved in-project + external includes + reverse deps)
cpp-map includes . --file MainForm.cpp

# Related files (same-stem header/impl, include targets, include sources; with reason and score)
cpp-map related . --file MainForm.cpp

# Rank related files from a keyword (Japanese keywords OK, Shift-JIS sources supported)
cpp-map focus . "社員一覧"

# Slice out only the body for a single symbol (an alternative to reading a whole file)
# When --file is omitted, it searches the whole project and prefers the definition body over the declaration
cpp-map snippet . --symbol ButtonSaveClick
cpp-map snippet . --symbol LoadEmployees --file EmployeeListForm.cpp --context 2

# Reverse-lookup a symbol's references (returns definitions and call sites separately)
cpp-map refs . LoadEmployees

# Human-readable dependency graph from the current directory as the project root
cpp-map graph src/A.cpp
cpp-map graph src/A.cpp --format mermaid
cpp-map graph src/A.cpp --format dot --output graph.dot
cpp-map graph src/A.cpp --format html --output graph.html --open
cpp-map graph src/A.cpp --format png --output graph.png
cpp-map graph src/A.cpp --format svg --output graph.svg --keep-dot
```

For the JSON query commands, run `scan` once before the first query. Every
subsequent query compares mtime/size at runtime and automatically re-parses only
the changed/added/deleted files (incremental re-scan). The human-facing `graph`
command also creates the index on first use when it is missing.

The default output is compact JSON. Add `--pretty` when a human is reading it.
The `graph` command is the exception: it is human-oriented and prints a
terminal summary plus Mermaid by default. Use `--project <dir>` when the project
root is not the current directory. `html`/`svg`/`png`/`pdf` output requires
`--output <file>`, and `svg`/`png`/`pdf` additionally require Graphviz. If
Graphviz is not on `PATH`, pass `--graphviz-path`.

## A typical investigation flow

For a project you are seeing for the first time, run `scan` to build
`.ai-context/index.json`, then check entry-point candidates and key directories
with `overview`.

```bash
cpp-map scan C:\path\to\project
cpp-map overview C:\path\to\project --pretty
```

When you already know a feature name, screen name, or Japanese UI text, start
from `focus`. Use the returned `suggested_reading_order` as a guide to reading
order, and combine `related`, `symbols`, and `snippet` per candidate file.

```bash
cpp-map focus . "社員一覧" --pretty
cpp-map related . --file forms/EmployeeListForm.cpp --pretty
cpp-map symbols . --file forms/EmployeeListForm.h --pretty
cpp-map snippet . --symbol LoadEmployees --file forms/EmployeeListForm.cpp --context 2 --pretty
```

Before changing an existing method, use `refs` to confirm its definition and
call sites separately. `refs` performs word-boundary matching on code with
comments and strings stripped out, so it is better suited to checking call sites
than a plain full-text search.

```bash
cpp-map refs . LoadEmployees --pretty
```

## Using it as an MCP server

It embeds a stdio server that exposes every command as an MCP tool.

```bash
# Start pinned to a project root (project_path can be omitted per tool)
cpp-map mcp C:\path\to\project

# Start without pinning a root (project_path is required on each tool call)
cpp-map mcp
```

Registering with Claude Code:

```bash
claude mcp add cpp-map -- C:\path\to\cpp-map.exe mcp C:\path\to\project
```

Unlike the CLI, calling a query with no index yet triggers an automatic `scan`
before responding. For normal queries after the index exists, it does the same
incremental re-scan of only changed/added/deleted files as the CLI. To force a
full rebuild, call the `scan` tool.

The MCP tool names match the CLI subcommands, exposing `scan`, `overview`,
`files`, `symbols`, `includes`, `related`, `focus`, `refs`, `snippet`, and
`graph`. If
you pass a project root at startup, each tool's `project_path` can be omitted.
If you start without pinning a root, `project_path` is required on every tool
call.

## Features

- Targets only `.cpp` `.h` `.hpp` `.cc` `.cxx` `.cbproj`. Excludes `.dfm`, build
  artifacts, and directories like `__history/` and `Debug/`
- Generally does not emit source bodies; returns paths, line numbers, symbol
  names, and relevance reasons (the exception is `snippet`, which returns only
  the queried symbol's range, capped by `--max-lines`)
- `graph` renders a human-facing dependency graph. It excludes system and
  project-external includes by default, adds same-stem implementations for
  included headers, and supports reverse dependency lookup with `--reverse`
- Detects line drift between the index and the real file and prompts a re-scan
  with an `index_stale` error
- No full AST parse; tolerant candidate extraction via comment/string stripping
  plus regular expressions (recognizes `__fastcall`, `__published`, `__property`)
- Reads files by auto-detecting UTF-8 / UTF-16 BOM / Shift_JIS (cp932)
- The only generated artifact is `.ai-context/index.json`

## Technical structure

The implementation is a single Rust binary; the CLI and the MCP server share the
same command implementations.

- `src/main.rs`: CLI argument definitions and JSON output
- `src/scan.rs`: project traversal, excluded-directory detection, index generation, incremental re-scan
- `src/parse.rs`: candidate extraction for includes, classes, methods, functions, and `__property`
- `src/index.rs`: on-disk format of `.ai-context/index.json`, file-argument resolution, same-stem header/impl pairing
- `src/graph.rs`: dependency graph construction, text/Mermaid/DOT/HTML rendering, and optional Graphviz image output
- `src/commands.rs`: query handling for `overview`, `files`, `focus`, `snippet`, and so on
- `src/mcp.rs`: MCP tools/list and tools/call over JSON-RPC over stdio

The index stores no source bodies. It stores metadata such as the
project-relative path, file kind, inferred role, resolved includes, external
includes, symbol candidates, and mtime/size. The body searches in `snippet` and
`focus` re-read the real file on demand.

Include resolution looks for candidates in this order: relative to the including
directory, relative to the project root, then by same basename. Symbol
extraction blanks out comments and strings while preserving line numbers, and
handles `__fastcall`, `__published`, `__property`, and VCL form base-class
candidates commonly used in C++Builder.

## Limitations

- It is not a full C++ compiler or AST, so it does not strictly interpret templates, macro expansion, or conditional compilation
- Symbols are "candidates"; same-name, overloaded, or macro-generated definitions may be ambiguous
- `.dfm` is not parsed directly; form detection relies on `#pragma resource "*.dfm"` inside the `.cpp`
- `focus` is a ranking that combines path, file name, symbol name, includes, and body-occurrence count — not semantic analysis
- Because `snippet` uses the index's line numbers, a same-size edit undetectable by mtime/size can cause it to return `index_stale`

## Roles

`entry_point` `form_or_dialog_logic` `model` `repository` `utility`
`configuration` `resource` `unknown`

## Tests

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

GitHub Actions (on Windows) runs `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings`. Because clippy compiles all
targets, it also validates build health. The integration tests
(`tests/cli.rs` / `tests/mcp.rs`) are for local development and are not included
in the public repository; if you have the test suite locally, run it with
`cargo test`.

## License

Dual-licensed under either the MIT license ([`LICENSE-MIT`](LICENSE-MIT)) or the
Apache License 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE)), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall be
dual-licensed as above, without any additional terms or conditions.

# MCP project root safety

This note documents the MCP root-handling behavior used to prevent accidental or prompt-injected scans outside the intended project.

## Pinned root mode

```bash
cpp-map mcp C:\path\to\project
```

When a project root is passed at server startup, the server is pinned to that root.

- Tool calls may omit `project_path`.
- Tool calls must not pass `project_path`.
- If `project_path` is supplied, the tool call returns an error instead of overriding the pinned root.
- The startup root is canonicalized once and must be an existing directory.

This is the recommended mode for AI-agent use.

## Unpinned mode

```bash
cpp-map mcp
```

When no project root is passed at server startup, each tool call must include `project_path`.

The supplied path is canonicalized, must be an existing directory, and is rejected when it looks like a filesystem root or the user's home directory rather than a project.

## Auto scan

MCP query tools do not auto-run `scan` by default when the index is missing or stale. This avoids silently writing `.ai-context/index.json` into a directory the operator did not intend to modify.

To opt in to the old convenience behavior, start the server with:

```bash
cpp-map mcp C:\path\to\project --allow-auto-scan
```

Without `--allow-auto-scan`, run `scan` explicitly before query tools that need the index.

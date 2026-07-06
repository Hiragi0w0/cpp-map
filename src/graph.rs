//! Human-oriented dependency graph building and rendering.

use crate::index::{Index, resolve_file_arg, same_stem_counterparts};
use crate::scan;
use clap::ValueEnum;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Depth {
    #[value(name = "1")]
    One,
    #[value(name = "2")]
    Two,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum GraphFormat {
    Text,
    Mermaid,
    Dot,
    Html,
    Svg,
    Png,
    Pdf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Layout {
    Dot,
    Neato,
    Fdp,
    Sfdp,
    Circo,
    Twopi,
}

#[derive(Clone, Debug)]
pub struct GraphOptions {
    pub depth: Depth,
    pub format: Option<GraphFormat>,
    pub output: Option<PathBuf>,
    pub open: bool,
    pub layout: Layout,
    pub graphviz_path: Option<PathBuf>,
    pub keep_dot: bool,
    pub include_system: bool,
    pub include_external: bool,
    pub impl_pair: bool,
    pub reverse: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyGraph {
    pub root: String,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphNode {
    pub id: String,
    pub label: String,
    pub kind: FileKind,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Source,
    Header,
    System,
    External,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    Include,
    ImplementationPair,
    Reverse,
}

pub fn run_graph(project: &Path, file: &str, options: &GraphOptions) -> Result<(), String> {
    let graph = build_graph(project, file, options)?;
    let requested = options.format.unwrap_or(GraphFormat::Text);

    match requested {
        GraphFormat::Svg | GraphFormat::Png | GraphFormat::Pdf => {
            let output = require_output(options, requested)?;
            render_graphviz(&graph, requested, &output, options)?;
            maybe_open(&output, options.open);
        }
        GraphFormat::Html => {
            let output = require_output(options, requested)?;
            fs::write(&output, render_html(&graph))
                .map_err(|e| format!("cannot write {}: {e}", output.display()))?;
            maybe_open(&output, options.open);
        }
        GraphFormat::Text | GraphFormat::Mermaid | GraphFormat::Dot => {
            let rendered = match options.format {
                None => format!("{}\n\n{}", render_text(&graph), render_mermaid(&graph)),
                Some(GraphFormat::Text) => render_text(&graph),
                Some(GraphFormat::Mermaid) => render_mermaid(&graph),
                Some(GraphFormat::Dot) => render_dot(&graph),
                _ => unreachable!("handled above"),
            };
            if let Some(output) = &options.output {
                fs::write(output, rendered)
                    .map_err(|e| format!("cannot write {}: {e}", output.display()))?;
                maybe_open(output, options.open);
            } else {
                println!("{rendered}");
            }
        }
    }

    Ok(())
}

pub fn build_graph(
    project: &Path,
    file: &str,
    options: &GraphOptions,
) -> Result<DependencyGraph, String> {
    let index = graph_index(project)?;
    let root = resolve_file_arg(&index, file)?;
    let mut builder = GraphBuilder::new(&index, root.clone(), options);
    if options.reverse {
        builder.build_reverse();
    } else {
        builder.build_forward();
    }
    Ok(builder.finish())
}

fn graph_index(project: &Path) -> Result<Index, String> {
    match scan::ensure_fresh(project, scan::FreshnessMode::AutoRefresh) {
        Ok(index) => Ok(index),
        Err(e) if e.starts_with("index_not_found") => {
            let result = scan::scan(project)?;
            crate::index::save(project, &result.index)?;
            Ok(result.index)
        }
        Err(e) => Err(e),
    }
}

pub fn render_for_format(graph: &DependencyGraph, format: Option<GraphFormat>) -> String {
    match format {
        None => format!("{}\n\n{}", render_text(graph), render_mermaid(graph)),
        Some(GraphFormat::Text) => render_text(graph),
        Some(GraphFormat::Mermaid) => render_mermaid(graph),
        Some(GraphFormat::Dot) => render_dot(graph),
        Some(GraphFormat::Html) => render_html(graph),
        Some(GraphFormat::Svg | GraphFormat::Png | GraphFormat::Pdf) => {
            unreachable!("image formats are rendered by Graphviz")
        }
    }
}

struct GraphBuilder<'a> {
    index: &'a Index,
    root: String,
    options: &'a GraphOptions,
    nodes: BTreeMap<String, GraphNode>,
    edges: BTreeSet<GraphEdge>,
}

impl<'a> GraphBuilder<'a> {
    fn new(index: &'a Index, root: String, options: &'a GraphOptions) -> Self {
        let mut this = Self {
            index,
            root,
            options,
            nodes: BTreeMap::new(),
            edges: BTreeSet::new(),
        };
        let root = this.root.clone();
        this.add_file_node(&root);
        this
    }

    fn build_forward(&mut self) {
        let mut queue = VecDeque::from([(self.root.clone(), 0usize)]);
        let mut best_seen: BTreeMap<String, usize> = BTreeMap::new();

        while let Some((rel, level)) = queue.pop_front() {
            if best_seen.get(&rel).is_some_and(|seen| *seen <= level) {
                continue;
            }
            best_seen.insert(rel.clone(), level);
            if level >= self.max_depth() {
                continue;
            }

            let Some(entry) = self.index.files.get(&rel) else {
                continue;
            };
            for include in &entry.includes {
                self.add_file_node(include);
                self.add_edge(&rel, include, EdgeKind::Include);
                queue.push_back((include.clone(), level + 1));
                self.add_impl_pairs(include, level);
            }
            self.add_external_edges(&rel, entry);
        }
    }

    fn build_reverse(&mut self) {
        let mut queue = VecDeque::from([(self.root.clone(), 0usize)]);
        let mut best_seen: BTreeMap<String, usize> = BTreeMap::new();

        while let Some((target, level)) = queue.pop_front() {
            if best_seen.get(&target).is_some_and(|seen| *seen <= level) {
                continue;
            }
            best_seen.insert(target.clone(), level);
            if level >= self.max_depth() {
                continue;
            }

            let includers: Vec<String> = self
                .index
                .files
                .iter()
                .filter(|(_, entry)| entry.includes.contains(&target))
                .map(|(rel, _)| rel.clone())
                .collect();

            for includer in includers {
                self.add_file_node(&includer);
                self.add_edge(&includer, &target, EdgeKind::Reverse);
                queue.push_back((includer, level + 1));
            }
        }
    }

    fn add_impl_pairs(&mut self, rel: &str, level: usize) {
        if !self.options.impl_pair || level >= self.max_depth() {
            return;
        }
        if self
            .index
            .files
            .get(rel)
            .is_none_or(|entry| entry.kind != "header")
        {
            return;
        }
        for counterpart in same_stem_counterparts(self.index, rel) {
            self.add_file_node(&counterpart);
            self.add_edge(rel, &counterpart, EdgeKind::ImplementationPair);
        }
    }

    fn add_external_edges(&mut self, from: &str, entry: &crate::index::FileEntry) {
        for raw in &entry.external_includes {
            let (kind, label) = classify_external(raw);
            match kind {
                FileKind::System if !self.options.include_system => continue,
                FileKind::External if !self.options.include_external => continue,
                _ => {}
            }
            let id = match kind {
                FileKind::System => format!("system:{raw}"),
                FileKind::External => format!("external:{raw}"),
                _ => unreachable!("external classification must be system or external"),
            };
            self.nodes.entry(id.clone()).or_insert(GraphNode {
                id: id.clone(),
                label,
                kind,
            });
            self.add_edge(from, &id, EdgeKind::Include);
        }
    }

    fn add_file_node(&mut self, rel: &str) {
        let kind = self
            .index
            .files
            .get(rel)
            .map(|entry| file_kind(&entry.kind))
            .unwrap_or(FileKind::Unknown);
        self.nodes.entry(rel.to_string()).or_insert(GraphNode {
            id: rel.to_string(),
            label: rel.to_string(),
            kind,
        });
    }

    fn add_edge(&mut self, from: &str, to: &str, kind: EdgeKind) {
        self.edges.insert(GraphEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind,
        });
    }

    fn max_depth(&self) -> usize {
        match self.options.depth {
            Depth::One => 1,
            Depth::Two => 2,
            Depth::All => usize::MAX,
        }
    }

    fn finish(self) -> DependencyGraph {
        DependencyGraph {
            root: self.root,
            nodes: self.nodes.into_values().collect(),
            edges: self.edges.into_iter().collect(),
        }
    }
}

fn file_kind(kind: &str) -> FileKind {
    match kind {
        "cpp" => FileKind::Source,
        "header" => FileKind::Header,
        _ => FileKind::Unknown,
    }
}

fn classify_external(raw: &str) -> (FileKind, String) {
    let normalized = raw.replace('\\', "/");
    if !normalized.contains('/') && !normalized.contains('.') {
        (FileKind::System, format!("<{raw}>"))
    } else {
        (FileKind::External, raw.to_string())
    }
}

fn require_output(options: &GraphOptions, format: GraphFormat) -> Result<PathBuf, String> {
    options
        .output
        .clone()
        .ok_or_else(|| format!("{} output requires --output <file>", format_name(format)))
}

fn format_name(format: GraphFormat) -> &'static str {
    match format {
        GraphFormat::Text => "text",
        GraphFormat::Mermaid => "mermaid",
        GraphFormat::Dot => "dot",
        GraphFormat::Html => "html",
        GraphFormat::Svg => "svg",
        GraphFormat::Png => "png",
        GraphFormat::Pdf => "pdf",
    }
}

pub fn render_text(graph: &DependencyGraph) -> String {
    let mut lines = vec![
        format!("Dependency graph for: {}", graph.root),
        String::new(),
    ];
    lines.push(graph.root.clone());
    let children = outgoing(graph);
    append_text_children(
        graph,
        &children,
        &graph.root,
        "",
        &mut lines,
        &mut BTreeSet::new(),
    );
    lines.join("\n")
}

fn append_text_children(
    graph: &DependencyGraph,
    children: &BTreeMap<&str, Vec<&GraphEdge>>,
    node: &str,
    prefix: &str,
    lines: &mut Vec<String>,
    path_seen: &mut BTreeSet<String>,
) {
    if !path_seen.insert(node.to_string()) {
        return;
    }
    let edges = children.get(node).cloned().unwrap_or_default();
    for (i, edge) in edges.iter().enumerate() {
        let last = i + 1 == edges.len();
        let branch = if last { "`-" } else { "|-" };
        let next_prefix = if last { "  " } else { "| " };
        let label = label_for(graph, &edge.to);
        lines.push(format!(
            "{prefix}{branch} {}: {label}",
            edge_kind_label(edge.kind)
        ));
        append_text_children(
            graph,
            children,
            &edge.to,
            &format!("{prefix}{next_prefix}"),
            lines,
            path_seen,
        );
    }
    path_seen.remove(node);
}

fn outgoing(graph: &DependencyGraph) -> BTreeMap<&str, Vec<&GraphEdge>> {
    let mut map: BTreeMap<&str, Vec<&GraphEdge>> = BTreeMap::new();
    for edge in &graph.edges {
        map.entry(edge.from.as_str()).or_default().push(edge);
    }
    map
}

fn edge_kind_label(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Include => "include",
        EdgeKind::ImplementationPair => "implementation",
        EdgeKind::Reverse => "included_by",
    }
}

pub fn render_mermaid(graph: &DependencyGraph) -> String {
    let mut out = String::from("graph LR\n");
    for edge in &graph.edges {
        out.push_str(&format!(
            "  {}[\"{}\"] --> {}[\"{}\"]\n",
            mermaid_id(&edge.from),
            escape_mermaid(label_for(graph, &edge.from)),
            mermaid_id(&edge.to),
            escape_mermaid(label_for(graph, &edge.to))
        ));
    }
    out
}

pub fn render_dot(graph: &DependencyGraph) -> String {
    let mut out = String::from("digraph dependencies {\n  rankdir=LR;\n");
    for node in &graph.nodes {
        out.push_str(&format!(
            "  \"{}\" [label=\"{}\"];\n",
            escape_dot(&node.id),
            escape_dot(&node.label)
        ));
    }
    for edge in &graph.edges {
        out.push_str(&format!(
            "  \"{}\" -> \"{}\";\n",
            escape_dot(&edge.from),
            escape_dot(&edge.to)
        ));
    }
    out.push_str("}\n");
    out
}

pub fn render_html(graph: &DependencyGraph) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>cpp-map dependency graph</title>
  <style>
    body {{ font-family: system-ui, sans-serif; margin: 24px; color: #202124; }}
    h1 {{ font-size: 20px; margin: 0 0 16px; }}
    .mermaid {{ background: #fff; }}
  </style>
</head>
<body>
  <h1>Dependency graph for: {}</h1>
  <pre class="mermaid">
{}
  </pre>
  <script type="module">
    import mermaid from 'https://cdn.jsdelivr.net/npm/mermaid@10/dist/mermaid.esm.min.mjs';
    mermaid.initialize({{ startOnLoad: true }});
  </script>
</body>
</html>
"#,
        html_escape(&graph.root),
        html_escape(&render_mermaid(graph))
    )
}

fn render_graphviz(
    graph: &DependencyGraph,
    format: GraphFormat,
    output: &Path,
    options: &GraphOptions,
) -> Result<(), String> {
    let dot = render_dot(graph);
    let mut dot_file = None;
    if options.keep_dot {
        let path = output.with_extension("dot");
        fs::write(&path, &dot).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        dot_file = Some(path);
    }

    let program = options
        .graphviz_path
        .as_ref()
        .map(|p| p.as_os_str().to_os_string())
        .unwrap_or_else(|| layout_name(options.layout).into());
    let mut command = Command::new(program);
    command.arg(format!("-T{}", format_name(format)));
    if let Some(path) = &dot_file {
        command.arg(path);
    } else {
        command.stdin(Stdio::piped());
    }
    command.arg("-o").arg(output);

    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            graphviz_missing_message()
        } else {
            format!("failed to start Graphviz: {e}")
        }
    })?;
    if dot_file.is_none()
        && let Some(stdin) = child.stdin.as_mut()
    {
        stdin
            .write_all(dot.as_bytes())
            .map_err(|e| format!("failed to write DOT to Graphviz: {e}"))?;
    }
    let status = child
        .wait()
        .map_err(|e| format!("failed to wait for Graphviz: {e}"))?;
    if !status.success() {
        return Err(format!("Graphviz failed with status: {status}"));
    }
    Ok(())
}

fn graphviz_missing_message() -> String {
    "画像出力には Graphviz の dot コマンドが必要です。\n\n解決方法:\n1. Graphviz をインストールしてください\n2. dot コマンドに PATH を通してください\n3. または --graphviz-path で dot.exe の場所を指定してください\n\n例:\ncpp-map graph src/A.cpp --format png --output graph.png --graphviz-path \"C:\\Program Files\\Graphviz\\bin\\dot.exe\"".to_string()
}

fn layout_name(layout: Layout) -> &'static str {
    match layout {
        Layout::Dot => "dot",
        Layout::Neato => "neato",
        Layout::Fdp => "fdp",
        Layout::Sfdp => "sfdp",
        Layout::Circo => "circo",
        Layout::Twopi => "twopi",
    }
}

fn maybe_open(path: &Path, open: bool) {
    if !open {
        return;
    }
    let result = if cfg!(windows) {
        Command::new("cmd")
            .arg("/C")
            .arg("start")
            .arg("")
            .arg(path)
            .status()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(path).status()
    } else {
        Command::new("xdg-open").arg(path).status()
    };
    match result {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("warning: failed to open {}: {status}", path.display()),
        Err(e) => eprintln!("warning: failed to open {}: {e}", path.display()),
    }
}

fn label_for<'a>(graph: &'a DependencyGraph, id: &'a str) -> &'a str {
    graph
        .nodes
        .iter()
        .find(|node| node.id == id)
        .map(|node| node.label.as_str())
        .unwrap_or(id)
}

fn mermaid_id(value: &str) -> String {
    let mut id = String::from("n_");
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            id.push(ch);
        } else {
            id.push('_');
        }
    }
    id
}

fn escape_mermaid(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape_dot(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_project_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "cpp_map_graph_test_{}_{}",
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

    fn write_source(dir: &Path, name: &str, contents: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write source file");
    }

    fn options() -> GraphOptions {
        GraphOptions {
            depth: Depth::One,
            format: None,
            output: None,
            open: false,
            layout: Layout::Dot,
            graphviz_path: None,
            keep_dot: false,
            include_system: false,
            include_external: false,
            impl_pair: true,
            reverse: false,
        }
    }

    #[test]
    fn depth_one_includes_direct_headers_and_impl_pairs() {
        let dir = temp_project_dir();
        write_source(
            &dir,
            "src/A.cpp",
            "#include \"B.h\"\n#include <vector>\nint main(){return 0;}",
        );
        write_source(&dir, "src/B.h", "#include \"Common.h\"\n");
        write_source(&dir, "src/B.cpp", "#include \"B.h\"\n");
        write_source(&dir, "src/Common.h", "");

        let graph = build_graph(&dir, "src/A.cpp", &options()).expect("build graph");

        assert!(graph.nodes.iter().any(|node| node.id == "src/B.h"));
        assert!(graph.nodes.iter().any(|node| node.id == "src/B.cpp"));
        assert!(!graph.nodes.iter().any(|node| node.id == "src/Common.h"));
        assert_eq!(
            graph
                .edges
                .iter()
                .filter(|edge| edge.from == "src/A.cpp" && edge.to == "src/B.h")
                .count(),
            1
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn depth_two_follows_transitive_includes() {
        let dir = temp_project_dir();
        write_source(&dir, "src/A.cpp", "#include \"B.h\"\n");
        write_source(&dir, "src/B.h", "#include \"Common.h\"\n");
        write_source(&dir, "src/Common.h", "");

        let mut opts = options();
        opts.depth = Depth::Two;
        let graph = build_graph(&dir, "src/A.cpp", &opts).expect("build graph");

        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.from == "src/B.h" && edge.to == "src/Common.h")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_impl_pair_disables_header_to_source_edge() {
        let dir = temp_project_dir();
        write_source(&dir, "A.cpp", "#include \"B.h\"\n");
        write_source(&dir, "B.h", "");
        write_source(&dir, "B.cpp", "");

        let mut opts = options();
        opts.impl_pair = false;
        let graph = build_graph(&dir, "A.cpp", &opts).expect("build graph");

        assert!(!graph.nodes.iter().any(|node| node.id == "B.cpp"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn include_system_and_external_are_opt_in() {
        let dir = temp_project_dir();
        write_source(
            &dir,
            "A.cpp",
            "#include <vector>\n#include \"../vendor/Vendor.h\"\n",
        );

        let graph = build_graph(&dir, "A.cpp", &options()).expect("build graph");
        assert_eq!(graph.nodes.len(), 1);

        let mut opts = options();
        opts.include_system = true;
        opts.include_external = true;
        let graph = build_graph(&dir, "A.cpp", &opts).expect("build graph");

        assert!(graph.nodes.iter().any(|node| node.id == "system:vector"));
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| node.id == "external:../vendor/Vendor.h")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reverse_graph_finds_includers() {
        let dir = temp_project_dir();
        write_source(&dir, "A.cpp", "#include \"B.h\"\n");
        write_source(&dir, "C.cpp", "#include \"B.h\"\n");
        write_source(&dir, "B.h", "");

        let mut opts = options();
        opts.reverse = true;
        let graph = build_graph(&dir, "B.h", &opts).expect("build graph");

        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.from == "A.cpp" && edge.to == "B.h")
        );
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.from == "C.cpp" && edge.to == "B.h")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mermaid_and_dot_render_edges_once() {
        let graph = DependencyGraph {
            root: "A.cpp".to_string(),
            nodes: vec![
                GraphNode {
                    id: "A.cpp".to_string(),
                    label: "A.cpp".to_string(),
                    kind: FileKind::Source,
                },
                GraphNode {
                    id: "B.h".to_string(),
                    label: "B.h".to_string(),
                    kind: FileKind::Header,
                },
            ],
            edges: vec![GraphEdge {
                from: "A.cpp".to_string(),
                to: "B.h".to_string(),
                kind: EdgeKind::Include,
            }],
        };

        assert!(render_mermaid(&graph).contains("A.cpp"));
        assert!(render_dot(&graph).contains("\"A.cpp\" -> \"B.h\""));
    }

    #[test]
    fn graphviz_missing_error_is_actionable() {
        let message = graphviz_missing_message();
        assert!(message.contains("Graphviz"));
        assert!(message.contains("--graphviz-path"));
    }
}

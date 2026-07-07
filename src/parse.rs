//! Tolerant, regex-based extraction of includes and symbol candidates.
//! Deliberately not a full C++ parser: the goal is "good enough candidates
//! with line numbers" so an AI agent knows where to look.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Symbol {
    pub kind: String, // class | struct | method | function | property
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_candidates: Option<Vec<String>>,
    pub line: u32,
    /// Last line of the symbol's extent: closing brace of a body, or the `;`
    /// of a declaration. None when the extent could not be determined.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub end_line: Option<u32>,
}

pub struct ParsedSource {
    pub includes: Vec<String>, // raw include targets as written
    pub symbols: Vec<Symbol>,
    pub has_dfm_pragma: bool,
    pub has_vcl_app_init: bool,
}

// Group 1 is the opening delimiter (`"` or `<`) so the bracket style is kept:
// angle includes are system/external, quoted includes are project-local.
static INCLUDE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*#\s*include\s*(["<])([^">]+)[">]"#).unwrap());

// `class PACKAGE TMainForm : public TForm {` — optional uppercase macro between
// keyword and name; forward declarations don't match (a `{` is required).
static CLASS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(class|struct)\s+(?:[A-Z_][A-Za-z0-9_]*\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*(:[^{;]+?)?\s*\{")
        .unwrap()
});

// `void __fastcall TMainForm::ButtonSaveClick(` — out-of-class definitions.
static QUALIFIED_DEF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\s*::\s*(~?[A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap()
});

// Top-level free functions: require some return-type-ish tokens before the name
// at the start of a line (`int WINAPI WinMain(`, `int main(`).
static FREE_FUNC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^[ \t]*(?:[A-Za-z_][A-Za-z0-9_<>:\*&, \t]*?[ \t\*&])(~?[A-Za-z_][A-Za-z0-9_]*)\s*\(",
    )
    .unwrap()
});

static METHOD_CANDIDATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(~?[A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap());

static ACCESS_LABEL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:public|private|protected|__published|__automated)\s*:").unwrap()
});

const NON_METHOD_NAMES: &[&str] = &[
    "if", "for", "while", "switch", "return", "sizeof", "catch", "throw", "new", "delete",
    "operator", "defined", "void", "int", "char", "bool", "float", "double", "long", "short",
    "unsigned", "signed", "const", "static", "virtual", "inline", "typedef", "using", "else",
    "case", "do", "goto", "class", "struct", "enum", "union", "template", "typename",
];

pub fn parse_source(raw: &str) -> ParsedSource {
    // Angle-bracket includes are stored with their brackets (`<windows.h>`) so
    // the scanner and graph can tell system/external includes from quoted,
    // project-local ones. Quoted includes are stored bare so path resolution
    // can match them against project files.
    let includes: Vec<String> = INCLUDE_RE
        .captures_iter(raw)
        .map(|c| {
            let target = c[2].trim();
            if &c[1] == "<" {
                format!("<{target}>")
            } else {
                target.to_string()
            }
        })
        .collect();
    let has_dfm_pragma = raw.contains("#pragma resource") && raw.contains(".dfm");
    let has_vcl_app_init =
        raw.contains("Application->Initialize") || raw.contains("Application->CreateForm");

    let text = strip_comments_and_strings(raw);
    let lines = LineIndex::new(&text);
    let mut symbols: Vec<Symbol> = Vec::new();

    collect_classes(&text, &lines, &mut symbols);
    collect_qualified_definitions(&text, &lines, &mut symbols);
    collect_free_functions(&text, &lines, &mut symbols);

    symbols.sort_by_key(|s| s.line);
    ParsedSource {
        includes,
        symbols,
        has_dfm_pragma,
        has_vcl_app_init,
    }
}

/// Replace comment bodies and string/char literal contents with spaces,
/// preserving newlines and byte offsets so line numbers stay valid.
pub fn strip_comments_and_strings(src: &str) -> String {
    #[derive(PartialEq)]
    enum St {
        Code,
        LineComment,
        BlockComment,
        Str,
        Chr,
    }
    let mut out = String::with_capacity(src.len());
    let mut st = St::Code;
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match st {
            St::Code => match c {
                '/' if chars.peek() == Some(&'/') => {
                    chars.next();
                    out.push_str("  ");
                    st = St::LineComment;
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    out.push_str("  ");
                    st = St::BlockComment;
                }
                '"' => {
                    out.push('"');
                    st = St::Str;
                }
                '\'' => {
                    out.push('\'');
                    st = St::Chr;
                }
                _ => out.push(c),
            },
            St::LineComment => {
                if c == '\n' {
                    out.push('\n');
                    st = St::Code;
                } else {
                    push_blank(&mut out, c);
                }
            }
            St::BlockComment => {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    out.push_str("  ");
                    st = St::Code;
                } else if c == '\n' {
                    out.push('\n');
                } else {
                    push_blank(&mut out, c);
                }
            }
            St::Str | St::Chr => {
                let quote = if st == St::Str { '"' } else { '\'' };
                if c == '\\' {
                    push_blank(&mut out, c);
                    if let Some(n) = chars.next() {
                        if n == '\n' {
                            out.push('\n');
                        } else {
                            push_blank(&mut out, n);
                        }
                    }
                } else if c == quote {
                    out.push(quote);
                    st = St::Code;
                } else if c == '\n' {
                    // unterminated literal on this line; recover
                    out.push('\n');
                    st = St::Code;
                } else {
                    push_blank(&mut out, c);
                }
            }
        }
    }
    out
}

fn push_blank(out: &mut String, c: char) {
    for _ in 0..c.len_utf8() {
        out.push(' ');
    }
}

struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        LineIndex { starts }
    }
    fn line_of(&self, offset: usize) -> u32 {
        match self.starts.binary_search(&offset) {
            Ok(i) => (i + 1) as u32,
            Err(i) => i as u32,
        }
    }
}

fn find_matching_brace(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Brace depth at each of the given (sorted) offsets.
fn depths_at(text: &str, offsets: &[usize]) -> Vec<i32> {
    let mut result = Vec::with_capacity(offsets.len());
    let mut depth = 0i32;
    let mut it = offsets.iter().peekable();
    for (i, b) in text.bytes().enumerate() {
        while let Some(&&off) = it.peek() {
            if off == i {
                result.push(depth);
                it.next();
            } else {
                break;
            }
        }
        match b {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
    }
    while result.len() < offsets.len() {
        result.push(depth);
    }
    result
}

fn parse_base_candidates(spec: &str) -> Vec<String> {
    spec.trim_start_matches(':')
        .split(',')
        .filter_map(|part| {
            let cleaned: String = part
                .split_whitespace()
                .filter(|w| !matches!(*w, "public" | "protected" | "private" | "virtual"))
                .collect::<Vec<_>>()
                .join(" ");
            let cleaned = cleaned.trim().to_string();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        })
        .collect()
}

fn collect_classes(text: &str, lines: &LineIndex, symbols: &mut Vec<Symbol>) {
    for cap in CLASS_RE.captures_iter(text) {
        let whole = cap.get(0).unwrap();
        let kind = cap[1].to_string();
        let name = cap[2].to_string();
        let bases = cap.get(3).map(|m| parse_base_candidates(m.as_str()));
        let bases = match bases {
            Some(v) if !v.is_empty() => Some(v),
            _ => None,
        };
        let open = whole.end() - 1;
        let close = find_matching_brace(text, open);
        symbols.push(Symbol {
            kind,
            name: name.clone(),
            owner: None,
            base_candidates: bases,
            line: lines.line_of(whole.start()),
            end_line: close.map(|c| lines.line_of(c)),
        });

        // Members declared directly in the class body (depth 1 inside the braces).
        if let Some(close) = close {
            collect_members(&text[open + 1..close], open + 1, &name, lines, symbols);
        }
    }
}

fn collect_members(
    body: &str,
    body_offset: usize,
    owner: &str,
    lines: &LineIndex,
    symbols: &mut Vec<Symbol>,
) {
    let mut depth = 0i32;
    let mut stmt = String::new();
    // offset of stmt's first byte inside `body`; stmt mirrors the source 1:1
    let mut stmt_begin: Option<usize> = None;
    // symbol whose inline body is open; its end_line is set at the closing brace
    let mut pending_inline: Option<usize> = None;
    for (i, c) in body.char_indices() {
        if depth == 0 {
            match c {
                ';' => {
                    process_member(
                        &stmt,
                        stmt_begin.map(|s| body_offset + s),
                        body_offset + i,
                        owner,
                        lines,
                        symbols,
                    );
                    stmt.clear();
                    stmt_begin = None;
                }
                '{' => {
                    // inline method body / nested type body: statement header ends here
                    pending_inline = process_member(
                        &stmt,
                        stmt_begin.map(|s| body_offset + s),
                        body_offset + i,
                        owner,
                        lines,
                        symbols,
                    );
                    stmt.clear();
                    stmt_begin = None;
                    depth += 1;
                }
                _ => {
                    if stmt.is_empty() {
                        stmt_begin = Some(i);
                    }
                    stmt.push(c);
                }
            }
        } else {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0
                        && let Some(k) = pending_inline.take()
                    {
                        symbols[k].end_line = Some(lines.line_of(body_offset + i));
                    }
                }
                _ => {}
            }
        }
    }
}

/// Returns the index of the symbol pushed, if any, so an inline body's
/// closing brace can overwrite `end_line`.
/// `begin` is the offset of `stmt`'s first byte in the source text; the
/// symbol line is taken from the declaration itself, after any leading
/// access label (`private: void Foo();` reports Foo's line, not the label's).
fn process_member(
    stmt: &str,
    begin: Option<usize>,
    end: usize,
    owner: &str,
    lines: &LineIndex,
    symbols: &mut Vec<Symbol>,
) -> Option<usize> {
    let begin = begin?;
    let mut s = stmt.trim();
    // strip access labels possibly glued to the front: `__published: void ...`
    while let Some(m) = ACCESS_LABEL_RE.find(s) {
        s = s[m.end()..].trim_start();
    }
    // where the actual declaration starts, as an offset in the source text
    let start = begin + (s.as_ptr() as usize - stmt.as_ptr() as usize);
    if s.is_empty() || s.starts_with('#') {
        return None;
    }
    let first_word = s
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or("");
    if matches!(
        first_word,
        "friend" | "typedef" | "using" | "enum" | "class" | "struct" | "union"
    ) {
        return None;
    }

    if s.starts_with("__property") {
        // `__property String Name = {read=FName, write=FName};`
        let head = s.split('=').next().unwrap_or(s);
        let head = head.trim_end();
        let head = head.trim_end_matches(|c: char| c == ']' || c == '[' || c.is_whitespace());
        // also drop an index spec like `Items[int i]`
        let head = match head.find('[') {
            Some(p) => head[..p].trim_end(),
            None => head,
        };
        if let Some(name) = head
            .rsplit(|c: char| !c.is_alphanumeric() && c != '_')
            .next()
            && !name.is_empty()
        {
            symbols.push(Symbol {
                kind: "property".into(),
                name: name.to_string(),
                owner: Some(owner.to_string()),
                base_candidates: None,
                line: lines.line_of(start),
                end_line: Some(lines.line_of(end)),
            });
            return Some(symbols.len() - 1);
        }
        return None;
    }

    for cap in METHOD_CANDIDATE_RE.captures_iter(s) {
        let m = cap.get(1).unwrap();
        let name = m.as_str();
        let bare = name.trim_start_matches('~');
        if NON_METHOD_NAMES.contains(&bare) {
            continue;
        }
        // `int x = init();` is a field, not a method
        if s[..m.start()].contains('=') {
            return None;
        }
        symbols.push(Symbol {
            kind: "method".into(),
            name: name.to_string(),
            owner: Some(owner.to_string()),
            base_candidates: None,
            line: lines.line_of(start),
            end_line: Some(lines.line_of(end)),
        });
        return Some(symbols.len() - 1);
    }
    None
}

/// After the parameter list's `)`, find the `{` opening a body before any `;`.
/// Some(brace offset) means this is a definition; None means a declaration.
fn body_open(text: &str, paren_open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut i = paren_open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    while i < bytes.len() {
        match bytes[i] {
            b'{' => return Some(i),
            b';' => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

fn collect_qualified_definitions(text: &str, lines: &LineIndex, symbols: &mut Vec<Symbol>) {
    let matches: Vec<_> = QUALIFIED_DEF_RE.captures_iter(text).collect();
    let offsets: Vec<usize> = matches.iter().map(|c| c.get(0).unwrap().start()).collect();
    let depths = depths_at(text, &offsets);
    for (cap, depth) in matches.iter().zip(depths) {
        if depth != 0 {
            continue;
        }
        let whole = cap.get(0).unwrap();
        let paren = whole.end() - 1;
        let Some(open) = body_open(text, paren) else {
            continue;
        };
        symbols.push(Symbol {
            kind: "method".into(),
            name: cap[2].to_string(),
            owner: Some(cap[1].to_string()),
            base_candidates: None,
            line: lines.line_of(whole.start()),
            end_line: find_matching_brace(text, open).map(|c| lines.line_of(c)),
        });
    }
}

fn collect_free_functions(text: &str, lines: &LineIndex, symbols: &mut Vec<Symbol>) {
    let matches: Vec<_> = FREE_FUNC_RE.captures_iter(text).collect();
    let offsets: Vec<usize> = matches.iter().map(|c| c.get(0).unwrap().start()).collect();
    let depths = depths_at(text, &offsets);
    for (cap, depth) in matches.iter().zip(depths) {
        if depth != 0 {
            continue;
        }
        let whole = cap.get(0).unwrap();
        if whole.as_str().contains("::") {
            continue; // handled by collect_qualified_definitions
        }
        let name = cap[1].to_string();
        if NON_METHOD_NAMES.contains(&name.trim_start_matches('~')) {
            continue;
        }
        let paren = whole.end() - 1;
        let Some(open) = body_open(text, paren) else {
            continue;
        };
        symbols.push(Symbol {
            kind: "function".into(),
            name,
            owner: None,
            base_candidates: None,
            line: lines.line_of(whole.start()),
            end_line: find_matching_brace(text, open).map(|c| lines.line_of(c)),
        });
    }
}

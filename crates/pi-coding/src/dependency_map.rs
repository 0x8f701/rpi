//! Pure source dependency extraction for the explorer dependency map.
//!
//! `extract_dependencies(path, source)` is a pure function over one file's
//! text: no filesystem, no workspace walking, no RPC. Rust / TypeScript / TSX
//! / JavaScript are parsed with the already-compiled tree-sitter grammars
//! (same `ENABLED_LANGS` subset as `tools::ast_grep`); Solidity is tokenized
//! with a small lexical scanner because `ast-grep-language`'s Solidity parser
//! is an `unimplemented!()` stub with the `tree-sitter-solidity` feature off.
//! Any other extension is reported as [`MapLanguage::Unsupported`] with no
//! guessed edges.
//!
//! A tree-sitter ERROR or missing node, an undecodable specifier escape, or a
//! malformed Solidity import reports [`ParseStatus::Incomplete`] and publishes
//! nothing: a partial factual graph is never emitted.

use std::path::Path;

use ast_grep_core::tree_sitter::StrDoc;
use ast_grep_core::Node;
use ast_grep_language::{LanguageExt, SupportLang};
use serde::Serialize;

/// Language inferred purely from the requested path's extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MapLanguage {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Solidity,
    Unsupported,
}

/// Parse outcome for the single source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseStatus {
    Ok,
    Incomplete,
    Unsupported,
}

/// Static dependency edge kind. Dynamic `import()` / `require()` calls are
/// never edges; they are counted in [`SourceDependencies::dynamic_omitted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// ESM/CJS `import … from "spec"` (including side-effect imports and
    /// TS `import x = require("spec")`).
    Import,
    /// `export … from "spec"`.
    ExportFrom,
    /// Rust `use …` declaration; `specifier` is the raw use-path text.
    Use,
    /// Rust `mod name;` (no body); `specifier` is the module name.
    Mod,
    /// Solidity `import … "spec";` in one of its four validated forms:
    /// `import "P";`, `import "P" as I;`, `import * as I from "P";`,
    /// or `import { A as B, C } from "P";`.
    SolImport,
}

/// One static dependency edge. Line is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DependencyEdge {
    pub kind: EdgeKind,
    pub specifier: String,
    pub line: usize,
}

/// One top-level declaration with its source-backed doc comment. Markers are
/// stripped and one shared leading indent is removed; the remaining text is
/// verbatim. Doc is attached only when the comment immediately precedes the
/// declaration, with attributes allowed in between. Line is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeclarationEntry {
    /// Grammar-backed declaration kind (`fn`, `struct`, `contract`, …).
    pub kind: String,
    /// Declared identifier; `None` for Rust `impl` blocks.
    pub name: Option<String>,
    pub line: usize,
    pub doc: Option<String>,
}

/// Extraction result for one source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceDependencies {
    pub language: MapLanguage,
    pub parse: ParseStatus,
    /// Empty when `parse` is not [`ParseStatus::Ok`].
    pub edges: Vec<DependencyEdge>,
    /// Empty when `parse` is not [`ParseStatus::Ok`].
    pub declarations: Vec<DeclarationEntry>,
    /// Dynamic `import()` / `require()` calls seen but not resolved as edges.
    pub dynamic_omitted: usize,
}

/// Extracts static dependencies and top-level declarations from one file's
/// source text. Pure: the path is used only for language inference.
#[must_use]
pub fn extract_dependencies(path: &str, source: &str) -> SourceDependencies {
    match language_for_path(path) {
        MapLanguage::Unsupported => empty(MapLanguage::Unsupported, ParseStatus::Unsupported),
        MapLanguage::Solidity => extract_solidity(source),
        language => extract_ast(language, source),
    }
}

fn empty(language: MapLanguage, parse: ParseStatus) -> SourceDependencies {
    SourceDependencies {
        language,
        parse,
        edges: Vec::new(),
        declarations: Vec::new(),
        dynamic_omitted: 0,
    }
}

/// Extension → language. Inference is exact-match on the lowercased file name;
/// no content sniffing, no fallback guesses.
#[must_use]
fn language_for_path(path: &str) -> MapLanguage {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if name.ends_with(".rs") {
        MapLanguage::Rust
    } else if name.ends_with(".d.ts")
        || name.ends_with(".ts")
        || name.ends_with(".mts")
        || name.ends_with(".cts")
    {
        MapLanguage::TypeScript
    } else if name.ends_with(".tsx") {
        MapLanguage::Tsx
    } else if name.ends_with(".js")
        || name.ends_with(".mjs")
        || name.ends_with(".cjs")
        || name.ends_with(".jsx")
    {
        MapLanguage::JavaScript
    } else if name.ends_with(".sol") {
        MapLanguage::Solidity
    } else {
        MapLanguage::Unsupported
    }
}

/// Concrete node type produced by `SupportLang::ast_grep`.
type TsNode<'a> = Node<'a, StrDoc<SupportLang>>;

/// AST extraction for the tree-sitter-backed languages.
fn extract_ast(language: MapLanguage, source: &str) -> SourceDependencies {
    let support = match language {
        MapLanguage::Rust => SupportLang::Rust,
        MapLanguage::TypeScript => SupportLang::TypeScript,
        MapLanguage::Tsx => SupportLang::Tsx,
        MapLanguage::JavaScript => SupportLang::JavaScript,
        MapLanguage::Solidity | MapLanguage::Unsupported => unreachable!("handled by caller"),
    };
    let grep = support.ast_grep(source);
    let root = grep.root();
    let mut edges = Vec::new();
    let mut dynamic_omitted = 0usize;
    // Iterative preorder: nesting depth must not consume native stack frames.
    for node in root.dfs() {
        if node.is_error() || node.is_missing() {
            return empty(language, ParseStatus::Incomplete);
        }
        match node.kind().as_ref() {
            "import_statement" => {
                let Some(source) = module_source(&node) else {
                    return empty(language, ParseStatus::Incomplete);
                };
                edges.push(DependencyEdge {
                    kind: EdgeKind::Import,
                    specifier: source,
                    line: node.start_pos().line() + 1,
                });
            }
            "export_statement" => {
                if node.field("source").is_some() {
                    let Some(source) = module_source(&node) else {
                        return empty(language, ParseStatus::Incomplete);
                    };
                    edges.push(DependencyEdge {
                        kind: EdgeKind::ExportFrom,
                        specifier: source,
                        line: node.start_pos().line() + 1,
                    });
                }
            }
            "use_declaration" => {
                if let Some(argument) = node.field("argument") {
                    edges.push(DependencyEdge {
                        kind: EdgeKind::Use,
                        specifier: argument.text().into_owned(),
                        line: node.start_pos().line() + 1,
                    });
                }
            }
            // `mod name;` is a file edge candidate; `mod name { … }` is not.
            "mod_item" if node.field("body").is_none() => {
                if let Some(name) = node.field("name") {
                    edges.push(DependencyEdge {
                        kind: EdgeKind::Mod,
                        specifier: name.text().into_owned(),
                        line: node.start_pos().line() + 1,
                    });
                }
            }
            "call_expression" => {
                if let Some(function) = node.field("function") {
                    if function.kind() == "import" || function.text() == "require" {
                        dynamic_omitted += 1;
                    }
                }
            }
            _ => {}
        }
    }
    SourceDependencies {
        language,
        parse: ParseStatus::Ok,
        edges,
        declarations: top_level_declarations(&root, language),
        dynamic_omitted,
    }
}

/// Decoded `source` of an import/export, including TS `import x = require(...)`.
fn module_source(node: &TsNode<'_>) -> Option<String> {
    if let Some(source) = node.field("source") {
        return decode_js_string(&source.text());
    }
    let clause = node
        .children()
        .find(|child| child.kind() == "import_require_clause")?;
    decode_js_string(&clause.field("source")?.text())
}

/// Decodes one JS/TS string literal. An unknown or truncated escape rejects
/// the whole file instead of publishing the raw escape as a dependency target.
fn decode_js_string(text: &str) -> Option<String> {
    let bytes = text.trim().as_bytes();
    if bytes.len() < 2 || bytes[0] != bytes[bytes.len() - 1] {
        return None;
    }
    let quote = bytes[0];
    if quote != b'"' && quote != 39 && quote != b'`' {
        return None;
    }
    let body = &bytes[1..bytes.len() - 1];
    let mut out = String::new();
    let mut i = 0;
    while i < body.len() {
        if body[i] != b'\\' {
            let rest = std::str::from_utf8(&body[i..]).ok()?;
            let ch = rest.chars().next()?;
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let escaped = *body.get(i + 1)?;
        i += 2;
        match escaped {
            b'\\' => out.push('\\'),
            b'\'' => out.push('\''),
            b'"' => out.push('"'),
            b'`' => out.push('`'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000c}'),
            b'v' => out.push('\u{000b}'),
            b'0' => out.push('\0'),
            b'x' => out.push(char::from_u32(read_hex(body, &mut i, 2)?)?),
            b'u' => out.push(decode_js_unicode(body, &mut i)?),
            b'\r' | b'\n' => {
                if escaped == b'\r' && body.get(i) == Some(&b'\n') {
                    i += 1;
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

fn decode_js_unicode(body: &[u8], i: &mut usize) -> Option<char> {
    if body.get(*i) == Some(&b'{') {
        *i += 1;
        let start = *i;
        while body.get(*i).is_some_and(u8::is_ascii_hexdigit) {
            *i += 1;
        }
        if *i == start || *i - start > 6 || body.get(*i) != Some(&b'}') {
            return None;
        }
        let value = u32::from_str_radix(std::str::from_utf8(&body[start..*i]).ok()?, 16).ok()?;
        *i += 1;
        return char::from_u32(value);
    }
    let unit = read_hex(body, i, 4)?;
    if (0xD800..=0xDBFF).contains(&unit) {
        if body.get(*i) != Some(&b'\\') || body.get(*i + 1) != Some(&b'u') {
            return None;
        }
        *i += 2;
        let low = read_hex(body, i, 4)?;
        if !(0xDC00..=0xDFFF).contains(&low) {
            return None;
        }
        return char::from_u32(0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00));
    }
    if (0xDC00..=0xDFFF).contains(&unit) {
        return None;
    }
    char::from_u32(unit)
}

fn read_hex(body: &[u8], i: &mut usize, digits: usize) -> Option<u32> {
    let slice = body.get(*i..*i + digits)?;
    if !slice.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let value = u32::from_str_radix(std::str::from_utf8(slice).ok()?, 16).ok()?;
    *i += digits;
    Some(value)
}

/// Top-level declarations with immediately preceding doc comments. Comments
/// are tree-sitter "extra" nodes surfaced as root children. Consecutive docs
/// accumulate; attributes and decorators keep the pending doc.
fn top_level_declarations(root: &TsNode<'_>, language: MapLanguage) -> Vec<DeclarationEntry> {
    let mut out = Vec::new();
    let mut pending_doc = String::new();
    for child in root.children() {
        let kind = child.kind();
        if kind == "comment" || kind == "line_comment" || kind == "block_comment" {
            if let Some(doc) = doc_from_comment(language, &child.text()) {
                if !pending_doc.is_empty() {
                    pending_doc.push('\n');
                }
                pending_doc.push_str(&doc);
            } else {
                pending_doc.clear();
            }
            continue;
        }
        if kind == "attribute_item" || kind == "inner_attribute_item" || kind == "decorator" {
            continue;
        }
        let declared = if kind == "export_statement" {
            child.field("declaration").as_ref().and_then(declaration_name)
        } else {
            declaration_name(&child)
        };
        if let Some((decl_kind, name)) = declared {
            out.push(DeclarationEntry {
                kind: decl_kind,
                name,
                line: child.start_pos().line() + 1,
                doc: (!pending_doc.is_empty()).then(|| std::mem::take(&mut pending_doc)),
            });
        }
        pending_doc.clear();
    }
    out
}

/// `(kind label, name)` for one declaration node.
fn declaration_name(node: &TsNode<'_>) -> Option<(String, Option<String>)> {
    let (kind, named): (&str, bool) = match node.kind().as_ref() {
        "function_item" => ("fn", true),
        "struct_item" => ("struct", true),
        "enum_item" => ("enum", true),
        "union_item" => ("union", true),
        "trait_item" => ("trait", true),
        "type_item" => ("type", true),
        "const_item" => ("const", true),
        "static_item" => ("static", true),
        "mod_item" => ("mod", true),
        "macro_definition" => ("macro", true),
        "impl_item" => ("impl", false),
        "function_declaration" | "generator_function_declaration" => ("function", true),
        "class_declaration" | "abstract_class_declaration" => ("class", true),
        "interface_declaration" => ("interface", true),
        "type_alias_declaration" => ("type", true),
        "enum_declaration" => ("enum", true),
        "lexical_declaration" | "variable_declaration" => ("variable", true),
        _ => return None,
    };
    let name = if named {
        node.field("name")
            .map(|name| name.text().into_owned())
            .or_else(|| {
                node.children()
                    .find(|child| child.kind() == "variable_declarator")
                    .and_then(|declarator| declarator.field("name"))
                    .map(|name| name.text().into_owned())
            })
    } else {
        None
    };
    Some((kind.to_string(), name))
}

/// Strips comment markers from a doc comment. Rust attaches only `///` and
/// `/** */` (rustdoc); TS/JS also attach plain `//` lines; Solidity attaches
/// `///` natspec and `/** */`. Returns the verbatim inner text.
fn doc_from_comment(language: MapLanguage, raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if language == MapLanguage::Rust {
        if let Some(inner) = trimmed.strip_prefix("///") {
            return Some(inner.trim().to_string());
        }
        if trimmed.starts_with("/**") {
            return Some(strip_block_doc(trimmed));
        }
        return None;
    }
    if trimmed.starts_with("/**") {
        return Some(strip_block_doc(trimmed));
    }
    if trimmed.starts_with("//") {
        return Some(trimmed.trim_start_matches('/').trim().to_string());
    }
    None
}

fn strip_block_doc(comment: &str) -> String {
    let inner = comment
        .strip_prefix("/**")
        .and_then(|rest| rest.strip_suffix("*/"))
        .unwrap_or(comment);
    let lines: Vec<&str> = inner.lines().collect();
    let body = if lines.len() <= 1 {
        lines.first().copied().unwrap_or("").trim().trim_start_matches('*').trim().to_string()
    } else {
        let content: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let shared = content
            .iter()
            .map(|line| line.chars().take_while(|ch| *ch == ' ').count())
            .min()
            .unwrap_or(0);
        content
            .iter()
            .map(|line| {
                let stripped = line.get(shared..).unwrap_or(line).trim_end();
                stripped.strip_prefix("* ").or_else(|| stripped.strip_prefix('*')).unwrap_or(stripped)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    body.trim().to_string()
}

// ---------------------------------------------------------------------------
// Solidity lexical extraction
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum SolMode {
    Code,
    LineComment,
    BlockComment,
    Str(u8),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SolTok {
    Ident,
    String,
    Star,
    Comma,
    LBrace,
    RBrace,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImportState {
    Start,
    Path,
    PathAs,
    PathAlias,
    Star,
    StarAs,
    StarAlias,
    StarFrom,
    From,
    Brace,
    BraceName,
    BraceAs,
    BraceAlias,
    BraceSep,
    BraceFrom,
}

/// Lexical Solidity scan. Comments and strings are skipped before any import
/// decision; an import is accepted only when its tokens match one of:
/// `import "P";`, `import "P" as I;`, `import * as I from "P";`, or
/// `import { A as B, C } from "P";`. A default import (`import I from "P";`)
/// is not Solidity and is incomplete. Any other token sequence is incomplete.
/// EOF validly ends a line comment; an open block comment, string, or import,
/// or a nonzero brace depth, rejects the file.
fn extract_solidity(source: &str) -> SourceDependencies {
    let bytes = source.as_bytes();
    let mut mode = SolMode::Code;
    let mut i = 0usize;
    let mut line = 1usize;
    let mut edges = Vec::new();
    let mut declarations = Vec::new();
    let mut depth = 0i32;
    let mut import: Option<(ImportState, usize)> = None;
    let mut import_spec = String::new();
    let mut str_buf: Vec<u8> = Vec::new();
    let mut comment_buf: Vec<u8> = Vec::new();
    let mut comment_doc = false;
    let mut last_doc: Option<(String, usize)> = None;


    while i < bytes.len() {
        match mode {
            SolMode::Code => {
                let b = bytes[i];
                if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
                    mode = SolMode::LineComment;
                    comment_buf.clear();
                    i += 2;
                } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    mode = SolMode::BlockComment;
                    comment_buf.clear();
                    comment_doc = bytes.get(i + 2) == Some(&b'*');
                    i += 2;
                } else if b == b'"' || b == 39 {
                    mode = SolMode::Str(b);
                    str_buf.clear();
                    i += 1;
                } else if b == b'\n' {
                    line += 1;
                    i += 1;
                } else if matches!(b, b' ' | b'\t' | b'\r') {
                    i += 1;
                } else if is_sol_ident_start(b) {
                    let start = i;
                    while bytes.get(i).is_some_and(|byte| is_sol_ident_byte(*byte)) {
                        i += 1;
                    }
                    let ident = &source[start..i];
                    if (import.is_some() || depth == 0)
                        && feed_import(&mut import, &mut import_spec, SolTok::Ident, ident, line)
                    {
                        return empty(MapLanguage::Solidity, ParseStatus::Incomplete);
                    }
                    if import.is_some() {
                        last_doc = None;
                    } else if depth == 0
                        && matches!(
                            ident,
                            "contract"
                                | "interface"
                                | "library"
                                | "struct"
                                | "enum"
                                | "event"
                                | "error"
                                | "modifier"
                                | "function"
                        )
                    {
                        let mut j = i;
                        let mut skipped_lines = 0usize;
                        while matches!(bytes.get(j), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                            if bytes[j] == b'\n' {
                                skipped_lines += 1;
                            }
                            j += 1;
                        }
                        if bytes.get(j).is_some_and(|byte| is_sol_ident_start(*byte)) {
                            let name_start = j;
                            while bytes.get(j).is_some_and(|byte| is_sol_ident_byte(*byte)) {
                                j += 1;
                            }
                            let doc = last_doc
                                .take()
                                .filter(|(_, end_line)| line.saturating_sub(*end_line) <= 1)
                                .map(|(text, _)| text);
                            declarations.push(DeclarationEntry {
                                kind: ident.to_string(),
                                name: Some(source[name_start..j].to_string()),
                                line,
                                doc,
                            });
                            line += skipped_lines;
                            i = j;
                        } else if ident != "function" {
                            return empty(MapLanguage::Solidity, ParseStatus::Incomplete);
                        }
                    } else {
                        last_doc = None;
                    }

                } else {
                    let token = match b {
                        b'*' => SolTok::Star,
                        b',' => SolTok::Comma,
                        b'{' => {
                            depth += 1;
                            SolTok::LBrace
                        }
                        b'}' => {
                            depth -= 1;
                            if depth < 0 {
                                return empty(MapLanguage::Solidity, ParseStatus::Incomplete);
                            }
                            SolTok::RBrace
                        }
                        b';' => {
                            if let Some((state, edge_line)) = import.take() {
                                if matches!(state, ImportState::Path | ImportState::PathAlias)
                                    && !import_spec.is_empty()
                                {
                                    edges.push(DependencyEdge {
                                        kind: EdgeKind::SolImport,
                                        specifier: std::mem::take(&mut import_spec),
                                        line: edge_line,
                                    });
                                } else {
                                    return empty(MapLanguage::Solidity, ParseStatus::Incomplete);
                                }
                            }
                            last_doc = None;
                            i += 1;
                            continue;
                        }
                        _ => SolTok::Other,
                    };
                    if (import.is_some() || depth == 0)
                        && feed_import(&mut import, &mut import_spec, token, "", line)
                    {
                        return empty(MapLanguage::Solidity, ParseStatus::Incomplete);
                    }
                    if token != SolTok::LBrace && token != SolTok::RBrace {
                        last_doc = None;
                    }
                    i += 1;
                }
            }
            SolMode::LineComment => {
                if bytes[i] == b'\n' {
                    store_line_doc(&comment_buf, line, &mut last_doc);
                    mode = SolMode::Code;
                    line += 1;
                } else {
                    comment_buf.push(bytes[i]);
                }
                i += 1;
            }
            SolMode::BlockComment => {
                if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    if comment_doc {
                        let body = comment_buf.strip_prefix(b"*").unwrap_or(comment_buf.as_slice());
                        if let Ok(text) = std::str::from_utf8(body) {
                            store_comment(&format!("/**{text}*/"), line, &mut last_doc);
                        }
                    }
                    mode = SolMode::Code;
                    i += 2;
                } else {
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    comment_buf.push(bytes[i]);
                    i += 1;
                }
            }
            SolMode::Str(quote) => {
                if bytes[i] == b'\\' {
                    let Some(escaped) = bytes.get(i + 1).copied() else {
                        return empty(MapLanguage::Solidity, ParseStatus::Incomplete);
                    };
                    let Some(decoded) = decode_sol_escape(escaped) else {
                        return empty(MapLanguage::Solidity, ParseStatus::Incomplete);
                    };
                    str_buf.extend(decoded);
                    i += 2;
                } else if bytes[i] == quote {
                    if import.is_some() {
                        let decoded = String::from_utf8_lossy(&str_buf).into_owned();
                        if feed_import(
                            &mut import,
                            &mut import_spec,
                            SolTok::String,
                            &decoded,
                            line,
                        ) {
                            return empty(MapLanguage::Solidity, ParseStatus::Incomplete);
                        }
                    }
                    last_doc = None;
                    mode = SolMode::Code;
                    i += 1;
                } else {
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    str_buf.push(bytes[i]);
                    i += 1;
                }
            }
        }
    }
    if mode == SolMode::LineComment {
        store_line_doc(&comment_buf, line, &mut last_doc);
        mode = SolMode::Code;
    }
    if mode != SolMode::Code || import.is_some() || depth != 0 {
        return empty(MapLanguage::Solidity, ParseStatus::Incomplete);
    }
    SourceDependencies {
        language: MapLanguage::Solidity,
        parse: ParseStatus::Ok,
        edges,
        declarations,
        dynamic_omitted: 0,
    }
}

fn store_line_doc(buf: &[u8], line: usize, last_doc: &mut Option<(String, usize)>) {
    let Ok(text) = std::str::from_utf8(buf) else {
        return;
    };
    store_comment(&format!("//{text}"), line, last_doc);
}

fn store_comment(raw: &str, line: usize, last_doc: &mut Option<(String, usize)>) {
    let Some(doc) = doc_from_comment(MapLanguage::Solidity, raw) else {
        return;
    };
    if let Some((previous, end_line)) = last_doc.as_mut() {
        if line.saturating_sub(*end_line) == 1 {
            previous.push('\n');
            previous.push_str(&doc);
            *end_line = line;
            return;
        }
    }
    *last_doc = Some((doc, line));
}

/// `true` when an open import form sees a token it cannot accept.
fn feed_import(
    import: &mut Option<(ImportState, usize)>,
    spec: &mut String,
    token: SolTok,
    text: &str,
    line: usize,
) -> bool {
    let Some((state, _)) = import.as_mut() else {
        if token == SolTok::Ident && text == "import" {
            *import = Some((ImportState::Start, line));
            spec.clear();
        }
        return false;
    };
    let next = match (*state, token, text) {
        (ImportState::Start, SolTok::String, _) => {
            spec.clear();
            spec.push_str(text);
            ImportState::Path
        }
        (ImportState::Path, SolTok::Ident, "as") => ImportState::PathAs,
        (ImportState::PathAs, SolTok::Ident, _) => ImportState::PathAlias,
        (ImportState::Start, SolTok::Star, _) => ImportState::Star,
        (ImportState::Start, SolTok::LBrace, _) => ImportState::Brace,
        (ImportState::Star, SolTok::Ident, "as") => ImportState::StarAs,
        (ImportState::StarAs, SolTok::Ident, _) => ImportState::StarAlias,
        (ImportState::StarAlias, SolTok::Ident, "from") => ImportState::StarFrom,
        (ImportState::StarFrom, SolTok::String, _)
        | (ImportState::From, SolTok::String, _) => {
            spec.clear();
            spec.push_str(text);
            ImportState::Path
        }
        (ImportState::Brace, SolTok::Ident, _) | (ImportState::BraceSep, SolTok::Ident, _) => {
            ImportState::BraceName
        }
        (ImportState::Brace, SolTok::RBrace, _) => ImportState::BraceFrom,
        (ImportState::BraceName, SolTok::Ident, "as") => ImportState::BraceAs,
        (ImportState::BraceAs, SolTok::Ident, _) => ImportState::BraceAlias,
        (ImportState::BraceName, SolTok::Comma, _) | (ImportState::BraceAlias, SolTok::Comma, _) => {
            ImportState::BraceSep
        }
        (ImportState::BraceName, SolTok::RBrace, _)
        | (ImportState::BraceAlias, SolTok::RBrace, _) => ImportState::BraceFrom,
        (ImportState::BraceFrom, SolTok::Ident, "from") => ImportState::From,
        _ => return true,
    };
    *state = next;
    false
}

fn decode_sol_escape(escaped: u8) -> Option<Vec<u8>> {
    Some(match escaped {
        b'\\' => vec![b'\\'],
        b'\'' => vec![b'\''],
        b'"' => vec![b'"'],
        b'n' => vec![b'\n'],
        b'r' => vec![b'\r'],
        b't' => vec![b'\t'],
        _ => return None,
    })
}

fn is_sol_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_sol_ident_byte(b: u8) -> bool {
    is_sol_ident_start(b) || b.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_import_forms_carry_exact_specifiers_and_lines() {
        let source = concat!(
            "import def, { named } from './a.ts';\n",
            "import * as ns from \"./b.ts\";\n",
            "import './side-effect.ts';\n",
            "import req = require('./c.ts');\n",
        );
        let result = extract_dependencies("m.ts", source);
        assert_eq!(result.parse, ParseStatus::Ok);
        assert_eq!(result.language, MapLanguage::TypeScript);
        let specs: Vec<_> = result
            .edges
            .iter()
            .map(|edge| (edge.kind, edge.specifier.as_str(), edge.line))
            .collect();
        assert_eq!(
            specs,
            vec![
                (EdgeKind::Import, "./a.ts", 1),
                (EdgeKind::Import, "./b.ts", 2),
                (EdgeKind::Import, "./side-effect.ts", 3),
                (EdgeKind::Import, "./c.ts", 4),
            ]
        );
        assert_eq!(result.dynamic_omitted, 0);
    }

    #[test]
    fn ts_comment_import_and_string_import_are_not_edges() {
        let source = concat!(
            "// import './commented.js';\n",
            "/* import './blocked.js'; */\n",
            "const fake = \"import './in-string.js'\";\n",
            "import real from './real.js';\n",
        );
        let result = extract_dependencies("m.js", source);
        assert_eq!(result.parse, ParseStatus::Ok);
        assert_eq!(result.edges.len(), 1);
        assert_eq!(result.edges[0].specifier, "./real.js");
        assert_eq!(result.edges[0].line, 4);
    }

    #[test]
    fn ts_dynamic_import_and_require_counted_not_edges() {
        let source = concat!(
            "export { x } from './reexport.js';\n",
            "async function load() {\n",
            "  await import('./dyn.js');\n",
            "  const r = require('./req.js');\n",
            "}\n",
        );
        let result = extract_dependencies("m.js", source);
        assert_eq!(result.parse, ParseStatus::Ok);
        assert_eq!(result.edges.len(), 1);
        assert_eq!(result.edges[0].kind, EdgeKind::ExportFrom);
        assert_eq!(result.edges[0].specifier, "./reexport.js");
        assert_eq!(result.dynamic_omitted, 2);
    }

    #[test]
    fn js_unicode_escape_decodes_and_unknown_escape_rejects() {
        let decoded = extract_dependencies("m.js", "import \"./\\u0061.js\";\n");
        assert_eq!(decoded.parse, ParseStatus::Ok);
        assert_eq!(decoded.edges[0].specifier, "./a.js");

        let rejected = extract_dependencies("m.js", "import \"./\\q.js\";\n");
        assert_eq!(rejected.parse, ParseStatus::Incomplete);
        assert!(rejected.edges.is_empty());
    }

    #[test]
    fn ts_malformed_source_is_incomplete_with_no_edges() {
        let result = extract_dependencies("m.js", "= = =\nimport ok from './ok.js';\n");
        assert_eq!(result.parse, ParseStatus::Incomplete);
        assert!(result.edges.is_empty());
        assert!(result.declarations.is_empty());
    }

    #[test]
    fn ts_missing_token_is_incomplete_with_no_edges() {
        let result = extract_dependencies("m.ts", "import from './missing.ts';\n");
        assert_eq!(result.parse, ParseStatus::Incomplete);
        assert!(result.edges.is_empty());
        assert!(result.declarations.is_empty());
    }

    #[test]
    fn ts_declaration_doc_and_exported_declaration_are_source_backed() {
        let source = concat!(
            "/** Adds one. */\n",
            "function addOne(x: number): number { return x + 1; }\n",
            "export class Counter {}\n",
        );
        let result = extract_dependencies("m.ts", source);
        assert_eq!(result.declarations.len(), 2);
        assert_eq!(result.declarations[0].kind, "function");
        assert_eq!(result.declarations[0].name.as_deref(), Some("addOne"));
        assert_eq!(result.declarations[0].doc.as_deref(), Some("Adds one."));
        assert_eq!(result.declarations[1].kind, "class");
        assert_eq!(result.declarations[1].name.as_deref(), Some("Counter"));
        assert_eq!(result.declarations[1].line, 3);
    }

    #[test]
    fn rust_mod_and_use_edges_and_inline_mod_distinction() {
        let source = concat!(
            "mod leaf;\n",
            "mod inline {\n",
            "    mod nested;\n",
            "}\n",
            "use crate::store::Session;\n",
            "use std::io::{Read, Write};\n",
            "pub use self::inner::Thing as Alias;\n",
        );
        let result = extract_dependencies("lib.rs", source);
        assert_eq!(result.parse, ParseStatus::Ok);
        let specs: Vec<_> = result
            .edges
            .iter()
            .map(|edge| (edge.kind, edge.specifier.as_str(), edge.line))
            .collect();
        assert_eq!(
            specs,
            vec![
                (EdgeKind::Mod, "leaf", 1),
                (EdgeKind::Mod, "nested", 3),
                (EdgeKind::Use, "crate::store::Session", 5),
                (EdgeKind::Use, "std::io::{Read, Write}", 6),
                (EdgeKind::Use, "self::inner::Thing as Alias", 7),
            ]
        );
    }

    #[test]
    fn rust_doc_survives_consecutive_lines_and_attributes() {
        let source = concat!(
            "/// Adds one.\n",
            "/// Returns the successor.\n",
            "#[inline]\n",
            "fn add_one(x: u64) -> u64 { x + 1 }\n",
        );
        let result = extract_dependencies("math.rs", source);
        assert_eq!(result.declarations.len(), 1);
        let declaration = &result.declarations[0];
        assert_eq!(declaration.kind, "fn");
        assert_eq!(declaration.name.as_deref(), Some("add_one"));
        assert_eq!(
            declaration.doc.as_deref(),
            Some("Adds one.\nReturns the successor.")
        );
        assert_eq!(declaration.line, 4);
    }

    #[test]
    fn rust_plain_comment_is_not_doc_evidence() {
        let result = extract_dependencies("m.rs", "// not rustdoc\nfn f() {}\n");
        assert_eq!(result.declarations[0].doc, None);
    }

    #[test]
    fn solidity_all_four_import_forms_capture_specifier_and_line() {
        let source = concat!(
            "// SPDX-License-Identifier: MIT\n",
            "pragma solidity ^0.8.20;\n",
            "\n",
            "import \"./plain.sol\";\n",
            "import * as Tokens from \"./star.sol\";\n",
            "import './B.sol' as B;\n",
            "import {A as B, C} from \"./braces.sol\";\n",
        );
        let result = extract_dependencies("Token.sol", source);
        assert_eq!(result.parse, ParseStatus::Ok);
        let specs: Vec<_> = result
            .edges
            .iter()
            .map(|edge| (edge.kind, edge.specifier.as_str(), edge.line))
            .collect();
        assert_eq!(
            specs,
            vec![
                (EdgeKind::SolImport, "./plain.sol", 4),
                (EdgeKind::SolImport, "./star.sol", 5),
                (EdgeKind::SolImport, "./B.sol", 6),
                (EdgeKind::SolImport, "./braces.sol", 7),
            ]
        );
    }

    #[test]
    fn solidity_default_import_is_incomplete_and_empty() {
        let source = "import \"./ok.sol\";\nimport B from './B.sol';\ncontract Kept {}\n";
        let result = extract_dependencies("A.sol", source);
        assert_eq!(result.parse, ParseStatus::Incomplete);
        assert!(result.edges.is_empty());
        assert!(result.declarations.is_empty());
    }

    #[test]
    fn solidity_unclosed_brace_at_eof_is_incomplete_and_empty() {
        let source = "import \"./B.sol\";\ncontract A {\n";
        let result = extract_dependencies("A.sol", source);
        assert_eq!(result.parse, ParseStatus::Incomplete);
        assert!(result.edges.is_empty());
        assert!(result.declarations.is_empty());
    }

    #[test]
    fn solidity_multiline_declaration_name_preserves_keyword_line() {
        let source = concat!(
            "contract\n",
            "A {}\n",
            "import \"./B.sol\";\n",
            "interface\n",
            "I {}\n",
        );
        let result = extract_dependencies("A.sol", source);
        assert_eq!(result.parse, ParseStatus::Ok);
        let declarations: Vec<_> = result
            .declarations
            .iter()
            .map(|declaration| {
                (
                    declaration.kind.as_str(),
                    declaration.name.as_deref(),
                    declaration.line,
                )
            })
            .collect();
        assert_eq!(
            declarations,
            vec![("contract", Some("A"), 1), ("interface", Some("I"), 4)]
        );
        assert_eq!(
            result
                .edges
                .iter()
                .map(|edge| (edge.kind, edge.specifier.as_str(), edge.line))
                .collect::<Vec<_>>(),
            vec![(EdgeKind::SolImport, "./B.sol", 3)]
        );
    }

    #[test]
    fn solidity_malformed_import_forms_are_incomplete() {
        for source in ["import nonsense \"./fake.sol\";\n", "import \"./a.sol\" \"./b.sol\";\n"] {
            let result = extract_dependencies("Token.sol", source);
            assert_eq!(result.parse, ParseStatus::Incomplete, "{source}");
            assert!(result.edges.is_empty(), "{source}");
        }
    }

    #[test]
    fn solidity_comment_and_string_imports_are_not_edges() {
        let source = concat!(
            "// import './commented.sol';\n",
            "/* import './blocked.sol'; */\n",
            "import \"./real.sol\";\n",
            "contract C { string s = \"import './in-string.sol'\"; }\n",
        );
        let result = extract_dependencies("Token.sol", source);
        assert_eq!(result.parse, ParseStatus::Ok);
        assert_eq!(result.edges.len(), 1);
        assert_eq!(result.edges[0].specifier, "./real.sol");
        assert_eq!(result.declarations.len(), 1);
        assert_eq!(result.declarations[0].name.as_deref(), Some("C"));
    }

    #[test]
    fn solidity_escaped_string_decodes_and_eof_line_comment_is_valid() {
        let source = "import \"./a\\n.sol\"; // trailing comment";
        let result = extract_dependencies("Token.sol", source);
        assert_eq!(result.parse, ParseStatus::Ok);
        assert_eq!(result.edges[0].specifier, "./a\n.sol");

        let rejected = extract_dependencies("Token.sol", "import \"./a\\q.sol\";\n");
        assert_eq!(rejected.parse, ParseStatus::Incomplete);
        assert!(rejected.edges.is_empty());
    }

    #[test]
    fn solidity_unclosed_string_comment_and_import_are_incomplete() {
        for source in [
            "import \"./a.sol\";\ncontract C { string s = \"unterminated;\n",
            "import \"./a.sol\";\n/* never closed\n",
            "import \"./a.sol\"\n",
        ] {
            let result = extract_dependencies("Token.sol", source);
            assert_eq!(result.parse, ParseStatus::Incomplete, "{source}");
            assert!(result.edges.is_empty(), "{source}");
            assert!(result.declarations.is_empty(), "{source}");
        }
    }

    #[test]
    fn solidity_contract_declaration_carries_natspec_doc() {
        let source = concat!(
            "/// @title Token supply ledger.\n",
            "contract TokenStore {\n",
            "    /// Member docs are not top-level.\n",
            "    function total() public pure returns (uint256) { return 0; }\n",
            "}\n",
        );
        let result = extract_dependencies("Token.sol", source);
        assert_eq!(result.parse, ParseStatus::Ok);
        assert_eq!(result.declarations.len(), 1);
        let declaration = &result.declarations[0];
        assert_eq!(declaration.kind, "contract");
        assert_eq!(declaration.name.as_deref(), Some("TokenStore"));
        assert_eq!(declaration.doc.as_deref(), Some("@title Token supply ledger."));
        assert_eq!(declaration.line, 2);
    }

    #[test]
    fn solidity_consecutive_multilingual_docs_accumulate() {
        let source = "/// \u{8d26}\u{672c}\n/// ledger\ncontract TokenStore {\n}\n";
        let result = extract_dependencies("Token.sol", source);
        assert_eq!(result.parse, ParseStatus::Ok);
        assert_eq!(result.declarations.len(), 1);
        assert_eq!(result.declarations[0].doc.as_deref(), Some("\u{8d26}\u{672c}\nledger"));
    }

    #[test]
    fn solidity_indented_top_level_declaration_keeps_doc() {
        let source = "    /// @title Ledger.\n    contract TokenStore {\n    }\n";
        let result = extract_dependencies("Token.sol", source);
        assert_eq!(result.declarations.len(), 1);
        assert_eq!(result.declarations[0].doc.as_deref(), Some("@title Ledger."));
    }

    #[test]
    fn unsupported_language_is_explicit_with_no_edges() {
        let result = extract_dependencies("script.py", "def hello():\n    return 'world'\n");
        assert_eq!(result.language, MapLanguage::Unsupported);
        assert_eq!(result.parse, ParseStatus::Unsupported);
        assert!(result.edges.is_empty());
        assert_eq!(result.dynamic_omitted, 0);
    }

    #[test]
    fn language_inference_covers_supported_extensions() {
        assert_eq!(language_for_path("a.rs"), MapLanguage::Rust);
        assert_eq!(language_for_path("a.d.ts"), MapLanguage::TypeScript);
        assert_eq!(language_for_path("a.mts"), MapLanguage::TypeScript);
        assert_eq!(language_for_path("a.tsx"), MapLanguage::Tsx);
        assert_eq!(language_for_path("a.mjs"), MapLanguage::JavaScript);
        assert_eq!(language_for_path("a.CJS"), MapLanguage::JavaScript);
        assert_eq!(language_for_path("a.SOL"), MapLanguage::Solidity);
        assert_eq!(language_for_path("a.py"), MapLanguage::Unsupported);
    }
}

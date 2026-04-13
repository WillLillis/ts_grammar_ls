use std::path::{Path, PathBuf};

use ropey::Rope;
use tower_lsp::lsp_types::Url;

use tree_sitter_generate::nativedsl::{self, ast};

use crate::document::{Analysis, DefKind, Definition, Document, RefKind, Reference};

// ---------------------------------------------------------------------------
// Analysis extraction - walk the AST to collect definitions and references
// ---------------------------------------------------------------------------

/// Extract definitions from an AST's root items.
fn extract_definitions(parsed_ast: &ast::Ast<'_>) -> Vec<Definition> {
    let mut definitions = Vec::new();
    for &item_id in &parsed_ast.root_items {
        match parsed_ast.node(item_id) {
            ast::Node::Rule { name, .. } => {
                let name_span = parsed_ast.span(*name);
                definitions.push(Definition {
                    name: parsed_ast.text(name_span).to_owned(),
                    kind: DefKind::Rule,
                    name_span,
                    full_span: parsed_ast.span(item_id),
                });
            }
            ast::Node::OverrideRule { name, .. } => {
                let name_span = parsed_ast.span(*name);
                definitions.push(Definition {
                    name: parsed_ast.text(name_span).to_owned(),
                    kind: DefKind::OverrideRule,
                    name_span,
                    full_span: parsed_ast.span(item_id),
                });
            }
            ast::Node::Fn(fn_idx) => {
                let config = parsed_ast.get_fn(*fn_idx);
                let name_span = parsed_ast.span(config.name);
                let fn_name = parsed_ast.text(name_span);
                let signature = build_fn_signature(parsed_ast, config, fn_name);
                let fn_span = parsed_ast.span(item_id);
                definitions.push(Definition {
                    name: fn_name.to_owned(),
                    kind: DefKind::Function { signature },
                    name_span,
                    full_span: fn_span,
                });
                // Extract parameters as scoped definitions.
                for param in &config.params {
                    let param_span = parsed_ast.span(param.name);
                    definitions.push(Definition {
                        name: parsed_ast.text(param_span).to_owned(),
                        kind: DefKind::Parameter { scope: fn_span },
                        name_span: param_span,
                        full_span: param_span,
                    });
                }
            }
            ast::Node::Let { name, value, .. } => {
                let name_span = parsed_ast.span(*name);
                let full_span = parsed_ast.span(item_id);
                definitions.push(Definition {
                    name: parsed_ast.text(name_span).to_owned(),
                    kind: DefKind::Let { scope: None },
                    name_span,
                    full_span,
                });
                // Extract object field keys as definitions.
                if let ast::Node::Object(range) = parsed_ast.node(*value) {
                    for &(key_span, _) in parsed_ast.context.get_object(*range) {
                        definitions.push(Definition {
                            name: parsed_ast.text(key_span).to_owned(),
                            kind: DefKind::ObjectKey,
                            name_span: key_span,
                            full_span: key_span,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    // Extract for-loop bindings from all nodes.
    for (i, node) in parsed_ast.nodes.iter().enumerate().skip(1) {
        if let ast::Node::For(for_id) = node {
            let for_span = parsed_ast.context.spans[i];
            let config = parsed_ast.get_for(*for_id);
            let scope = find_scope(for_span, parsed_ast).unwrap_or(for_span);
            for &(binding_span, _) in &config.bindings {
                definitions.push(Definition {
                    name: parsed_ast.text(binding_span).to_owned(),
                    kind: DefKind::Parameter { scope },
                    name_span: binding_span,
                    full_span: binding_span,
                });
            }
        }
    }

    definitions
}

/// Determine the narrowest enclosing scope (function or for-loop) for a span.
fn find_scope(span: ast::Span, parsed_ast: &ast::Ast<'_>) -> Option<ast::Span> {
    let mut best: Option<ast::Span> = None;
    // Check functions.
    for &item_id in &parsed_ast.root_items {
        if let ast::Node::Fn(_) = parsed_ast.node(item_id) {
            let fn_span = parsed_ast.span(item_id);
            if span.start >= fn_span.start && span.end <= fn_span.end {
                best = Some(fn_span);
            }
        }
    }
    // Check for-loops (may narrow the scope further).
    for (i, node) in parsed_ast.nodes.iter().enumerate().skip(1) {
        if let ast::Node::For(_) = node {
            let for_span = parsed_ast.context.spans[i];
            if span.start >= for_span.start && span.end <= for_span.end {
                match best {
                    Some(b) if for_span.start > b.start => best = Some(for_span),
                    None => best = Some(for_span),
                    _ => {}
                }
            }
        }
    }
    best
}

/// Extract resolved references from all nodes in the AST.
fn extract_references(parsed_ast: &ast::Ast<'_>) -> Vec<Reference> {
    let mut references = Vec::new();

    for (i, node) in parsed_ast.nodes.iter().enumerate().skip(1) {
        let id = ast::NodeId(std::num::NonZeroU32::new(i as u32).unwrap());
        let span = parsed_ast.span(id);
        match node {
            ast::Node::RuleRef => {
                references.push(Reference {
                    span,
                    kind: RefKind::Rule(parsed_ast.text(span).to_owned()),
                    scope: find_scope(span, parsed_ast),
                });
            }
            ast::Node::VarRef => {
                references.push(Reference {
                    span,
                    kind: RefKind::Variable(parsed_ast.text(span).to_owned()),
                    scope: find_scope(span, parsed_ast),
                });
            }
            // The `rule` child of RuleInline stays as Ident (not resolved),
            // so we extract it directly as a BaseRule reference.
            ast::Node::RuleInline { rule, .. } => {
                let rule_span = parsed_ast.span(*rule);
                references.push(Reference {
                    span: rule_span,
                    kind: RefKind::BaseRule(parsed_ast.text(rule_span).to_owned()),
                    scope: find_scope(rule_span, parsed_ast),
                });
            }
            // Field access: `obj.field` - extract the field as an ObjectField ref.
            ast::Node::FieldAccess { obj, field } => {
                let obj_span = parsed_ast.span(*obj);
                let field_span = parsed_ast.span(*field);
                references.push(Reference {
                    span: field_span,
                    kind: RefKind::ObjectField {
                        field: parsed_ast.text(field_span).to_owned(),
                        object: parsed_ast.text(obj_span).to_owned(),
                    },
                    scope: find_scope(field_span, parsed_ast),
                });
            }
            // inherit("path") - the path string literal.
            ast::Node::Inherit { path } => {
                let path_span = parsed_ast.span(*path);
                references.push(Reference {
                    span: path_span,
                    kind: RefKind::InheritPath,
                    scope: None,
                });
            }
            _ => {}
        }
    }

    references
}

/// Identifies a version of a source file - either an LSP document version
/// (for in-memory edits) or a filesystem mtime (for on-disk files).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceVersion {
    Document(i32),
    Disk(std::time::SystemTime),
}

#[derive(Clone)]
pub struct CachedBaseGrammar {
    pub version: SourceVersion,
    pub definitions: Vec<Definition>,
    pub references: Vec<Reference>,
    pub rope: Rope,
}

/// Cache of parsed base grammars, keyed by absolute path.
pub type BaseGrammarCache = dashmap::DashMap<PathBuf, CachedBaseGrammar>;

/// Context passed to `analyze()` to enable base grammar caching and
/// lookup of in-memory text for open documents.
pub struct AnalysisContext<'a> {
    pub base_cache: &'a BaseGrammarCache,
    pub document_map: &'a dashmap::DashMap<Url, Document>,
}

/// Parse and resolve a base grammar file to extract definitions, references, and a rope.
fn parse_base_grammar(
    path: &Path,
    content: &str,
) -> Option<(Vec<Definition>, Vec<Reference>, Rope)> {
    let tokens = nativedsl::lexer::Lexer::new(content).tokenize().ok()?;
    let mut parsed_ast = nativedsl::parser::Parser::new(&tokens, content, path)
        .parse()
        .ok()?;
    // Resolve so that Ident nodes become RuleRef/VarRef (needed for extract_references).
    let _ = nativedsl::resolve::resolve(&mut parsed_ast, &[], None, path);
    let definitions = extract_definitions(&parsed_ast);
    let references = extract_references(&parsed_ast);
    Some((definitions, references, Rope::from_str(content)))
}

/// Determine the current version and content source for a base grammar path.
/// Prefers in-memory text from `document_map` over the on-disk file.
fn current_base_source(
    path: &Path,
    document_map: &dashmap::DashMap<Url, Document>,
) -> Option<(SourceVersion, String)> {
    // Is this path open as a document?
    if let Ok(uri) = Url::from_file_path(path)
        && let Some(doc) = document_map.get(&uri)
    {
        return Some((SourceVersion::Document(doc.version), doc.text.clone()));
    }
    // Fall back to disk.
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    Some((SourceVersion::Disk(mtime), content))
}

fn extract_base_grammar_info(
    path: &PathBuf,
    ctx: Option<&AnalysisContext<'_>>,
) -> Option<(Vec<Definition>, Vec<Reference>, Rope)> {
    // Without a context (e.g. tests), fall back to a simple disk read.
    let Some(ctx) = ctx else {
        let content = std::fs::read_to_string(path).ok()?;
        return parse_base_grammar(path, &content);
    };

    let (version, content) = current_base_source(path, ctx.document_map)?;

    // Cache hit: version matches.
    if let Some(entry) = ctx.base_cache.get(path)
        && entry.version == version
    {
        return Some((
            entry.definitions.clone(),
            entry.references.clone(),
            entry.rope.clone(),
        ));
    }

    // Miss or stale: reparse and update cache.
    let (definitions, references, rope) = parse_base_grammar(path, &content)?;
    ctx.base_cache.insert(
        path.clone(),
        CachedBaseGrammar {
            version,
            definitions: definitions.clone(),
            references: references.clone(),
            rope: rope.clone(),
        },
    );
    Some((definitions, references, rope))
}

fn extract_analysis(
    tokens: &[nativedsl::lexer::Token],
    parsed_ast: &ast::Ast<'_>,
    base_grammar_path: Option<PathBuf>,
    ctx: Option<&AnalysisContext<'_>>,
) -> Analysis {
    let grammar_span = parsed_ast
        .root_items
        .iter()
        .find(|&&id| matches!(parsed_ast.node(id), ast::Node::Grammar))
        .map(|&id| parsed_ast.span(id));

    let definitions = extract_definitions(parsed_ast);
    let mut references = extract_references(parsed_ast);
    extract_builtin_references(tokens, grammar_span, &mut references);
    let (base_definitions, base_references, base_rope) = base_grammar_path
        .as_ref()
        .and_then(|p| extract_base_grammar_info(p, ctx))
        .map_or((Vec::new(), Vec::new(), None), |(defs, refs, rope)| {
            (defs, refs, Some(rope))
        });

    Analysis {
        tokens: Some(tokens.to_vec()),
        grammar_span,
        definitions: Some(definitions),
        references: Some(references),
        base_grammar_path,
        base_definitions: Some(base_definitions),
        base_references: Some(base_references),
        base_rope,
    }
}

/// Scan lexer tokens for builtin combinator keywords and add them as references.
/// Keywords inside the grammar block that are followed by `:` are config fields,
/// not builtin usages, and are excluded.
fn extract_builtin_references(
    tokens: &[nativedsl::lexer::Token],
    grammar_span: Option<ast::Span>,
    references: &mut Vec<Reference>,
) {
    use nativedsl::lexer::TokenKind;

    for (i, token) in tokens.iter().enumerate() {
        if !token.kind.is_keyword()
            || matches!(
                token.kind,
                TokenKind::KwGrammar
                    | TokenKind::KwRule
                    | TokenKind::KwLet
                    | TokenKind::KwFn
                    | TokenKind::KwFor
                    | TokenKind::KwIn
                    | TokenKind::KwOverride
            )
        {
            continue;
        }
        // Skip config field keywords inside the grammar block (e.g. `reserved:`).
        if let Some(gs) = grammar_span
            && token.span.start >= gs.start
            && token.span.end <= gs.end
            && tokens
                .get(i + 1)
                .is_some_and(|t| t.kind == TokenKind::Colon)
        {
            continue;
        }
        references.push(Reference {
            span: token.span,
            kind: RefKind::Builtin,
            scope: None,
        });
    }
}

fn build_fn_signature(parsed_ast: &ast::Ast<'_>, config: &ast::FnConfig, fn_name: &str) -> String {
    let mut sig = format!("fn {fn_name}(");
    for (i, param) in config.params.iter().enumerate() {
        if i > 0 {
            sig.push_str(", ");
        }
        let pname = parsed_ast.text(parsed_ast.span(param.name));
        let pty = type_node_to_str(parsed_ast, param.ty);
        sig.push_str(pname);
        sig.push_str(": ");
        sig.push_str(&pty);
    }
    sig.push_str(") -> ");
    sig.push_str(&type_node_to_str(parsed_ast, config.return_ty));
    sig
}

fn type_node_to_str(parsed_ast: &ast::Ast<'_>, ty_id: ast::NodeId) -> String {
    match parsed_ast.node(ty_id) {
        ast::Node::TypeRule => "rule_t".into(),
        ast::Node::TypeStr => "str_t".into(),
        ast::Node::TypeInt => "int_t".into(),
        ast::Node::TypeListRule => "list_rule_t".into(),
        ast::Node::TypeListStr => "list_str_t".into(),
        ast::Node::TypeListInt => "list_int_t".into(),
        ast::Node::TypeListListRule => "list_list_rule_t".into(),
        ast::Node::TypeListListStr => "list_list_str_t".into(),
        ast::Node::TypeListListInt => "list_list_int_t".into(),
        ast::Node::TypeVoid => "void_t".into(),
        ast::Node::TypeSpread => "spread_t".into(),
        _ => "?".into(),
    }
}

#[must_use]
pub fn uri_to_grammar_path(uri: &Url) -> PathBuf {
    uri.to_file_path()
        .unwrap_or_else(|()| PathBuf::from("grammar.tsg"))
}

/// Run the DSL pipeline (lex -> parse -> resolve) on the given source text and
/// return analysis data. Each field is populated as far as the pipeline gets.
///
/// If lex fails, all fields are `None`. If parse succeeds but resolve fails,
/// `tokens`/`grammar_span`/`definitions` are `Some`, references may be partial, etc.
#[must_use]
#[expect(clippy::missing_panics_doc, reason = "file  always has a parent")]
pub fn analyze(text: &str, uri: &Url, ctx: Option<&AnalysisContext<'_>>) -> Analysis {
    let grammar_path = uri_to_grammar_path(uri);

    // Stage 1: Lex
    let Ok(tokens) = nativedsl::lexer::Lexer::new(text).tokenize() else {
        return Analysis::default();
    };

    // Stage 2: Parse
    let Ok(mut parsed_ast) = nativedsl::parser::Parser::new(&tokens, text, &grammar_path).parse()
    else {
        return Analysis {
            tokens: Some(tokens),
            ..Analysis::default()
        };
    };

    // Stage 3: Validate inheritance
    if nativedsl::validate_inherit(&parsed_ast).is_err() {
        return extract_analysis(&tokens, &parsed_ast, None, ctx);
    }

    // Stage 4: Load base grammar (for inheritance)
    let grammar_dir = grammar_path.parent().unwrap();
    let (base, base_path) = match nativedsl::load_base_grammar(&parsed_ast, grammar_dir, &[]) {
        Ok(Some((g, p))) => (Some(g), Some(p)),
        Ok(None) => (None, None),
        Err(_) => return extract_analysis(&tokens, &parsed_ast, None, ctx),
    };

    let base_rule_names: Vec<String> = base
        .as_ref()
        .map(|g| g.variables.iter().map(|v| v.name.clone()).collect())
        .unwrap_or_default();

    let inherit_span = nativedsl::find_inherit_node(&parsed_ast).map(|id| parsed_ast.span(id));

    // Stage 5: Resolve
    if nativedsl::resolve::resolve(
        &mut parsed_ast,
        &base_rule_names,
        inherit_span,
        &grammar_path,
    )
    .is_err()
    {
        return extract_analysis(&tokens, &parsed_ast, base_path, ctx);
    }

    // Full success through resolve - extract analysis.
    extract_analysis(&tokens, &parsed_ast, base_path, ctx)
}

/// Run lex+parse and invoke `f` with the parsed AST. Returns `None` if either stage fails.
/// Use this when you need AST access but don't need type information.
pub fn with_ast<T>(text: &str, uri: &Url, f: impl FnOnce(&ast::Ast<'_>) -> T) -> Option<T> {
    let grammar_path = uri_to_grammar_path(uri);
    let tokens = nativedsl::lexer::Lexer::new(text).tokenize().ok()?;
    let parsed_ast = nativedsl::parser::Parser::new(&tokens, text, &grammar_path)
        .parse()
        .ok()?;
    Some(f(&parsed_ast))
}

/// Find the key span and value node of `<object_name>.<field_name>` by walking
/// the AST for a top-level `let <object_name> = { ... }` binding with an object
/// literal value.
#[must_use]
pub fn find_object_field(
    parsed_ast: &ast::Ast<'_>,
    object_name: &str,
    field_name: &str,
) -> Option<(ast::Span, ast::NodeId)> {
    for &item_id in &parsed_ast.root_items {
        if let ast::Node::Let { name, value, .. } = parsed_ast.node(item_id)
            && parsed_ast.text(parsed_ast.span(*name)) == object_name
            && let ast::Node::Object(range) = parsed_ast.node(*value)
        {
            for &(key_span, value_id) in parsed_ast.context.get_object(*range) {
                if parsed_ast.text(key_span) == field_name {
                    return Some((key_span, value_id));
                }
            }
        }
    }
    None
}

/// Run the pipeline through typecheck on the given source text, calling `f`
/// with the resolved AST and type environment.
#[expect(clippy::missing_panics_doc, reason = "file always has a parent")]
pub fn with_type_env<T>(
    text: &str,
    uri: &Url,
    f: impl FnOnce(&ast::Ast<'_>, &nativedsl::typecheck::TypeEnv<'_>) -> T,
) -> Option<T> {
    let grammar_path = uri_to_grammar_path(uri);
    let tokens = nativedsl::lexer::Lexer::new(text).tokenize().ok()?;
    let mut parsed_ast = nativedsl::parser::Parser::new(&tokens, text, &grammar_path)
        .parse()
        .ok()?;
    nativedsl::validate_inherit(&parsed_ast).ok()?;

    let grammar_dir = grammar_path.parent().unwrap();
    let base = nativedsl::load_base_grammar(&parsed_ast, grammar_dir, &[])
        .ok()?
        .map(|(g, _)| g);
    let base_rule_names: Vec<String> = base
        .as_ref()
        .map(|g| g.variables.iter().map(|v| v.name.clone()).collect())
        .unwrap_or_default();
    let inherit_span = nativedsl::find_inherit_node(&parsed_ast).map(|id| parsed_ast.span(id));

    nativedsl::resolve::resolve(
        &mut parsed_ast,
        &base_rule_names,
        inherit_span,
        &grammar_path,
    )
    .ok()?;
    let env = nativedsl::typecheck::check(&parsed_ast).ok()?;

    Some(f(&parsed_ast, &env))
}

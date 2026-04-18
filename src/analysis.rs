use std::path::{Path, PathBuf};

use ropey::Rope;
use tower_lsp::lsp_types::Url;

use tree_sitter_generate::nativedsl::{self, ast};

use crate::document::{Analysis, DefKind, Definition, Document, RefKind, Reference};

// ---------------------------------------------------------------------------
// Analysis extraction - walk the AST to collect definitions and references
// ---------------------------------------------------------------------------

/// Extract definitions from an AST's root items.
fn extract_definitions(parsed_ast: &ast::Ast) -> Vec<Definition> {
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
                let kind = if matches!(parsed_ast.node(*value), ast::Node::Import { .. }) {
                    DefKind::Import
                } else {
                    DefKind::Let { scope: None }
                };
                definitions.push(Definition {
                    name: parsed_ast.text(name_span).to_owned(),
                    kind,
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
fn find_scope(span: ast::Span, parsed_ast: &ast::Ast) -> Option<ast::Span> {
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

/// Collect the names of import variables from the AST (`let x = import(...)`).
fn collect_import_names(parsed_ast: &ast::Ast) -> rustc_hash::FxHashSet<String> {
    let mut names = rustc_hash::FxHashSet::default();
    for &item_id in &parsed_ast.root_items {
        if let ast::Node::Let { name, value, .. } = parsed_ast.node(item_id)
            && matches!(parsed_ast.node(*value), ast::Node::Import { .. })
        {
            names.insert(parsed_ast.text(parsed_ast.span(*name)).to_owned());
        }
    }
    names
}

/// Resolve the qualified access path segments from an obj node.
/// For `a::b::c`, given the `c` node's obj (which is `a::b`), returns `["a", "b"]`.
fn collect_qualified_path(parsed_ast: &ast::Ast, obj_id: ast::NodeId) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = obj_id;
    loop {
        match parsed_ast.node(current) {
            ast::Node::VarRef | ast::Node::Ident => {
                path.push(parsed_ast.text(parsed_ast.span(current)).to_owned());
                break;
            }
            ast::Node::QualifiedAccess { obj, member } => {
                path.push(parsed_ast.text(parsed_ast.span(*member)).to_owned());
                current = *obj;
            }
            _ => break,
        }
    }
    path.reverse();
    path
}

/// Extract resolved references from all nodes in the AST.
fn extract_references(
    parsed_ast: &ast::Ast,
    import_names: &rustc_hash::FxHashSet<String>,
) -> Vec<Reference> {
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
            // `expr::member` qualified access - could be base rule or import access.
            ast::Node::QualifiedAccess { obj, member } => {
                let member_span = parsed_ast.span(*member);
                let member_name = parsed_ast.text(member_span).to_owned();
                // Check if the root of the access chain is an import variable.
                let path = collect_qualified_path(parsed_ast, *obj);
                let is_import = path.first().is_some_and(|root| import_names.contains(root));
                let kind = if is_import {
                    RefKind::ImportedMember {
                        path,
                        member: member_name,
                    }
                } else {
                    RefKind::BaseRule(member_name)
                };
                references.push(Reference {
                    span: member_span,
                    kind,
                    scope: find_scope(member_span, parsed_ast),
                });
            }
            // `expr::fn_name(args)` qualified call - import function call.
            ast::Node::QualifiedCall(range) => {
                let (obj, name, _args) = parsed_ast.get_qualified_call(*range);
                let name_span = parsed_ast.span(name);
                let member_name = parsed_ast.text(name_span).to_owned();
                let path = collect_qualified_path(parsed_ast, obj);
                let is_import = path.first().is_some_and(|root| import_names.contains(root));
                if is_import {
                    references.push(Reference {
                        span: name_span,
                        kind: RefKind::ImportedMember {
                            path,
                            member: member_name,
                        },
                        scope: find_scope(name_span, parsed_ast),
                    });
                }
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
            ast::Node::Inherit { path, .. } => {
                let path_span = parsed_ast.span(*path);
                references.push(Reference {
                    span: path_span,
                    kind: RefKind::InheritPath,
                    scope: None,
                });
            }
            // import("path") - the path string literal.
            ast::Node::Import { path, .. } => {
                let path_span = parsed_ast.span(*path);
                references.push(Reference {
                    span: path_span,
                    kind: RefKind::ImportPath,
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

/// Resolve the inherit path from the AST and read the base grammar to extract rule names.
/// Returns (`rule_names`, `canonical_path`).
fn resolve_base_grammar(
    parsed_ast: &ast::Ast,
    grammar_dir: &Path,
) -> (Vec<String>, Option<PathBuf>) {
    let Some(inherit_id) = nativedsl::find_inherit_node(parsed_ast) else {
        return (Vec::new(), None);
    };
    let ast::Node::Inherit { path, .. } = parsed_ast.node(inherit_id) else {
        return (Vec::new(), None);
    };
    let path_str = parsed_ast.node_text(*path);
    let full_path = grammar_dir.join(path_str);
    let canonical = dunce::canonicalize(&full_path).ok().unwrap_or(full_path);

    // Read and parse the base grammar to get rule names.
    let Ok(content) = std::fs::read_to_string(&canonical) else {
        return (Vec::new(), Some(canonical));
    };

    let ext = canonical.extension().and_then(|e| e.to_str());
    let rule_names = match ext {
        Some("tsg") => {
            let Ok(tokens) = nativedsl::lexer::Lexer::new(&content).tokenize() else {
                return (Vec::new(), Some(canonical));
            };
            let Ok(base_ast) = nativedsl::parser::Parser::new(&tokens, content, &canonical).parse()
            else {
                return (Vec::new(), Some(canonical));
            };
            base_ast
                .root_items
                .iter()
                .filter_map(|&id| {
                    if let ast::Node::Rule { name, .. } = base_ast.node(id) {
                        Some(base_ast.text(base_ast.span(*name)).to_owned())
                    } else {
                        None
                    }
                })
                .collect()
        }
        Some("json") => {
            // Extract rule names from grammar.json by parsing the "rules" keys.
            serde_json::from_str::<serde_json::Value>(&content)
                .ok()
                .and_then(|v| Some(v.get("rules")?.as_object()?.keys().cloned().collect()))
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };

    (rule_names, Some(canonical))
}

/// Parse and resolve a base grammar file to extract definitions, references, and a rope.
fn parse_base_grammar(
    path: &Path,
    content: &str,
) -> Option<(Vec<Definition>, Vec<Reference>, Rope)> {
    let tokens = nativedsl::lexer::Lexer::new(content).tokenize().ok()?;
    let mut parsed_ast = nativedsl::parser::Parser::new(&tokens, content.to_owned(), path)
        .parse()
        .ok()?;
    // Resolve so that Ident nodes become RuleRef/VarRef (needed for extract_references).
    let _ = nativedsl::resolve::resolve(&mut parsed_ast, &[], None, path);
    let definitions = extract_definitions(&parsed_ast);
    let import_names = collect_import_names(&parsed_ast);
    let references = extract_references(&parsed_ast, &import_names);
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
    parsed_ast: &ast::Ast,
    grammar_dir: &Path,
    base_grammar_path: Option<PathBuf>,
    ctx: Option<&AnalysisContext<'_>>,
) -> Analysis {
    let grammar_span = parsed_ast
        .root_items
        .iter()
        .find(|&&id| matches!(parsed_ast.node(id), ast::Node::Grammar))
        .map(|&id| parsed_ast.span(id));

    let definitions = extract_definitions(parsed_ast);
    let import_names = collect_import_names(parsed_ast);
    let mut references = extract_references(parsed_ast, &import_names);
    extract_builtin_references(tokens, grammar_span, &mut references);
    let (base_definitions, base_references, base_rope) = base_grammar_path
        .as_ref()
        .and_then(|p| extract_base_grammar_info(p, ctx))
        .map_or((Vec::new(), Vec::new(), None), |(defs, refs, rope)| {
            (defs, refs, Some(rope))
        });

    // Load imported modules.
    let import_modules = extract_import_modules(parsed_ast, grammar_dir, ctx);

    Analysis {
        tokens: Some(tokens.to_vec()),
        grammar_span,
        definitions: Some(definitions),
        references: Some(references),
        base_grammar_path,
        base_definitions: Some(base_definitions),
        base_references: Some(base_references),
        base_rope,
        import_modules,
    }
}

/// Shared state for recursive import loading: cycle detection and deduplication.
struct ImportLoadContext<'a> {
    analysis_ctx: Option<&'a AnalysisContext<'a>>,
    /// Ancestor chain for cycle detection (canonical paths of files being loaded).
    ancestor_paths: Vec<PathBuf>,
    /// Deduplication cache: canonical path -> already-loaded module info.
    /// When the same file is imported by multiple modules, we clone from
    /// here instead of re-parsing.
    loaded: rustc_hash::FxHashMap<PathBuf, crate::document::ExternalModuleInfo>,
}

/// Load imported module info for all `let x = import("path")` bindings.
/// Resolves each import path relative to `grammar_dir`, parses the module,
/// and recursively loads sub-imports with cycle detection and deduplication.
fn extract_import_modules(
    parsed_ast: &ast::Ast,
    grammar_dir: &Path,
    ctx: Option<&AnalysisContext<'_>>,
) -> Vec<(String, crate::document::ExternalModuleInfo)> {
    let mut load_ctx = ImportLoadContext {
        analysis_ctx: ctx,
        ancestor_paths: Vec::new(),
        loaded: rustc_hash::FxHashMap::default(),
    };
    extract_import_modules_inner(parsed_ast, grammar_dir, &mut load_ctx)
}

fn extract_import_modules_inner(
    parsed_ast: &ast::Ast,
    grammar_dir: &Path,
    load_ctx: &mut ImportLoadContext<'_>,
) -> Vec<(String, crate::document::ExternalModuleInfo)> {
    use crate::document::ExternalModuleInfo;

    let mut modules = Vec::new();

    for &item_id in &parsed_ast.root_items {
        let ast::Node::Let { name, value, .. } = parsed_ast.node(item_id) else {
            continue;
        };
        let ast::Node::Import { path, .. } = parsed_ast.node(*value) else {
            continue;
        };
        let var_name = parsed_ast.text(parsed_ast.span(*name)).to_owned();
        let path_str = parsed_ast.node_text(*path);

        let full_path = grammar_dir.join(path_str);
        let canonical = dunce::canonicalize(&full_path).ok().unwrap_or(full_path);

        // Cycle detection: skip if this path is already in the ancestor chain.
        if load_ctx.ancestor_paths.iter().any(|p| p == &canonical) {
            continue;
        }

        // Deduplication: reuse if we've already loaded this path.
        if let Some(cached) = load_ctx.loaded.get(&canonical) {
            modules.push((var_name, cached.clone()));
            continue;
        }

        if let Some((defs, refs, rope)) = extract_base_grammar_info(&canonical, load_ctx.analysis_ctx) {
            load_ctx.ancestor_paths.push(canonical.clone());
            let sub_imports = load_sub_imports(&canonical, load_ctx);
            load_ctx.ancestor_paths.pop();

            let info = ExternalModuleInfo {
                path: canonical.clone(),
                definitions: defs,
                references: refs,
                rope,
                import_modules: sub_imports,
            };
            load_ctx.loaded.insert(canonical, info.clone());
            modules.push((var_name, info));
        }
    }

    modules
}

/// Parse an imported module file and recursively extract its sub-imports.
fn load_sub_imports(
    module_path: &Path,
    load_ctx: &mut ImportLoadContext<'_>,
) -> Vec<(String, crate::document::ExternalModuleInfo)> {
    let Some(module_dir) = module_path.parent() else {
        return Vec::new();
    };
    let content = load_ctx
        .analysis_ctx
        .and_then(|c| current_base_source(module_path, c.document_map))
        .map(|(_, c)| c)
        .or_else(|| std::fs::read_to_string(module_path).ok());
    let Some(content) = content else {
        return Vec::new();
    };
    let Ok(tokens) = nativedsl::lexer::Lexer::new(&content).tokenize() else {
        return Vec::new();
    };
    let Ok(parsed_ast) =
        nativedsl::parser::Parser::new(&tokens, content, module_path).parse()
    else {
        return Vec::new();
    };
    extract_import_modules_inner(&parsed_ast, module_dir, load_ctx)
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

fn build_fn_signature(parsed_ast: &ast::Ast, config: &ast::FnConfig, fn_name: &str) -> String {
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

fn type_node_to_str(parsed_ast: &ast::Ast, ty_id: ast::NodeId) -> String {
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
    let Ok(mut parsed_ast) =
        nativedsl::parser::Parser::new(&tokens, text.to_owned(), &grammar_path).parse()
    else {
        return Analysis {
            tokens: Some(tokens),
            ..Analysis::default()
        };
    };

    let grammar_dir = grammar_path.parent().unwrap();

    // Stage 3: Validate inheritance
    if nativedsl::validate_grammar(&parsed_ast).is_err() {
        return extract_analysis(&tokens, &parsed_ast, grammar_dir, None, ctx);
    }

    // Stage 4: Resolve the base grammar path and rule names from inherit()
    let (base_rule_names, base_path) = resolve_base_grammar(&parsed_ast, grammar_dir);

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
        return extract_analysis(&tokens, &parsed_ast, grammar_dir, base_path, ctx);
    }

    // Full success through resolve - extract analysis.
    extract_analysis(&tokens, &parsed_ast, grammar_dir, base_path, ctx)
}

/// Run lex+parse and invoke `f` with the parsed AST. Returns `None` if either stage fails.
/// Use this when you need AST access but don't need type information.
pub fn with_ast<T>(text: &str, uri: &Url, f: impl FnOnce(&ast::Ast) -> T) -> Option<T> {
    let grammar_path = uri_to_grammar_path(uri);
    let tokens = nativedsl::lexer::Lexer::new(text).tokenize().ok()?;
    let parsed_ast = nativedsl::parser::Parser::new(&tokens, text.to_owned(), &grammar_path)
        .parse()
        .ok()?;
    Some(f(&parsed_ast))
}

/// Find the key span and value node of `<object_name>.<field_name>` by walking
/// the AST for a top-level `let <object_name> = { ... }` binding with an object
/// literal value.
#[must_use]
pub fn find_object_field(
    parsed_ast: &ast::Ast,
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

/// Run the pipeline through typecheck on the given source text.
///
/// Calls `f` with the resolved AST and type environment. Uses the core's
/// `load_module` for proper module loading, index tagging, and recursive
/// typecheck.
pub fn with_type_env<T>(
    text: &str,
    uri: &Url,
    f: impl FnOnce(&ast::Ast, &nativedsl::typecheck::TypeEnv<'_>) -> T,
) -> Option<T> {
    let grammar_path = uri_to_grammar_path(uri);
    let module =
        nativedsl::load_module(text, &grammar_path, nativedsl::ModuleKind::Grammar, &[]).ok()?;
    let module_envs = nativedsl::typecheck_modules(&module.sub_modules).ok()?;
    let env = nativedsl::typecheck::check(&module.ast, module_envs).ok()?;
    Some(f(&module.ast, &env))
}

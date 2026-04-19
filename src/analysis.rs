use std::path::{Path, PathBuf};

use ropey::Rope;
use tower_lsp::lsp_types::Url;

use tree_sitter_generate::nativedsl::{self, ast};

use crate::document::{Analysis, DefKind, Definition, Document, RefKind, Reference};

const MAX_GRAMMAR_CACHE_ENTRIES: usize = 32;

// ---------------------------------------------------------------------------
// Analysis extraction - walk the AST to collect definitions and references
// ---------------------------------------------------------------------------

/// Extract definitions from an AST's root items.
fn extract_definitions(parsed_ast: &ast::Ast, scopes: &ScopeIndex) -> Vec<Definition> {
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
                let kind = match parsed_ast.node(*value) {
                    ast::Node::Import { .. } => DefKind::Import,
                    ast::Node::Inherit { .. } => DefKind::Inherit,
                    _ => DefKind::Let { scope: None },
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
            let scope = scopes.find(for_span).unwrap_or(for_span);
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

/// Pre-collected scope spans (functions and for-loops), sorted by start
/// position for efficient lookup.
struct ScopeIndex {
    /// All scope spans, sorted by start position descending so the
    /// narrowest enclosing scope is found first.
    scopes: Vec<ast::Span>,
}

impl ScopeIndex {
    fn build(parsed_ast: &ast::Ast) -> Self {
        let mut scopes = Vec::new();
        for &item_id in &parsed_ast.root_items {
            if matches!(parsed_ast.node(item_id), ast::Node::Fn(_)) {
                scopes.push(parsed_ast.span(item_id));
            }
        }
        for (i, node) in parsed_ast.nodes.iter().enumerate().skip(1) {
            if matches!(node, ast::Node::For(_)) {
                scopes.push(parsed_ast.context.spans[i]);
            }
        }
        // Sort by start descending so inner (narrower) scopes come first.
        scopes.sort_unstable_by(|a, b| b.start.cmp(&a.start));
        Self { scopes }
    }

    /// Find the narrowest enclosing scope for a span.
    fn find(&self, span: ast::Span) -> Option<ast::Span> {
        self.scopes
            .iter()
            .copied()
            .filter(|s| s.start <= span.start && span.end <= s.end)
            .min_by_key(|s| s.end - s.start)
    }
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
    scopes: &ScopeIndex,
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
                    scope: scopes.find(span),
                });
            }
            ast::Node::VarRef => {
                references.push(Reference {
                    span,
                    kind: RefKind::Variable(parsed_ast.text(span).to_owned()),
                    scope: scopes.find(span),
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
                    scope: scopes.find(member_span),
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
                        scope: scopes.find(name_span),
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
                    scope: scopes.find(field_span),
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

pub struct CachedGrammar {
    pub version: SourceVersion,
    pub definitions: Vec<Definition>,
    pub references: Vec<Reference>,
    pub rope: Rope,
    /// The full loaded module, for use by `resolve` (needs `InputGrammar`).
    /// `None` for grammars that were only parsed for definitions (e.g. imports
    /// loaded via `parse_base_grammar`).
    pub module: Option<std::sync::Arc<nativedsl::Module>>,
}

/// Cache of parsed grammars, keyed by absolute path. Stores both the
/// extracted LSP data (definitions/references/rope) and optionally the
/// full `Module` from core's `load_module`.
pub type GrammarCache = dashmap::DashMap<PathBuf, CachedGrammar>;

/// Context passed to `analyze()` to enable base grammar caching and
/// lookup of in-memory text for open documents.
pub struct AnalysisContext<'a> {
    pub grammar_cache: &'a GrammarCache,
    pub document_map: &'a dashmap::DashMap<Url, Document>,
}

/// Resolve the inherit path from the AST and load the base grammar.
///
/// Uses the core's `load_module` pipeline. Caches the result by path +
/// mtime so subsequent calls skip the expensive pipeline.
fn resolve_base_grammar(
    parsed_ast: &ast::Ast,
    grammar_dir: &Path,
    ctx: Option<&AnalysisContext<'_>>,
) -> (Option<std::sync::Arc<nativedsl::Module>>, Option<PathBuf>) {
    let Some(inherit_id) = nativedsl::find_inherit_node(parsed_ast) else {
        return (None, None);
    };
    let ast::Node::Inherit { path, .. } = parsed_ast.node(inherit_id) else {
        return (None, None);
    };
    let path_str = parsed_ast.node_text(*path);
    let full_path = grammar_dir.join(path_str);
    let canonical = dunce::canonicalize(&full_path).ok().unwrap_or(full_path);

    // Check the cache for a previously loaded module with matching mtime.
    let mtime = std::fs::metadata(&canonical)
        .and_then(|m| m.modified())
        .ok();
    if let Some(ctx) = ctx
        && let Some(mtime) = mtime
        && let Some(entry) = ctx.grammar_cache.get(&canonical)
        && entry.version == SourceVersion::Disk(mtime)
        && let Some(module) = &entry.module
    {
        return (Some(std::sync::Arc::clone(module)), Some(canonical));
    }

    let Ok(content) = std::fs::read_to_string(&canonical) else {
        return (None, Some(canonical));
    };

    let module = nativedsl::load_module(
        &content,
        &canonical,
        nativedsl::ModuleKind::Grammar,
        &[],
    )
    .ok()
    .map(std::sync::Arc::new);

    // Store the module in the grammar cache for reuse.
    if let Some(ctx) = ctx
        && let Some(mtime) = mtime
        && let Some(module) = &module
    {
        // Also extract definitions/references so the cache entry serves
        // both resolve_base_grammar (needs Module) and extract_base_grammar_info
        // (needs definitions/references/rope).
        let scopes = ScopeIndex::build(&module.ast);
        let definitions = extract_definitions(&module.ast, &scopes);
        let import_names = collect_import_names(&module.ast);
        let references = extract_references(&module.ast, &import_names, &scopes);

        if ctx.grammar_cache.len() >= MAX_GRAMMAR_CACHE_ENTRIES {
            ctx.grammar_cache.clear();
        }
        ctx.grammar_cache.insert(
            canonical.clone(),
            CachedGrammar {
                version: SourceVersion::Disk(mtime),
                definitions,
                references,
                rope: Rope::from_str(module.ast.source()),
                module: Some(std::sync::Arc::clone(module)),
            },
        );
    }

    (module, Some(canonical))
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
    let _ = nativedsl::resolve::resolve(&mut parsed_ast, None, path);
    let scopes = ScopeIndex::build(&parsed_ast);
    let definitions = extract_definitions(&parsed_ast, &scopes);
    let import_names = collect_import_names(&parsed_ast);
    let references = extract_references(&parsed_ast, &import_names, &scopes);
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
    if let Some(entry) = ctx.grammar_cache.get(path)
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

    // Evict the entire cache if it grows too large. In practice a session
    // works with a handful of base grammars, so this rarely triggers.
    if ctx.grammar_cache.len() >= MAX_GRAMMAR_CACHE_ENTRIES {
        ctx.grammar_cache.clear();
    }

    ctx.grammar_cache.insert(
        path.clone(),
        CachedGrammar {
            version,
            definitions: definitions.clone(),
            references: references.clone(),
            rope: rope.clone(),
            module: None,
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

    let scopes = ScopeIndex::build(parsed_ast);
    let definitions = extract_definitions(parsed_ast, &scopes);
    let import_names = collect_import_names(parsed_ast);
    let mut references = extract_references(parsed_ast, &import_names, &scopes);
    extract_builtin_references(tokens, grammar_span, &mut references);
    let base_module = base_grammar_path.and_then(|path| {
        let (defs, refs, rope) = extract_base_grammar_info(&path, ctx)?;
        Some(crate::document::ExternalModuleInfo {
            path,
            definitions: defs,
            references: refs,
            rope,
            import_modules: Vec::new(),
        })
    });

    // Load imported modules.
    let import_modules = extract_import_modules(parsed_ast, grammar_dir, ctx);

    Analysis {
        tokens: Some(tokens.to_vec()),
        grammar_span,
        definitions: Some(definitions),
        references: Some(references),
        base_module,
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

    let inherit_node = nativedsl::find_inherit_node(&parsed_ast);

    // Stage 3: Validate inheritance
    if nativedsl::validate_grammar(&parsed_ast, inherit_node).is_err() {
        return extract_analysis(&tokens, &parsed_ast, grammar_dir, None, ctx);
    }

    // Stage 4: Load the base grammar (for resolve and LSP features)
    let (base_module, base_path) = resolve_base_grammar(&parsed_ast, grammar_dir, ctx);
    let inherit_span = inherit_node.map(|id| parsed_ast.span(id));
    let base_for_resolve = base_module
        .as_ref()
        .and_then(|m| Some((m.lowered.as_ref()?, inherit_span?)));

    // Stage 5: Resolve
    if nativedsl::resolve::resolve(&mut parsed_ast, base_for_resolve, &grammar_path).is_err() {
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

#[cfg(test)]
mod bench {
    use super::*;

    #[test]
    #[ignore = "benchmark"]
    fn bench_analyze_stages() {
        let path =
            std::path::PathBuf::from("/home/lillis/projects/grammars/tree-sitter-cpp/grammar.tsg");
        let Ok(source) = std::fs::read_to_string(&path) else {
            eprintln!("skipping: cpp grammar not found");
            return;
        };
        let uri = Url::from_file_path(&path).unwrap();
        let grammar_path = uri_to_grammar_path(&uri);
        let grammar_dir = grammar_path.parent().unwrap();

        let n = 200u32;

        // Stage 1: Lex
        let start = std::time::Instant::now();
        let mut tokens_store = None;
        for _ in 0..n {
            let t = nativedsl::lexer::Lexer::new(&source).tokenize().unwrap();
            tokens_store = Some(t);
        }
        let lex_time = start.elapsed() / n;
        let tokens = tokens_store.unwrap();

        // Stage 2: Parse
        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(
                nativedsl::parser::Parser::new(&tokens, source.clone(), &grammar_path)
                    .parse()
                    .unwrap(),
            );
        }
        let parse_time = start.elapsed() / n;

        let mut parsed_ast = nativedsl::parser::Parser::new(&tokens, source.clone(), &grammar_path)
            .parse()
            .unwrap();

        // Stage 3: Validate
        let inherit_node = nativedsl::find_inherit_node(&parsed_ast);
        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(nativedsl::validate_grammar(&parsed_ast, inherit_node).unwrap());
        }
        let validate_time = start.elapsed() / n;

        // Stage 4: Load base grammar
        let start = std::time::Instant::now();
        let (base_module, base_path) = resolve_base_grammar(&parsed_ast, grammar_dir, None);
        let load_base_time = start.elapsed();

        let inherit_span = inherit_node.map(|id| parsed_ast.span(id));
        let base_for_resolve = base_module
            .as_ref()
            .and_then(|m| Some((m.lowered.as_ref()?, inherit_span?)));

        // Stage 5: Resolve
        let start = std::time::Instant::now();
        for _ in 0..n {
            let mut ast_clone =
                nativedsl::parser::Parser::new(&tokens, source.clone(), &grammar_path)
                    .parse()
                    .unwrap();
            nativedsl::resolve::resolve(&mut ast_clone, base_for_resolve, &grammar_path).unwrap();
        }
        let resolve_time = start.elapsed() / n - parse_time; // subtract parse from resolve

        nativedsl::resolve::resolve(&mut parsed_ast, base_for_resolve, &grammar_path).unwrap();

        // Stage 6: extract_analysis components
        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(ScopeIndex::build(&parsed_ast));
        }
        let scope_build_time = start.elapsed() / n;

        let scopes = ScopeIndex::build(&parsed_ast);

        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(extract_definitions(&parsed_ast, &scopes));
        }
        let defs_time = start.elapsed() / n;

        let import_names = collect_import_names(&parsed_ast);

        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(extract_references(&parsed_ast, &import_names, &scopes));
        }
        let refs_time = start.elapsed() / n;

        let grammar_span = parsed_ast
            .root_items
            .iter()
            .find(|&&id| matches!(parsed_ast.node(id), ast::Node::Grammar))
            .map(|&id| parsed_ast.span(id));
        let start = std::time::Instant::now();
        for _ in 0..n {
            let mut refs = Vec::new();
            extract_builtin_references(&tokens, grammar_span, &mut refs);
            std::hint::black_box(refs);
        }
        let builtin_refs_time = start.elapsed() / n;

        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(extract_import_modules(&parsed_ast, grammar_dir, None));
        }
        let imports_time = start.elapsed() / n;

        // Stage 7: Full analyze
        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(analyze(&source, &uri, None));
        }
        let total_time = start.elapsed() / n;

        eprintln!("=== analyze() breakdown on cpp grammar ({} lines) ===", source.lines().count());
        eprintln!("Lex:               {:>8?}", lex_time);
        eprintln!("Parse:             {:>8?}", parse_time);
        eprintln!("Validate:          {:>8?}", validate_time);
        eprintln!("Load base grammar: {:>8?}  (one-shot, not amortized)", load_base_time);
        eprintln!("Resolve:           {:>8?}", resolve_time);
        eprintln!("ScopeIndex build:  {:>8?}", scope_build_time);
        eprintln!("Extract defs:      {:>8?}", defs_time);
        eprintln!("Extract refs:      {:>8?}", refs_time);
        eprintln!("Builtin refs:      {:>8?}", builtin_refs_time);
        eprintln!("Import modules:    {:>8?}", imports_time);
        eprintln!("---");
        eprintln!("Total analyze():   {:>8?}", total_time);
    }
}

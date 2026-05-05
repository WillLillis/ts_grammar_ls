use std::fmt::Write as _;
use std::path::PathBuf;

use ropey::Rope;
use tower_lsp::lsp_types::Url;

use tree_sitter_generate::nativedsl::{self, ast};

use crate::document::{Analysis, DefKind, Definition, RefKind, Reference};

// ---------------------------------------------------------------------------
// Analysis extraction - walk the AST to collect definitions and references
// ---------------------------------------------------------------------------

/// Extract definitions from an AST's root items.
fn extract_definitions(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    scopes: &ScopeIndex,
) -> Vec<Definition> {
    let mut definitions = Vec::new();
    for &item_id in &ctx.root_items {
        match shared.arena.get(item_id) {
            ast::Node::Rule {
                is_override, name, ..
            } => {
                definitions.push(Definition {
                    name: ctx.text(*name).to_owned(),
                    kind: if *is_override {
                        DefKind::OverrideRule
                    } else {
                        DefKind::Rule
                    },
                    name_span: *name,
                    full_span: shared.arena.span(item_id),
                });
            }
            ast::Node::Macro(macro_id) => {
                let config = shared.pools.get_macro(*macro_id);
                let fn_name = ctx.text(config.name);
                let signature = build_fn_signature(ctx, config, fn_name);
                let fn_span = shared.arena.span(item_id);
                definitions.push(Definition {
                    name: fn_name.to_owned(),
                    kind: DefKind::Function { signature },
                    name_span: config.name,
                    full_span: fn_span,
                });
                // Extract parameters as scoped definitions.
                for param in &config.params {
                    definitions.push(Definition {
                        name: ctx.text(param.name).to_owned(),
                        kind: DefKind::Parameter { scope: fn_span },
                        name_span: param.name,
                        full_span: param.name,
                    });
                }
            }
            ast::Node::Let { name, value, .. } => {
                let full_span = shared.arena.span(item_id);
                let kind = match shared.arena.get(*value) {
                    ast::Node::ModuleRef { import: true, .. } => DefKind::Import,
                    ast::Node::ModuleRef { import: false, .. } => DefKind::Inherit,
                    _ => DefKind::Let { scope: None },
                };
                definitions.push(Definition {
                    name: ctx.text(*name).to_owned(),
                    kind,
                    name_span: *name,
                    full_span,
                });
                // Extract object field keys as definitions.
                if let ast::Node::Object(range) = shared.arena.get(*value) {
                    for &(key_span, _) in shared.pools.get_object(*range) {
                        definitions.push(Definition {
                            name: ctx.text(key_span).to_owned(),
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

    // Extract for-loop bindings from all nodes in this module.
    for (node_id, node) in ctx.iter_own_nodes(&shared.arena) {
        if let ast::Node::For { for_id, .. } = node {
            let for_span = shared.arena.span(node_id);
            let config = shared.pools.get_for(*for_id);
            let scope = scopes.find(for_span).unwrap_or(for_span);
            for &(binding_span, _) in &config.bindings {
                definitions.push(Definition {
                    name: ctx.text(binding_span).to_owned(),
                    kind: DefKind::Parameter { scope },
                    name_span: binding_span,
                    full_span: binding_span,
                });
            }
        }
    }

    definitions
}

/// Pre-collected scope spans (macros and for-loops), sorted by start
/// position for efficient lookup. Macro scopes also track their `MacroId`
/// so `MacroParam` references can be resolved to parameter names.
struct ScopeIndex {
    /// All scope spans, sorted by start position descending so the
    /// narrowest enclosing scope is found first.
    scopes: Vec<ast::Span>,
    /// Map from a macro scope's start position to its `MacroId`. Used to
    /// resolve `MacroParam(index)` -> param name.
    macro_ids: rustc_hash::FxHashMap<u32, ast::MacroId>,
}

impl ScopeIndex {
    fn build(shared: &ast::SharedAst, ctx: &ast::ModuleContext) -> Self {
        let mut scopes = Vec::new();
        let mut macro_ids = rustc_hash::FxHashMap::default();
        for &item_id in &ctx.root_items {
            if let ast::Node::Macro(macro_id) = shared.arena.get(item_id) {
                let span = shared.arena.span(item_id);
                scopes.push(span);
                macro_ids.insert(span.start, *macro_id);
            }
        }
        for (node_id, node) in ctx.iter_own_nodes(&shared.arena) {
            if matches!(node, ast::Node::For { .. }) {
                scopes.push(shared.arena.span(node_id));
            }
        }
        // Sort by start descending so inner (narrower) scopes come first.
        scopes.sort_unstable_by(|a, b| b.start.cmp(&a.start));
        Self { scopes, macro_ids }
    }

    /// Find the narrowest enclosing scope for a span.
    fn find(&self, span: ast::Span) -> Option<ast::Span> {
        self.scopes
            .iter()
            .copied()
            .filter(|s| s.start <= span.start && span.end <= s.end)
            .min_by_key(|s| s.end - s.start)
    }

    /// If `scope` is a macro scope, return its `MacroId`.
    fn macro_id_for_span(&self, scope: ast::Span) -> Option<ast::MacroId> {
        self.macro_ids.get(&scope.start).copied()
    }
}

/// Collect the names of import variables from the AST (`let x = import(...)`).
fn collect_import_names(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
) -> rustc_hash::FxHashSet<String> {
    let mut names = rustc_hash::FxHashSet::default();
    for &item_id in &ctx.root_items {
        if let ast::Node::Let { name, value, .. } = shared.arena.get(item_id)
            && matches!(
                shared.arena.get(*value),
                ast::Node::ModuleRef { import: true, .. }
            )
        {
            names.insert(ctx.text(*name).to_owned());
        }
    }
    names
}

/// Resolve the qualified access path segments from an obj node.
/// For `a::b::c`, given the `c` node's obj (which is `a::b`), returns `["a", "b"]`.
///
/// The resolver may collapse a `QualifiedAccess` chain into a single
/// `Ident(Var(...))` whose span covers the entire chain. We split on `::` to
/// recover the segments in that case.
fn collect_qualified_path(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    obj_id: ast::NodeId,
) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = obj_id;
    loop {
        match shared.arena.get(current) {
            ast::Node::Ident(_) => {
                // Walking tail-to-root, so push segments in reverse.
                let text = ctx.text(shared.arena.span(current));
                for part in text.rsplit("::") {
                    path.push(part.trim().to_owned());
                }
                break;
            }
            ast::Node::QualifiedAccess { obj, member } => {
                path.push(ctx.text(*member).to_owned());
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
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    import_names: &rustc_hash::FxHashSet<String>,
    scopes: &ScopeIndex,
) -> Vec<Reference> {
    let mut references = Vec::new();

    for (node_id, node) in ctx.iter_own_nodes(&shared.arena) {
        let span = shared.arena.span(node_id);
        match node {
            ast::Node::Ident(ast::IdentKind::Rule) => {
                references.push(Reference {
                    span,
                    kind: RefKind::Rule(ctx.text(span).to_owned()),
                    scope: scopes.find(span),
                });
            }
            ast::Node::Ident(ast::IdentKind::Var(_) | ast::IdentKind::Macro(_)) => {
                references.push(Reference {
                    span,
                    kind: RefKind::Variable(ctx.text(span).to_owned()),
                    scope: scopes.find(span),
                });
            }
            // `expr::member` qualified access - could be base rule or import access.
            ast::Node::QualifiedAccess { obj, member } => {
                let member_name = ctx.text(*member).to_owned();
                // Check if the root of the access chain is an import variable.
                let path = collect_qualified_path(shared, ctx, *obj);
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
                    span: *member,
                    kind,
                    scope: scopes.find(*member),
                });
            }
            // `expr::fn_name(args)` qualified call - could be base or import.
            ast::Node::QualifiedCall(range) => {
                let (obj, name, _args) = shared.pools.get_qualified_call(*range);
                let name_span = shared.arena.span(name);
                let member_name = ctx.text(name_span).to_owned();
                let path = collect_qualified_path(shared, ctx, obj);
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
                    span: name_span,
                    kind,
                    scope: scopes.find(name_span),
                });
            }
            // Field access: `obj.field` - extract the field as an ObjectField ref.
            ast::Node::FieldAccess { obj, field } => {
                let obj_span = shared.arena.span(*obj);
                references.push(Reference {
                    span: *field,
                    kind: RefKind::ObjectField {
                        field: ctx.text(*field).to_owned(),
                        object: ctx.text(obj_span).to_owned(),
                    },
                    scope: scopes.find(*field),
                });
            }
            // inherit("path") or import("path") - the path string literal.
            ast::Node::ModuleRef { import, path, .. } => {
                references.push(Reference {
                    span: *path,
                    kind: if *import {
                        RefKind::ImportPath
                    } else {
                        RefKind::InheritPath
                    },
                    scope: None,
                });
            }
            // For-loop binding usage: resolve to the binding name via for_id.
            ast::Node::ForBinding { for_id, index, .. } => {
                let cfg = shared.pools.get_for(*for_id);
                let (binding_span, _) = cfg.bindings[*index as usize];
                references.push(Reference {
                    span,
                    kind: RefKind::Variable(ctx.text(binding_span).to_owned()),
                    scope: scopes.find(span),
                });
            }
            // Macro parameter usage: resolve to the param name via the
            // enclosing macro (looked up by ScopeIndex).
            ast::Node::MacroParam { index, .. } => {
                if let Some(scope) = scopes.find(span)
                    && let Some(macro_id) = scopes.macro_id_for_span(scope)
                {
                    let cfg = shared.pools.get_macro(macro_id);
                    let param_span = cfg.params[*index as usize].name;
                    references.push(Reference {
                        span,
                        kind: RefKind::Variable(ctx.text(param_span).to_owned()),
                        scope: Some(scope),
                    });
                }
            }
            _ => {}
        }
    }

    references
}

/// Extract LSP analysis data from a module's resolved AST.
///
/// `modules` contains all loaded modules (root + inherits + imports), produced
/// by the core's `Loader`. The root module is identified by `ctx`. Cross-module
/// info (`base_module`, `import_modules`) is extracted from `modules` rather
/// than re-parsing external files.
fn extract_analysis(
    tokens: &[nativedsl::lexer::Token],
    shared: &ast::SharedAst,
    modules: &[nativedsl::Module],
    ctx: &ast::ModuleContext,
) -> Analysis {
    let grammar_span = ctx
        .root_items
        .iter()
        .find(|&&id| matches!(shared.arena.get(id), ast::Node::Grammar))
        .map(|&id| shared.arena.span(id));

    let scopes = ScopeIndex::build(shared, ctx);
    let definitions = extract_definitions(shared, ctx, &scopes);
    let import_names = collect_import_names(shared, ctx);
    let mut references = extract_references(shared, ctx, &import_names, &scopes);
    extract_builtin_references(tokens, grammar_span, &mut references);

    // Find the inherited grammar module (if any) and extract its info.
    let base_module = ctx.inherit_ref.and_then(|inherit_id| {
        let ast::Node::ModuleRef {
            module: Some(idx), ..
        } = shared.arena.get(inherit_id)
        else {
            return None;
        };
        extract_external_module(shared, modules, *idx)
    });

    let import_modules = collect_import_modules(shared, modules, ctx);

    Analysis {
        source: ctx.source.clone(),
        rope: Rope::from_str(&ctx.source),
        tokens: Some(tokens.to_vec()),
        grammar_span,
        definitions: Some(definitions),
        references: Some(references),
        base_module,
        import_modules,
    }
}

/// Extract `ExternalModuleInfo` for a module loaded into `modules`. Recurses
/// through the module's own `let x = import(...)` bindings so nested chains
/// like `a::b::c` resolve.
fn extract_external_module(
    shared: &ast::SharedAst,
    modules: &[nativedsl::Module],
    idx: u8,
) -> Option<crate::document::ExternalModuleInfo> {
    let module = modules.get(idx as usize)?;
    let m_ctx = module.ctx();
    let scopes = ScopeIndex::build(shared, m_ctx);
    let defs = extract_definitions(shared, m_ctx, &scopes);
    let import_names = collect_import_names(shared, m_ctx);
    let refs = extract_references(shared, m_ctx, &import_names, &scopes);
    let import_modules = collect_import_modules(shared, modules, m_ctx);
    Some(crate::document::ExternalModuleInfo {
        path: m_ctx.path.clone(),
        definitions: defs,
        references: refs,
        rope: Rope::from_str(&m_ctx.source),
        import_modules,
    })
}

/// Collect `(binding_name, ExternalModuleInfo)` for each `let x = import(...)`
/// at the top level of `ctx`. Cycles are impossible here because the core
/// `Loader` rejects them before we get a successful module list.
fn collect_import_modules(
    shared: &ast::SharedAst,
    modules: &[nativedsl::Module],
    ctx: &ast::ModuleContext,
) -> Vec<(String, crate::document::ExternalModuleInfo)> {
    let mut out = Vec::new();
    for &item_id in &ctx.root_items {
        let ast::Node::Let { name, value, .. } = shared.arena.get(item_id) else {
            continue;
        };
        let ast::Node::ModuleRef {
            import: true,
            module: Some(idx),
            ..
        } = shared.arena.get(*value)
        else {
            continue;
        };
        if let Some(info) = extract_external_module(shared, modules, *idx) {
            out.push((ctx.text(*name).to_owned(), info));
        }
    }
    out
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
                    | TokenKind::KwMacro
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

fn build_fn_signature(
    ctx: &ast::ModuleContext,
    config: &ast::MacroConfig,
    fn_name: &str,
) -> String {
    let mut sig = format!("macro {fn_name}(");
    for (i, param) in config.params.iter().enumerate() {
        if i > 0 {
            sig.push_str(", ");
        }
        let _ = write!(sig, "{}: {}", ctx.text(param.name), param.ty);
    }
    let _ = write!(sig, ") {}", config.return_ty);
    sig
}

#[must_use]
pub fn uri_to_grammar_path(uri: &Url) -> PathBuf {
    uri.to_file_path()
        .unwrap_or_else(|()| PathBuf::from("grammar.tsg"))
}

/// Run the DSL pipeline on the given source text and return analysis data.
/// Uses the core's `Loader` to load all imports/inherits and run the full
/// resolve + typecheck pipeline on a single shared AST.
///
/// On any pipeline failure (parse, validate, resolve, typecheck), falls back
/// to manual lex+parse so partial analysis (definitions/references from the
/// root module) is still available for mid-keystroke features.
#[must_use]
#[expect(clippy::missing_panics_doc, reason = "file always has a parent")]
pub fn analyze(text: &str, uri: &Url) -> Analysis {
    let grammar_path = uri_to_grammar_path(uri);

    let source = text.to_owned();
    let rope = Rope::from_str(text);

    // Stage 1: Lex
    let Ok(tokens) = nativedsl::lexer::Lexer::new(text).tokenize() else {
        tracing::warn!("analyze: lex failed for {uri}");
        return Analysis {
            source,
            rope,
            ..Analysis::default()
        };
    };

    // Try the full Loader-based pipeline. This loads inherits/imports
    // recursively into one shared AST + TypeEnv, then resolves and
    // typechecks. On success we have fully resolved cross-module references.
    if let Some(canonical) = dunce::canonicalize(&grammar_path).ok() {
        let mut shared = ast::SharedAst::new(text.len() / 30);
        let mut modules: Vec<nativedsl::Module> = Vec::new();
        let mut env = nativedsl::typecheck::TypeEnv::default();
        let mut state = nativedsl::LoweringState::default();
        let mut loader = nativedsl::loader::Loader {
            shared: &mut shared,
            modules: &mut modules,
            env: &mut env,
            state: &mut state,
            ancestor_paths: vec![canonical.clone()],
        };
        if loader
            .load_module(text, &canonical, nativedsl::loader::ModuleKind::Grammar)
            .is_ok()
        {
            let root = modules.last().expect("root module pushed on success");
            return extract_analysis(&tokens, &shared, &modules, root.ctx());
        }
    }

    // Loader failed (parse error, missing file, validation error, type error).
    // Fall back to manual lex+parse so we still get partial analysis.
    let mut shared = ast::SharedAst::new(text.len() / 30);
    let Ok(module_ctx) =
        nativedsl::parser::Parser::new(&tokens, text.to_owned(), &grammar_path, &mut shared)
            .parse()
    else {
        tracing::warn!("analyze: parse failed for {uri}");
        return Analysis {
            source,
            rope,
            tokens: Some(tokens),
            ..Analysis::default()
        };
    };

    // Resolve what we can without loaded children. Imports/inherits won't
    // resolve, but local Ident -> RuleRef/VarRef rewrites will happen.
    let _ = nativedsl::resolve::resolve(&mut shared, &module_ctx, &[], None, &grammar_path);

    // Extract analysis from this single module (no loaded children).
    let modules: Vec<nativedsl::Module> = Vec::new();
    extract_analysis(&tokens, &shared, &modules, &module_ctx)
}

/// Run lex+parse and invoke `f` with the parsed AST. Returns `None` if either stage fails.
/// Use this when you need AST access but don't need type information.
pub fn with_ast<T>(
    text: &str,
    uri: &Url,
    f: impl FnOnce(&ast::SharedAst, &ast::ModuleContext) -> T,
) -> Option<T> {
    let grammar_path = uri_to_grammar_path(uri);
    let tokens = nativedsl::lexer::Lexer::new(text).tokenize().ok()?;
    let mut shared = ast::SharedAst::new(text.len() / 30);
    let module_ctx =
        nativedsl::parser::Parser::new(&tokens, text.to_owned(), &grammar_path, &mut shared)
            .parse()
            .ok()?;
    Some(f(&shared, &module_ctx))
}

/// Find the key span and value node of `<object_name>.<field_name>` by walking
/// the AST for a top-level `let <object_name> = { ... }` binding with an object
/// literal value.
#[must_use]
pub fn find_object_field(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    object_name: &str,
    field_name: &str,
) -> Option<(ast::Span, ast::NodeId)> {
    for &item_id in &ctx.root_items {
        if let ast::Node::Let { name, value, .. } = shared.arena.get(item_id)
            && ctx.text(*name) == object_name
            && let ast::Node::Object(range) = shared.arena.get(*value)
        {
            for &(key_span, value_id) in shared.pools.get_object(*range) {
                if ctx.text(key_span) == field_name {
                    return Some((key_span, value_id));
                }
            }
        }
    }
    None
}

/// Run the pipeline through typecheck on the given source text.
///
/// Calls `f` with the resolved SharedAst, ModuleContext, and type environment.
/// Uses the core's `load_module` for proper module loading, index tagging,
/// and recursive typecheck.
pub fn with_type_env<T>(
    text: &str,
    uri: &Url,
    f: impl FnOnce(&ast::SharedAst, &ast::ModuleContext, &nativedsl::typecheck::TypeEnv) -> T,
) -> Option<T> {
    let grammar_path = uri_to_grammar_path(uri);
    let canonical = dunce::canonicalize(&grammar_path).ok()?;
    let cap = text.len() / 30;
    let mut shared = ast::SharedAst::new(cap);
    let mut modules: Vec<nativedsl::Module> = Vec::new();
    let mut env = nativedsl::typecheck::TypeEnv::default();
    let mut state = nativedsl::LoweringState::default();
    let mut loader = nativedsl::loader::Loader {
        shared: &mut shared,
        modules: &mut modules,
        env: &mut env,
        state: &mut state,
        ancestor_paths: vec![canonical.clone()],
    };
    loader
        .load_module(text, &canonical, nativedsl::loader::ModuleKind::Grammar)
        .ok()?;
    drop(loader);
    let root = modules.last()?;
    Some(f(&shared, root.ctx(), &env))
}

#[cfg(test)]
mod bench {
    use super::*;

    #[test]
    #[ignore = "benchmark"]
    fn bench_analyze() {
        let path =
            std::path::PathBuf::from("/home/lillis/projects/grammars/tree-sitter-cpp/grammar.tsg");
        let Ok(source) = std::fs::read_to_string(&path) else {
            eprintln!("skipping: cpp grammar not found");
            return;
        };
        let uri = Url::from_file_path(&path).unwrap();

        let n = 200u32;

        // Lex (isolated)
        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(nativedsl::lexer::Lexer::new(&source).tokenize().unwrap());
        }
        let lex_time = start.elapsed() / n;

        // Full analyze
        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(analyze(&source, &uri));
        }
        let total_time = start.elapsed() / n;

        eprintln!(
            "=== analyze() on cpp grammar ({} lines) ===",
            source.lines().count()
        );
        eprintln!("Lex (isolated):    {lex_time:>8?}");
        eprintln!("Total analyze():   {total_time:>8?}");
    }
}

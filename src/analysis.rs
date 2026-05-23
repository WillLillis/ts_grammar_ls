use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use ropey::Rope;
use tower_lsp::lsp_types::Url;

use tree_sitter_generate::nativedsl::{self, ast, string_pool::StringPool};

use crate::document::{CfgFlag, DefKind, Definition, DisabledRegion, Module, RefKind, Reference};

// ---------------------------------------------------------------------------
// Analysis extraction - walk the AST to collect definitions and references
// ---------------------------------------------------------------------------

/// Extract definitions from an AST's root items. `env` (when present) is used
/// to attach inferred types to `Let` definitions.
fn extract_definitions(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    scopes: &ScopeIndex,
    env: Option<&nativedsl::typecheck::TypeEnv>,
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
                        kind: DefKind::Parameter {
                            scope: fn_span,
                            ty: param.ty,
                        },
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
                    _ => DefKind::Let {
                        scope: None,
                        ty: env.and_then(|e| e.vars.get(&item_id).copied()),
                    },
                };
                definitions.push(Definition {
                    name: ctx.text(*name).to_owned(),
                    kind,
                    name_span: *name,
                    full_span,
                });
                // Extract object field keys as definitions.
                if let ast::Node::Object(range) = shared.arena.get(*value) {
                    for &(key_span, value_id) in shared.pools.get_object(*range) {
                        let value_span = shared.arena.span(value_id);
                        definitions.push(Definition {
                            name: ctx.text(key_span).to_owned(),
                            kind: DefKind::ObjectKey { value_span },
                            name_span: key_span,
                            full_span: key_span,
                        });
                    }
                }
            }
            ast::Node::External { name } => {
                definitions.push(Definition {
                    name: ctx.text(*name).to_owned(),
                    kind: DefKind::External,
                    name_span: *name,
                    full_span: shared.arena.span(item_id),
                });
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
            for binding in &config.bindings {
                definitions.push(Definition {
                    name: ctx.text(binding.name).to_owned(),
                    kind: DefKind::Parameter {
                        scope,
                        ty: binding.ty,
                    },
                    name_span: binding.name,
                    full_span: binding.name,
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

    // Map each `import(...)` / `inherit(...)` node to its owning `let X = ...`
    // binding name, so the `ImportPath` / `InheritPath` reference can carry
    // it directly (avoids a span-containment reverse lookup at use sites).
    let mut module_ref_binding: rustc_hash::FxHashMap<ast::NodeId, String> =
        rustc_hash::FxHashMap::default();
    for &item_id in &ctx.root_items {
        if let ast::Node::Let { name, value, .. } = shared.arena.get(item_id)
            && matches!(shared.arena.get(*value), ast::Node::ModuleRef { .. })
        {
            module_ref_binding.insert(*value, ctx.text(*name).to_owned());
        }
    }

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
                let binding = module_ref_binding
                    .get(&node_id)
                    .cloned()
                    .unwrap_or_default();
                references.push(Reference {
                    span: *path,
                    kind: if *import {
                        RefKind::ImportPath(binding)
                    } else {
                        RefKind::InheritPath(binding)
                    },
                    scope: None,
                });
            }
            // For-loop binding usage: resolve to the binding name via for_id.
            ast::Node::ForBinding { for_id, index, .. } => {
                let cfg = shared.pools.get_for(*for_id);
                let binding_span = cfg.bindings[*index as usize].name;
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

/// How a `Module` is being extracted - root document vs. an inherited /
/// imported external. The root carries lex tokens and a loader-status flag;
/// externals get neither (no tokens stored, `loader_succeeded` forced true
/// since externals only exist after their own loader succeeded).
enum ExtractKind<'a> {
    Root {
        /// Tokens are stored on the resulting `Module` (moved, not copied).
        tokens: Vec<nativedsl::lexer::Token>,
        loader_succeeded: bool,
        /// Resolved type environment, when the loader pipeline succeeded.
        /// `None` on the manual-parse fallback path - hover will then show
        /// `let foo` without a type.
        env: Option<&'a nativedsl::typecheck::TypeEnv>,
        /// Cfg state from the loader pass: declared flag names + active set.
        /// `None` on the manual-parse fallback (apply_cfg never ran).
        cfg: Option<&'a nativedsl::apply_cfg::CfgState>,
    },
    External {
        /// Shared type environment from the (successful) loader pass.
        env: Option<&'a nativedsl::typecheck::TypeEnv>,
    },
}

/// Extract a `Module` from a resolved AST. `modules` contains all loaded
/// modules (root + inherits + imports), produced by the core's `Loader`;
/// cross-module info (`base_module`, `import_modules`) is extracted from
/// `modules` rather than re-parsing external files.
fn extract_module(
    shared: &Arc<ast::SharedAst>,
    modules: &[nativedsl::Module],
    ctx: &ast::ModuleContext,
    kind: ExtractKind<'_>,
) -> Module {
    let grammar_span = ctx
        .root_items
        .iter()
        .find(|&&id| matches!(shared.arena.get(id), ast::Node::Grammar))
        .map(|&id| shared.arena.span(id));

    let env = match kind {
        ExtractKind::Root { env, .. } => env,
        ExtractKind::External { env } => env,
    };
    let shared_ref: &ast::SharedAst = shared;
    let scopes = ScopeIndex::build(shared_ref, ctx);
    let definitions = extract_definitions(shared_ref, ctx, &scopes, env);
    let import_names = collect_import_names(shared_ref, ctx);
    let mut references = extract_references(shared_ref, ctx, &import_names, &scopes);

    // Find the inherited grammar module (if any) and extract its info.
    let base_module = ctx.inherit_ref.and_then(|inherit_id| {
        let ast::Node::ModuleRef {
            module: Some(idx), ..
        } = shared_ref.arena.get(inherit_id)
        else {
            return None;
        };
        extract_external_at(shared, modules, *idx, env).map(Box::new)
    });

    let import_modules = collect_import_modules(shared, modules, ctx, env);

    let (tokens_field, loader_succeeded, disabled_regions, declared_cfg_flags) = match kind {
        ExtractKind::Root {
            tokens,
            loader_succeeded,
            cfg,
            ..
        } => {
            extract_builtin_references(&tokens, grammar_span, &mut references);
            let declared = cfg.map(cfg_flag_list).unwrap_or_default();
            // Disabled cfg sites are recoverable from the post-apply arena
            // even when `cfg` itself isn't available, but we gate on it to
            // ensure we're only doing this when the loader actually ran.
            let regions = if cfg.is_some() {
                scan_disabled_cfg_regions(shared_ref, ctx)
            } else {
                Vec::new()
            };
            (Some(tokens), loader_succeeded, regions, declared)
        }
        ExtractKind::External { .. } => (None, true, Vec::new(), Vec::new()),
    };

    Module {
        path: ctx.path.clone(),
        source: ctx.source.clone(),
        rope: Rope::from_str(&ctx.source),
        tokens: tokens_field,
        grammar_span,
        definitions: Some(definitions),
        references: Some(references),
        base_module,
        import_modules,
        loader_succeeded,
        disabled_regions,
        declared_cfg_flags,
        shared: Arc::clone(shared),
    }
}

/// Extract an external (inherit/import) `Module` at index `idx` in `modules`.
fn extract_external_at(
    shared: &Arc<ast::SharedAst>,
    modules: &[nativedsl::Module],
    idx: u8,
    env: Option<&nativedsl::typecheck::TypeEnv>,
) -> Option<Module> {
    let module = modules.get(idx as usize)?;
    Some(extract_module(
        shared,
        modules,
        module.ctx(),
        ExtractKind::External { env },
    ))
}

/// Collect `(binding_name, Module)` for each `let x = import(...)` at the top
/// level of `ctx`. Cycles are impossible here because the core `Loader`
/// rejects them before we get a successful module list.
fn collect_import_modules(
    shared: &Arc<ast::SharedAst>,
    modules: &[nativedsl::Module],
    ctx: &ast::ModuleContext,
    env: Option<&nativedsl::typecheck::TypeEnv>,
) -> Vec<(String, Module)> {
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
        if let Some(info) = extract_external_at(shared, modules, *idx, env) {
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

/// Flatten the cfg state's declared-flag set into a stable-sorted vector of
/// `CfgFlag` records (name + enabled state). Used by hover and completion on
/// `#[cfg(...)]` flag names.
fn cfg_flag_list(cfg: &nativedsl::apply_cfg::CfgState) -> Vec<CfgFlag> {
    let mut flags: Vec<CfgFlag> = cfg
        .declared_any
        .iter()
        .map(|name| CfgFlag {
            name: name.clone(),
            enabled: cfg.active.get(name).copied().unwrap_or(false),
        })
        .collect();
    flags.sort_by(|a, b| a.name.cmp(&b.name));
    flags
}

/// Walk the post-apply-cfg AST for `Node::Cfg` nodes. After `apply_cfg`,
/// active cfg sites get overwritten in place with their child's data, while
/// disabled cfg sites are simply skipped (filtered from list ranges, etc.) -
/// the arena node itself is left intact at its original NodeId with its full
/// `#[cfg(NAME)] ITEM` source span. So every surviving `Node::Cfg` in the
/// arena is exactly one disabled cfg site, covering both top-level and inline
/// uses uniformly.
fn scan_disabled_cfg_regions(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
) -> Vec<DisabledRegion> {
    let mut out = Vec::new();
    for (node_id, node) in ctx.iter_own_nodes(&shared.arena) {
        if let ast::Node::Cfg { name, .. } = node {
            let full_span = shared.arena.span(node_id);
            out.push(DisabledRegion {
                name: ctx.text(*name).to_owned(),
                name_span: *name,
                full_span,
            });
        }
    }
    out
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
    // `MacroKind::Expression(ty)` carries the return type; rule-set macros
    // don't have one - they expand to top-level decls instead.
    match config.kind {
        ast::MacroKind::Expression(ty) => {
            let _ = write!(sig, ") {ty}");
        }
        ast::MacroKind::RuleSet => {
            sig.push_str(") rule set");
        }
    }
    sig
}

/// On-disk path for a `file://` URI. `None` for non-file URIs (e.g. `untitled:`).
#[must_use]
pub fn uri_to_grammar_path(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

/// Cheap lex+parse to extract the canonical paths of every file the grammar
/// at `file_path` would inherit or import. Used by the workspace scanner to
/// build a dep graph for *closed* files (so cross-file rename can reach them
/// without opening every grammar in the workspace). Resolves relative paths
/// against `file_path`'s directory and canonicalizes via `dunce`. Files that
/// don't canonicalize (missing on disk, broken symlinks) are silently
/// dropped - they'd fail the real loader anyway.
#[must_use]
pub fn extract_deps(text: &str, file_path: &std::path::Path) -> Vec<PathBuf> {
    let Ok(tokens) = nativedsl::lexer::Lexer::new(text).tokenize() else {
        return Vec::new();
    };
    let mut shared = ast::SharedAst::new(text.len() / 30);
    let Ok(ctx) =
        nativedsl::parser::Parser::new(&tokens, text.to_owned(), file_path.to_path_buf(), &mut shared)
            .parse()
    else {
        return Vec::new();
    };
    let module_dir = file_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""));
    let mut seen = rustc_hash::FxHashSet::default();
    let mut deps = Vec::new();
    for &node_id in &ctx.module_refs {
        if let ast::Node::ModuleRef { path, .. } = shared.arena.get(node_id) {
            let path_str = ctx.text(*path);
            let resolved = module_dir.join(path_str);
            if let Ok(canonical) = dunce::canonicalize(&resolved)
                && seen.insert(canonical.clone())
            {
                deps.push(canonical);
            }
        }
    }
    deps
}

/// One end-to-end analyze pass yields both the LSP-side `Module` (used by
/// every handler) and the pipeline `Result` (used by `publish_dsl_diagnostics`
/// for error reporting and by `spawn_generate_check` for the parsed grammar).
/// Bundling them lets us run the loader once per `did_change` instead of
/// twice (one in diagnostics, one in handlers).
pub struct AnalyzeOutcome {
    pub module: Module,
    /// `Some(Ok(grammar))` on full loader success.
    /// `Some(Err(error))` when the loader ran and produced a pipeline error.
    /// `None` when the loader didn't run (e.g. path couldn't be canonicalized)
    /// - the Module is still populated from the manual-parse fallback, but
    /// there's no pipeline error to surface as a diagnostic.
    pub pipeline: Option<Result<nativedsl::InputGrammar, nativedsl::DslError>>,
}

/// Run the DSL pipeline on the given source text and return analysis data
/// plus the pipeline outcome.
///
/// Returns `None` for URIs that aren't backed by a real file (e.g.
/// `untitled:`) - those can't anchor relative paths so there's nothing
/// meaningful to analyze. On pipeline failure (parse, validate, resolve,
/// typecheck) we fall back to manual lex+parse so partial analysis
/// (definitions/references from the root module) is still available for
/// mid-keystroke features; the `pipeline` field carries the original error.
#[must_use]
pub fn analyze(text: String, uri: &Url) -> Option<AnalyzeOutcome> {
    let grammar_path = uri_to_grammar_path(uri)?;

    // Stage 1: Lex
    let tokens = match nativedsl::lexer::Lexer::new(&text).tokenize() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("analyze: lex failed for {uri}");
            let rope = Rope::from_str(&text);
            return Some(AnalyzeOutcome {
                module: Module::empty(grammar_path, text, rope),
                pipeline: Some(Err(e.into())),
            });
        }
    };

    // Try the full Loader-based pipeline. On success we have fully resolved
    // cross-module references and a lowered grammar. On failure we capture
    // the error and fall through to a manual-parse fallback that produces
    // a partial Module so handlers keep working mid-keystroke.
    let loader_result = run_loader_pipeline(&text, &grammar_path, &tokens);
    if let Some(outcome) = loader_result {
        return Some(outcome);
    }

    // No file path / canonicalize failure - drop straight to manual parse.
    manual_parse_fallback(text, tokens, grammar_path, uri, None)
}

/// Run the loader (full pipeline) and produce an `AnalyzeOutcome` if the
/// loader was invocable. Returns `None` only when the path can't be
/// canonicalized; otherwise always returns Some - either with the
/// fully-resolved Module + grammar on success, or with the manual-parse
/// fallback Module carrying the pipeline error.
fn run_loader_pipeline(
    text: &str,
    grammar_path: &std::path::Path,
    tokens: &[nativedsl::lexer::Token],
) -> Option<AnalyzeOutcome> {
    let canonical = dunce::canonicalize(grammar_path).ok()?;
    let mut shared = ast::SharedAst::new(text.len() / 30);
    let mut modules: Vec<nativedsl::Module> = Vec::new();
    let mut env = nativedsl::typecheck::TypeEnv::default();
    let mut state = nativedsl::LoweringState::default();
    let mut strings = StringPool::default();
    let mut cfg = nativedsl::apply_cfg::CfgState::default();
    let mut loader = nativedsl::loader::Loader {
        shared: &mut shared,
        modules: &mut modules,
        env: &mut env,
        state: &mut state,
        strings: &mut strings,
        cfg: &mut cfg,
        ancestor_paths: vec![canonical.clone()],
        loaded: Vec::new(),
    };
    let load_result = loader.load_module(text, &canonical, nativedsl::loader::ModuleKind::Grammar);
    drop(loader);

    if load_result.is_ok() {
        // Pop the root grammar so we can take its lowered InputGrammar by
        // value while keeping the rest of `modules` for cross-module
        // resolution (child indices are unchanged).
        let root = modules.pop().expect("root module pushed on success");
        let (ctx, lowered) = match root {
            nativedsl::Module::Grammar { ctx, lowered } => (ctx, *lowered),
            nativedsl::Module::Helper { .. } => {
                unreachable!("root module must be Grammar")
            }
        };
        // Lift the AST arena into an `Arc` now that the loader has stopped
        // mutating it; every extracted Module shares the same handle.
        let shared = Arc::new(shared);
        let module = extract_module(
            &shared,
            &modules,
            &ctx,
            ExtractKind::Root {
                tokens: tokens.to_vec(),
                loader_succeeded: true,
                env: Some(&env),
                cfg: Some(&cfg),
            },
        );
        return Some(AnalyzeOutcome {
            module,
            pipeline: Some(Ok(lowered)),
        });
    }

    // Loader failed - fall back to manual parse and surface the error.
    let pipeline_err = load_result.expect_err("checked above");
    let uri = Url::from_file_path(grammar_path).ok()?;
    Some(manual_parse_fallback(
        text.to_owned(),
        tokens.to_vec(),
        grammar_path.to_path_buf(),
        &uri,
        Some(pipeline_err),
    )?)
}

/// Manual lex+parse fallback for when the full loader pipeline fails. Returns
/// an `AnalyzeOutcome` with the partial Module and the supplied pipeline
/// error (or a synthesized one if the manual parse also fails).
fn manual_parse_fallback(
    text: String,
    tokens: Vec<nativedsl::lexer::Token>,
    grammar_path: PathBuf,
    uri: &Url,
    pipeline_err: Option<nativedsl::DslError>,
) -> Option<AnalyzeOutcome> {
    let mut shared = ast::SharedAst::new(text.len() / 30);
    let module_ctx = match nativedsl::parser::Parser::new(
        &tokens,
        text.clone(),
        grammar_path.clone(),
        &mut shared,
    )
    .parse()
    {
        Ok(ctx) => ctx,
        Err(parse_err) => {
            tracing::warn!("analyze: parse failed for {uri}");
            let rope = Rope::from_str(&text);
            let mut module = Module::empty(grammar_path, text, rope);
            module.tokens = Some(tokens);
            return Some(AnalyzeOutcome {
                module,
                // Prefer the original pipeline error if we have one;
                // otherwise surface the parse failure.
                pipeline: Some(Err(pipeline_err.unwrap_or_else(|| parse_err.into()))),
            });
        }
    };

    // Resolve what we can without loaded children. Imports/inherits won't
    // resolve, but local Ident -> RuleRef/VarRef rewrites will happen.
    // `expand_macro_calls` doesn't run on this fallback path so no
    // `SynthRef` / `ExpandedRule` nodes are produced - a default pool is
    // enough to satisfy resolve's signature.
    let strings = StringPool::default();
    let _ = nativedsl::resolve::resolve(&mut shared, &module_ctx, &strings, &[], None);

    let modules: Vec<nativedsl::Module> = Vec::new();
    let shared = Arc::new(shared);
    let module = extract_module(
        &shared,
        &modules,
        &module_ctx,
        ExtractKind::Root {
            tokens,
            loader_succeeded: false,
            env: None,
            cfg: None,
        },
    );
    Some(AnalyzeOutcome {
        module,
        // `None` reflects "the loader never ran" (canonicalize failed).
        // The manual-parse fallback still produced a partial Module.
        pipeline: pipeline_err.map(Err),
    })
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
            std::hint::black_box(analyze(source.clone(), &uri));
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

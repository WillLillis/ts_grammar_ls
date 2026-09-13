use std::path::PathBuf;
use std::sync::Arc;
use std::{fmt::Write as _, path::Path};

use ropey::Rope;
use tower_lsp::lsp_types::Url;

use tree_sitter_generate::nativedsl::{self, DocumentMap, RulePool, StrPool, ast};

fn span_text(source: &str, span: ast::Span) -> &str {
    &source[span.start as usize..span.end as usize]
}

/// Construct the one-document store required by core's public lexer/parser
/// APIs.
#[doc(hidden)]
#[must_use]
pub fn document_map_for_source(path: &Path, source: &str) -> (DocumentMap, nativedsl::DocumentId) {
    let mut documents = DocumentMap::default();
    let id = documents.insert(path.to_path_buf(), source.to_owned());
    (documents, id)
}

use crate::document::{
    CfgFlag, ConflictDecl, DefKind, Definition, DisabledRegion, Module, RefKind, Reference,
    StringTable,
};

// ---------------------------------------------------------------------------
// Analysis extraction - walk the AST to collect definitions and references
// ---------------------------------------------------------------------------

/// Push a definition for each key in an object literal (`{ ADD: 1, ... }`) so
/// hover / goto-definition land on the field names.
fn push_object_key_defs(
    shared: &ast::SharedAst,
    source: &str,
    range: ast::ChildRange,
    out: &mut Vec<Definition>,
) {
    for &ast::ObjectField {
        name: key,
        value: value_id,
    } in shared.pools.get_object(range)
    {
        out.push(Definition {
            name: span_text(source, key.span).to_owned(),
            kind: DefKind::ObjectKey {
                value_span: shared.arena.span(value_id),
            },
            name_span: key.span,
            full_span: key.span,
        });
    }
}

/// Name span for a `<keyword..> <name>` declaration.
///
/// `Node::Rule` / `Let` / `Forward` intern their name and the arena keeps only
/// one span per node - the whole declaration - so the name's own span has to be
/// recovered by counting identifiers past the leading keywords. Core never
/// needs this (it reports duplicates at declaration granularity); only the LSP
/// does, because rename emits a `TextEdit` range and goto-def a `Location`.
///
/// `expected` is the name resolved from the node's `StrId`, and the recovered
/// span is checked against it. That turns the scan's one real fragility -
/// silently landing on the wrong identifier if a declaration ever grows a form
/// where an identifier can precede the name - into a visible fallback to the
/// declaration span. Also covers the mid-keystroke case where the source no
/// longer matches the last good parse.
fn decl_name_span(source: &str, full_span: ast::Span, index: usize, expected: &str) -> ast::Span {
    crate::text::nth_ident_span(source, full_span.start, index)
        .filter(|s| source.get(s.start as usize..s.end as usize) == Some(expected))
        .unwrap_or(full_span)
}

/// Recover a checked let's type from the resolved AST. This is deliberately a
/// read-only evaluator, not a second typechecker: annotations and embedded
/// node types are authoritative, and invalid combinations simply return
/// `None` (core already owns validation and diagnostics).
fn infer_let_type(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    id: ast::NodeId,
    cache: &mut rustc_hash::FxHashMap<ast::NodeId, nativedsl::Ty>,
    visiting: &mut rustc_hash::FxHashSet<ast::NodeId>,
) -> Option<nativedsl::Ty> {
    if let Some(&ty) = cache.get(&id) {
        return Some(ty);
    }
    if let Some(&ty) = ctx.let_types.get(&id) {
        cache.insert(id, ty);
        return Some(ty);
    }
    if !visiting.insert(id) {
        return None;
    }
    let ty = match *shared.arena.get(id) {
        ast::Node::Let { value, .. } => infer_expr_type(shared, ctx, value, cache, visiting),
        _ => None,
    };
    visiting.remove(&id);
    if let Some(ty) = ty {
        cache.insert(id, ty);
    }
    ty
}

fn infer_common_type(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    ids: &[ast::NodeId],
    cache: &mut rustc_hash::FxHashMap<ast::NodeId, nativedsl::Ty>,
    visiting: &mut rustc_hash::FxHashSet<ast::NodeId>,
) -> Option<nativedsl::Ty> {
    let mut ids = ids.iter().copied();
    let first = infer_expr_type(shared, ctx, ids.next()?, cache, visiting)?;
    ids.try_fold(first, |current, item| {
        current.widen(infer_expr_type(shared, ctx, item, cache, visiting)?)
    })
}

#[allow(clippy::too_many_lines)]
fn infer_expr_type(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    id: ast::NodeId,
    cache: &mut rustc_hash::FxHashMap<ast::NodeId, nativedsl::Ty>,
    visiting: &mut rustc_hash::FxHashSet<ast::NodeId>,
) -> Option<nativedsl::Ty> {
    use nativedsl::{DataTy, InnerTy, ScalarTy, Ty};

    match *shared.arena.get(id) {
        ast::Node::StringLit(_) | ast::Node::Concat(_) => Some(Ty::STR),
        ast::Node::IntLit(_) | ast::Node::Neg(_) | ast::Node::BinOp { .. } => Some(Ty::INT),
        ast::Node::Ident(ast::IdentKind::Var(let_id)) => {
            infer_let_type(shared, ctx, let_id, cache, visiting)
        }
        ast::Node::Import {
            module: Some(module),
            ..
        } => Some(Ty::Module(nativedsl::ModuleTy::Library(module))),
        ast::Node::Inherit {
            module: Some(module),
            ..
        } => Some(Ty::Module(nativedsl::ModuleTy::Grammar(module))),
        ast::Node::Object(range) => {
            let mut values = shared
                .pools
                .get_object(range)
                .iter()
                .map(|field| field.value);
            let first = infer_expr_type(shared, ctx, values.next()?, cache, visiting)?;
            let common = values.try_fold(first, |current, value| {
                current.widen(infer_expr_type(shared, ctx, value, cache, visiting)?)
            })?;
            let Ty::Data(data) = common else {
                return None;
            };
            Some(Ty::Data(DataTy::Object(InnerTy::try_from(data).ok()?)))
        }
        ast::Node::List(range) => infer_common_type(
            shared,
            ctx,
            shared.pools.child_slice(range),
            cache,
            visiting,
        )?
        .to_list(),
        ast::Node::Tuple(range) => {
            let scalars: Option<Vec<ScalarTy>> = shared
                .pools
                .child_slice(range)
                .iter()
                .map(
                    |&child| match infer_expr_type(shared, ctx, child, cache, visiting) {
                        Some(Ty::Data(DataTy::Scalar(scalar))) => Some(scalar),
                        _ => None,
                    },
                )
                .collect();
            Some(Ty::Data(DataTy::Tuple(
                nativedsl::TupleSig::new(&scalars?).ok()?,
            )))
        }
        ast::Node::FieldAccess { obj, .. } => infer_expr_type(shared, ctx, obj, cache, visiting)?
            .object_inner()
            .map(Ty::from),
        ast::Node::Append { left, right } => {
            let left = infer_expr_type(shared, ctx, left, cache, visiting);
            let right = infer_expr_type(shared, ctx, right, cache, visiting);
            match (left, right) {
                (Some(left), Some(right)) if left.is_list() && right.is_list() => left.widen(right),
                (Some(ty), None) | (None, Some(ty)) if ty.is_list() => Some(ty),
                _ => None,
            }
        }
        ast::Node::GrammarConfig { field, .. } => {
            use ast::ConfigField as C;
            Some(match field {
                C::Language => Ty::STR,
                C::Extras | C::Externals | C::Inline | C::Supertypes => Ty::LIST_RULE,
                C::Conflicts | C::Precedences => Ty::LIST_LIST_RULE,
                C::Word | C::Start => Ty::RULE,
                C::Reserved => Ty::OBJ_LIST_RULE,
                C::Inherits | C::Flags => return None,
            })
        }
        ast::Node::MacroParam { ty, .. } | ast::Node::ForBinding { ty, .. } => Some(ty),
        ast::Node::Call { name, .. } => {
            let ast::Node::Ident(ast::IdentKind::Macro(macro_id)) = shared.arena.get(name) else {
                return None;
            };
            match shared.pools.get_macro(*macro_id).kind {
                ast::MacroKind::Expression(ty) => Some(ty),
                ast::MacroKind::RuleSet => None,
            }
        }
        ast::Node::Ident(ast::IdentKind::Rule(_))
        | ast::Node::ModuleRule { .. }
        | ast::Node::SymRef { .. }
        | ast::Node::SeqOrChoice { .. }
        | ast::Node::Repeat { .. }
        | ast::Node::Blank
        | ast::Node::Eof
        | ast::Node::Field { .. }
        | ast::Node::Alias { .. }
        | ast::Node::Token { .. }
        | ast::Node::Prec { .. }
        | ast::Node::Reserved { .. }
        | ast::Node::DynRegex { .. } => Some(Ty::RULE),
        ast::Node::For { body, .. } => infer_expr_type(shared, ctx, body, cache, visiting),
        ast::Node::Cfg { child, .. } => infer_expr_type(shared, ctx, child, cache, visiting),
        _ => None,
    }
}

fn extract_definitions(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    source: &str,
    scopes: &ScopeIndex,
    strings: &StringTable,
) -> Vec<Definition> {
    let mut definitions = Vec::new();
    let mut inferred_types = rustc_hash::FxHashMap::default();
    let mut visiting_lets = rustc_hash::FxHashSet::default();
    for &item_id in &ctx.root_items {
        match shared.arena.get(item_id) {
            ast::Node::Rule {
                is_override, name, ..
            } => {
                let full_span = shared.arena.span(item_id);
                let text = strings.get(*name);
                definitions.push(Definition {
                    name: text.to_owned(),
                    kind: if *is_override {
                        DefKind::OverrideRule
                    } else {
                        DefKind::Rule
                    },
                    name_span: decl_name_span(
                        source,
                        full_span,
                        usize::from(*is_override) + 1,
                        text,
                    ),
                    full_span,
                });
            }
            ast::Node::Macro(macro_id) => {
                let config = shared.pools.get_macro(*macro_id);
                let fn_name = span_text(source, config.name.span);
                let signature = build_fn_signature(source, &shared.pools, config, fn_name);
                let fn_span = shared.arena.span(item_id);
                definitions.push(Definition {
                    name: fn_name.to_owned(),
                    kind: DefKind::Function { signature },
                    name_span: config.name.span,
                    full_span: fn_span,
                });
                // Extract parameters as scoped definitions.
                for param in shared.pools.param_slice(config.params) {
                    definitions.push(Definition {
                        name: span_text(source, param.name.span).to_owned(),
                        kind: DefKind::Parameter {
                            scope: fn_span,
                            ty: param.ty,
                        },
                        name_span: param.name.span,
                        full_span: param.name.span,
                    });
                }
            }
            ast::Node::Let { name, value, .. } => {
                let full_span = shared.arena.span(item_id);
                let kind = match shared.arena.get(*value) {
                    ast::Node::Import { .. } => DefKind::Import,
                    ast::Node::Inherit { .. } => DefKind::Inherit,
                    _ => DefKind::Let {
                        scope: None,
                        ty: infer_let_type(
                            shared,
                            ctx,
                            item_id,
                            &mut inferred_types,
                            &mut visiting_lets,
                        ),
                    },
                };
                let text = strings.get(*name);
                definitions.push(Definition {
                    name: text.to_owned(),
                    kind,
                    name_span: decl_name_span(source, full_span, 1, text),
                    full_span,
                });
                // Extract object field keys as definitions.
                if let ast::Node::Object(range) = shared.arena.get(*value) {
                    push_object_key_defs(shared, source, *range, &mut definitions);
                }
            }
            ast::Node::Forward { name } => {
                let full_span = shared.arena.span(item_id);
                let text = strings.get(*name);
                definitions.push(Definition {
                    name: text.to_owned(),
                    kind: DefKind::Forward,
                    name_span: decl_name_span(source, full_span, 1, text),
                    full_span,
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
            for binding in shared.pools.param_slice(config.bindings) {
                definitions.push(Definition {
                    name: span_text(source, binding.name.span).to_owned(),
                    kind: DefKind::Parameter {
                        scope,
                        ty: binding.ty,
                    },
                    name_span: binding.name.span,
                    full_span: binding.name.span,
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
        scopes.sort_unstable_by_key(|b| std::cmp::Reverse(b.start));
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
    strings: &StringTable,
) -> rustc_hash::FxHashSet<String> {
    let mut names = rustc_hash::FxHashSet::default();
    for &item_id in &ctx.root_items {
        if let ast::Node::Let { name, value, .. } = shared.arena.get(item_id)
            && matches!(shared.arena.get(*value), ast::Node::Import { .. })
        {
            names.insert(strings.get(*name).to_owned());
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
    source: &str,
    strings: &StringTable,
    obj_id: ast::NodeId,
) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = obj_id;
    loop {
        match shared.arena.get(current) {
            ast::Node::Ident(_) => {
                // Walking tail-to-root, so push segments in reverse.
                let text = span_text(source, shared.arena.span(current));
                for part in text.rsplit("::") {
                    path.push(part.trim().to_owned());
                }
                break;
            }
            ast::Node::QualifiedAccess { obj, member, .. } => {
                path.push(strings.get(*member).to_owned());
                current = *obj;
            }
            _ => break,
        }
    }
    path.reverse();
    path
}

/// Build a reference to a `::`-qualified member. It's an [`RefKind::ImportedMember`]
/// when the chain's root names an `import` binding, otherwise an
/// [`RefKind::BaseRule`] (an inherited grammar's rule). Shared by the
/// `QualifiedAccess`, qualified calls, and resolved `ModuleRule` arms.
fn qualified_member_reference(
    path: Vec<String>,
    member: String,
    member_span: ast::Span,
    import_names: &rustc_hash::FxHashSet<String>,
    scopes: &ScopeIndex,
) -> Reference {
    let kind = if path.first().is_some_and(|root| import_names.contains(root)) {
        RefKind::ImportedMember { path, member }
    } else {
        RefKind::BaseRule(member)
    };
    Reference {
        span: member_span,
        kind,
        scope: scopes.find(member_span),
    }
}

/// Reconstruct the cross-module rule reference carried by a resolved
/// `Node::ModuleRule`.
///
/// `resolve` rewrites a `mod::name` `QualifiedAccess` in place into a
/// `ModuleRule` once it finds the target rule in another module's lowered
/// output, so the original `obj`/`member` `NodeId`s are gone - but `arena.set`
/// preserves the source span. The path and member are recovered from the span
/// `text`, mirroring the `QualifiedAccess` arm so cross-module goto-definition
/// and find-references keep working. (The `mod` prefix's own `Ident` node is
/// orphaned but still iterated by `iter_own_nodes`, so its `Variable` reference
/// survives independently; only the member reference is re-emitted here.)
///
/// Returns `None` when `text` isn't a qualified form (no `::`).
fn module_rule_reference(
    text: &str,
    span: ast::Span,
    import_names: &rustc_hash::FxHashSet<String>,
    scopes: &ScopeIndex,
) -> Option<Reference> {
    let sep = text.rfind("::")?;
    let member_rel = sep + "::".len();
    let member = text[member_rel..].trim().to_owned();
    let path: Vec<String> = text[..sep]
        .split("::")
        .map(|s| s.trim().to_owned())
        .collect();
    // Tighten the span to just the member identifier (skip any whitespace after
    // `::`), matching the precision of the `QualifiedAccess` arm's `*member` span.
    let lead_ws = text[member_rel..].len() - text[member_rel..].trim_start().len();
    let member_start = span.start + u32::try_from(member_rel + lead_ws).unwrap_or(0);
    let member_span = ast::Span::new(member_start, span.end);
    Some(qualified_member_reference(
        path,
        member,
        member_span,
        import_names,
        scopes,
    ))
}

/// Map each `import(...)` / `inherit(...)` node to its owning `let X = ...`
/// binding name, so the `ImportPath` / `InheritPath` reference can carry it
/// directly (avoids a span-containment reverse lookup at use sites).
fn module_ref_bindings(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    strings: &StringTable,
) -> rustc_hash::FxHashMap<ast::NodeId, String> {
    let mut bindings = rustc_hash::FxHashMap::default();
    for &item_id in &ctx.root_items {
        if let ast::Node::Let { name, value, .. } = shared.arena.get(item_id)
            && matches!(
                shared.arena.get(*value),
                ast::Node::Import { .. } | ast::Node::Inherit { .. }
            )
        {
            bindings.insert(*value, strings.get(*name).to_owned());
        }
    }
    bindings
}

/// Extract resolved references from all nodes in the AST.
fn extract_references(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    source: &str,
    import_names: &rustc_hash::FxHashSet<String>,
    scopes: &ScopeIndex,
    strings: &StringTable,
) -> Vec<Reference> {
    let mut references = Vec::new();
    let module_ref_binding = module_ref_bindings(shared, ctx, strings);

    for (node_id, node) in ctx.iter_own_nodes(&shared.arena) {
        let span = shared.arena.span(node_id);
        match node {
            ast::Node::Ident(ast::IdentKind::Rule(_)) => {
                references.push(Reference {
                    span,
                    kind: RefKind::Rule(span_text(source, span).to_owned()),
                    scope: scopes.find(span),
                });
            }
            ast::Node::Ident(ast::IdentKind::Var(_) | ast::IdentKind::Macro(_)) => {
                let text = span_text(source, span);
                if let Some(reference) = module_rule_reference(text, span, import_names, scopes) {
                    references.push(reference);
                } else {
                    references.push(Reference {
                        span,
                        kind: RefKind::Variable(text.to_owned()),
                        scope: scopes.find(span),
                    });
                }
            }
            // `expr::member` qualified access - could be base rule or import access.
            ast::Node::QualifiedAccess {
                obj,
                member,
                member_offset,
            } => {
                let path = collect_qualified_path(shared, source, strings, *obj);
                let member_text = strings.get(*member);
                let member_span = ast::Span::new(
                    *member_offset,
                    member_offset.saturating_add(member_text.len() as u32),
                );
                references.push(qualified_member_reference(
                    path,
                    member_text.to_owned(),
                    member_span,
                    import_names,
                    scopes,
                ));
            }
            // `mod::name` cross-module rule reference, reconstructed from the
            // resolved `ModuleRule`. See [`module_rule_reference`].
            ast::Node::ModuleRule { .. } => {
                references.extend(module_rule_reference(
                    span_text(source, span),
                    span,
                    import_names,
                    scopes,
                ));
            }
            // Field access: `obj.field` - extract the field as an ObjectField ref.
            // `field` is interned, so recover its span from the node's: the
            // parser builds it as `obj_span.merge(field_span)`, so it ends
            // exactly at the field name. Taking the trailing `len` bytes is
            // also right for `obj.r#let`, whose token span covers only `let`.
            ast::Node::FieldAccess { obj, field } => {
                let obj_span = shared.arena.span(*obj);
                let field_len = strings.get(*field).len() as u32;
                let field_span = ast::Span::new(span.end.saturating_sub(field_len), span.end);
                references.push(Reference {
                    span: field_span,
                    kind: RefKind::ObjectField {
                        field: strings.get(*field).to_owned(),
                        object: span_text(source, obj_span).to_owned(),
                    },
                    scope: scopes.find(field_span),
                });
            }
            // inherit("path") or import("path") - the path string literal.
            ast::Node::Import { path, .. } | ast::Node::Inherit { path, .. } => {
                let binding = module_ref_binding
                    .get(&node_id)
                    .cloned()
                    .unwrap_or_default();
                references.push(Reference {
                    span: *path,
                    kind: if matches!(node, ast::Node::Import { .. }) {
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
                let binding_span = shared.pools.param_slice(cfg.bindings)[*index as usize]
                    .name
                    .span;
                references.push(Reference {
                    span,
                    kind: RefKind::Variable(span_text(source, binding_span).to_owned()),
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
                    let param_span = shared.pools.param_slice(cfg.params)[*index as usize]
                        .name
                        .span;
                    references.push(Reference {
                        span,
                        kind: RefKind::Variable(span_text(source, param_span).to_owned()),
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
        /// Cfg state from the loader pass: declared flag names + active set.
        /// `None` on the manual-parse fallback (`apply_cfg` never ran).
        cfg: Option<&'a nativedsl::apply_cfg::CfgState>,
    },
    External,
}

/// Clone the loader's compact, `Send + Sync` string pool into a durable
/// resolver. Called once per analyze; the resulting `StringTable` is
/// `Arc`-shared across the root and every extracted external module.
fn build_string_table(strings: &StrPool) -> StringTable {
    StringTable::from_pool(strings)
}

/// Extract a `Module` from a resolved AST. `modules` contains all loaded
/// modules (root + inherits + imports), produced by the core's `Loader`;
/// cross-module info (`base_module`, `import_modules`) is extracted from
/// `modules` rather than re-parsing external files.
fn extract_module(
    shared: &Arc<ast::SharedAst>,
    modules: &[nativedsl::Module],
    ctx: &ast::ModuleContext,
    documents: &DocumentMap,
    strings: &Arc<StringTable>,
    kind: ExtractKind<'_>,
) -> Module {
    let grammar_span = ctx
        .root_items
        .iter()
        .find(|&&id| matches!(shared.arena.get(id), ast::Node::Grammar))
        .map(|&id| shared.arena.span(id));

    let document = documents.document(ctx.document);
    let module_source = document.text();
    let shared_ref: &ast::SharedAst = shared;
    let scopes = ScopeIndex::build(shared_ref, ctx);
    let definitions = extract_definitions(shared_ref, ctx, module_source, &scopes, strings);
    let import_names = collect_import_names(shared_ref, ctx, strings);
    let mut references = extract_references(
        shared_ref,
        ctx,
        module_source,
        &import_names,
        &scopes,
        strings,
    );

    // Find the inherited grammar module (if any) and extract its info.
    // `inherits()` yields all inherit refs in source order; the base is the first.
    let base_module = ctx
        .inherits(&shared_ref.arena)
        .next()
        .and_then(|inherit_id| {
            let ast::Node::Inherit {
                module: Some(idx), ..
            } = shared_ref.arena.get(inherit_id)
            else {
                return None;
            };
            extract_external_at(shared, modules, *idx, documents, strings).map(Box::new)
        });

    let import_modules = collect_import_modules(shared, modules, ctx, documents, strings);

    let (tokens_field, loader_succeeded, disabled_regions, declared_cfg_flags) = match kind {
        ExtractKind::Root {
            tokens,
            loader_succeeded,
            cfg,
            ..
        } => {
            extract_builtin_references(&tokens, grammar_span, &mut references);
            let declared = cfg.map(|c| cfg_flag_list(c, strings)).unwrap_or_default();
            // Disabled cfg sites are recoverable from the post-apply arena
            // even when `cfg` itself isn't available, but we gate on it to
            // ensure we're only doing this when the loader actually ran.
            let regions = if cfg.is_some() {
                scan_disabled_cfg_regions(shared_ref, ctx, strings)
            } else {
                Vec::new()
            };
            (Some(tokens), loader_succeeded, regions, declared)
        }
        ExtractKind::External { .. } => (None, true, Vec::new(), Vec::new()),
    };

    let (conflict_decls, conflicts_value_span) = extract_conflicts(shared_ref, ctx, module_source);

    Module {
        path: document.path().to_owned(),
        source: module_source.to_owned(),
        rope: Rope::from_str(module_source),
        tokens: tokens_field,
        grammar_span,
        definitions: Some(definitions),
        references: Some(references),
        base_module,
        import_modules,
        loader_succeeded,
        disabled_regions,
        declared_cfg_flags,
        conflict_decls,
        conflicts_value_span,
        shared: Arc::clone(shared),
        strings: Arc::clone(strings),
        root_items: ctx.root_items.clone(),
    }
}

/// Collect this module's `conflicts: [[a, b], ...]` entries with their source
/// spans (for anchoring the `UnnecessaryConflicts` codegen diagnostic and its
/// fix), plus the span of the `conflicts:` value itself (so the last-entry fix
/// can drop the whole field). Both are empty/`None` when there's no grammar
/// config, no `conflicts:` field, or the value isn't a literal list of lists
/// (e.g. built via `append(...)` - rare; the diagnostic then falls back to the
/// grammar block with no fix).
fn extract_conflicts(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    source: &str,
) -> (Vec<ConflictDecl>, Option<ast::Span>) {
    let Some(conflicts_id) = ctx.grammar_config.as_ref().and_then(|c| c.conflicts) else {
        return (Vec::new(), None);
    };
    let value_span = shared.arena.span(conflicts_id);
    let ast::Node::List(outer) = *shared.arena.get(conflicts_id) else {
        return (Vec::new(), None);
    };
    let mut decls = Vec::new();
    for &group_id in shared.pools.child_slice(outer) {
        let ast::Node::List(inner) = *shared.arena.get(group_id) else {
            continue;
        };
        let names = shared
            .pools
            .child_slice(inner)
            .iter()
            .map(|&n| span_text(source, shared.arena.span(n)).to_owned())
            .collect();
        decls.push(ConflictDecl {
            span: shared.arena.span(group_id),
            names,
        });
    }
    (decls, Some(value_span))
}

/// Extract an external (inherit/import) `Module` at index `idx` in `modules`.
fn extract_external_at(
    shared: &Arc<ast::SharedAst>,
    modules: &[nativedsl::Module],
    idx: nativedsl::ModuleId,
    documents: &DocumentMap,
    strings: &Arc<StringTable>,
) -> Option<Module> {
    let module = modules.get(usize::from(idx))?;
    Some(extract_module(
        shared,
        modules,
        module.ctx(),
        documents,
        strings,
        ExtractKind::External,
    ))
}

/// Collect `(binding_name, Module)` for each `let x = import(...)` at the top
/// level of `ctx`. Cycles are impossible here because the core `Loader`
/// rejects them before we get a successful module list.
fn collect_import_modules(
    shared: &Arc<ast::SharedAst>,
    modules: &[nativedsl::Module],
    ctx: &ast::ModuleContext,
    documents: &DocumentMap,
    strings: &Arc<StringTable>,
) -> Vec<(String, Module)> {
    let mut out = Vec::new();
    for &item_id in &ctx.root_items {
        let ast::Node::Let { name, value, .. } = shared.arena.get(item_id) else {
            continue;
        };
        let ast::Node::Import {
            module: Some(idx), ..
        } = shared.arena.get(*value)
        else {
            continue;
        };
        if let Some(info) = extract_external_at(shared, modules, *idx, documents, strings) {
            out.push((strings.get(*name).to_owned(), info));
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
fn cfg_flag_list(cfg: &nativedsl::apply_cfg::CfgState, strings: &StringTable) -> Vec<CfgFlag> {
    // `active` keys are exactly the declared flag names (mapped to their
    // enabled state), so it doubles as the declared-flag set.
    let mut flags: Vec<CfgFlag> = cfg
        .flags()
        .map(|(name, enabled)| CfgFlag {
            name: strings.get(name).to_owned(),
            enabled,
        })
        .collect();
    flags.sort_by(|a, b| a.name.cmp(&b.name));
    flags
}

/// Walk the post-apply-cfg AST for `Node::Cfg` nodes. After `apply_cfg`,
/// active cfg sites get overwritten in place with their child's data, while
/// disabled cfg sites are simply skipped (filtered from list ranges, etc.) -
/// the arena node itself is left intact at its original `NodeId` with its full
/// `#[cfg(NAME)] ITEM` source span. So every surviving `Node::Cfg` in the
/// arena is exactly one disabled cfg site, covering both top-level and inline
/// uses uniformly.
fn scan_disabled_cfg_regions(
    shared: &ast::SharedAst,
    ctx: &ast::ModuleContext,
    strings: &StringTable,
) -> Vec<DisabledRegion> {
    let mut out = Vec::new();
    for (node_id, node) in ctx.iter_own_nodes(&shared.arena) {
        if let ast::Node::Cfg {
            name, name_offset, ..
        } = node
        {
            let full_span = shared.arena.span(node_id);
            let name_text = strings.get(*name);
            out.push(DisabledRegion {
                name: name_text.to_owned(),
                name_span: ast::Span::new(
                    *name_offset,
                    name_offset.saturating_add(name_text.len() as u32),
                ),
                full_span,
            });
        }
    }
    out
}

fn build_fn_signature(
    source: &str,
    pools: &ast::AstPools,
    config: &ast::MacroConfig,
    fn_name: &str,
) -> String {
    let mut sig = format!("macro {fn_name}(");
    for (i, param) in pools.param_slice(config.params).iter().enumerate() {
        if i > 0 {
            sig.push_str(", ");
        }
        let _ = write!(sig, "{}: {}", span_text(source, param.name.span), param.ty);
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
/// at `file_path` would inherit or import.
#[must_use]
pub fn extract_deps(text: &str, file_path: &Path) -> Vec<PathBuf> {
    let (documents, document_id) = document_map_for_source(file_path, text);
    let document = documents.document(document_id);
    let Ok(tokens) = nativedsl::lexer::Lexer::new(document).tokenize() else {
        return Vec::new();
    };
    let mut shared = ast::SharedAst::new(text.len() / 30);
    let mut pool = RulePool::default();
    let Ok(ctx) =
        nativedsl::parser::Parser::new(&tokens, document, &mut shared, pool.strs_mut()).parse()
    else {
        return Vec::new();
    };
    let module_dir = file_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""));
    let mut seen = rustc_hash::FxHashSet::default();
    let mut deps = Vec::new();
    for &node_id in &ctx.module_refs {
        if let ast::Node::Import { path, .. } | ast::Node::Inherit { path, .. } =
            shared.arena.get(node_id)
        {
            let path_str = span_text(text, *path);
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

/// One end-to-end analysis result.
///
/// It contains both the LSP-side `Module` (used by every handler) and the
/// pipeline `Result` (used by `publish_dsl_diagnostics` for error reporting
/// and by `spawn_generate_check` for the parsed grammar).
/// Bundling them lets us run the loader once per `did_change` instead of
/// twice (one in diagnostics, one in handlers).
pub struct AnalyzeOutcome {
    pub module: Module,
    /// Core document store used to resolve the `DocumentId`s carried by
    /// pipeline errors and related notes.
    pub documents: Arc<DocumentMap>,
    /// `Some(Ok(grammar))` on full loader success.
    /// `Some(Err(error))` when the loader ran and produced a pipeline error.
    /// `None` when the loader didn't run (e.g. path couldn't be canonicalized).
    /// The Module is still populated from the manual-parse fallback, but
    /// there is no pipeline error to surface as a diagnostic.
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
    let (lex_documents, lex_document_id) = document_map_for_source(&grammar_path, &text);

    // Stage 1: Lex
    let tokens =
        match nativedsl::lexer::Lexer::new(lex_documents.document(lex_document_id)).tokenize() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("analyze: lex failed for {uri}");
                let rope = Rope::from_str(&text);
                return Some(AnalyzeOutcome {
                    module: Module::empty(grammar_path, text, rope),
                    pipeline: Some(Err(e.into())),
                    documents: Arc::new(lex_documents),
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
    Some(manual_parse_fallback(
        text,
        tokens,
        grammar_path,
        uri,
        None,
        None,
    ))
}

/// Attach the loader-wide `RulePool` to the root module's lowered grammar,
/// yielding the `InputGrammar` the rest of the LSP consumes.
///
/// Mirrors the tail of `nativedsl::parse_native_dsl`, which does this via the
/// private `LoweredGrammar::into_input`. We can't call `parse_native_dsl`
/// itself because it returns only the `InputGrammar` and drops the arena,
/// module list, type env, and cfg state the LSP needs.
fn lowered_into_input(
    lowered: nativedsl::LoweredGrammar,
    pool: RulePool,
) -> nativedsl::InputGrammar {
    nativedsl::InputGrammar {
        pool,
        name: lowered.name,
        variables: lowered.variables,
        external_roots: lowered.external_roots,
        extra_roots: lowered.extra_roots,
        reserved_sets: lowered.reserved_sets,
        supertype_names: lowered.supertype_names,
        conflict_names: lowered.conflict_names,
        inline_names: lowered.inline_names,
        word_name: lowered.word_name,
        precedence_orderings: lowered.precedence_orderings,
    }
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
    let mut pool = RulePool::default();
    let mut cfg = nativedsl::apply_cfg::CfgState::default();
    let mut documents = DocumentMap::default();
    let load_result = nativedsl::loader::Loader::new(
        &mut shared,
        &mut modules,
        &mut env,
        &mut state,
        &mut pool,
        &mut cfg,
        &mut documents,
    )
    .load_root(text, &canonical);

    if load_result.is_ok() {
        // Pop the root grammar so we can take its lowered InputGrammar by
        // value while keeping the rest of `modules` for cross-module
        // resolution (child indices are unchanged).
        let root = modules.pop().expect("root module pushed on success");
        let (ctx, lowered) = match root {
            nativedsl::Module::Grammar { ctx, lowered, .. } => (ctx, *lowered),
            nativedsl::Module::Library { .. } => {
                unreachable!("root module must be Grammar")
            }
        };
        // Lift the AST arena into an `Arc` now that the loader has stopped
        // mutating it; every extracted Module shares the same handle.
        let shared = Arc::new(shared);
        // Keep a durable copy of the compact string pool. The original moves
        // into `InputGrammar`; every extracted module shares this resolver.
        let strings = Arc::new(build_string_table(pool.strs()));
        let module = extract_module(
            &shared,
            &modules,
            &ctx,
            &documents,
            &strings,
            ExtractKind::Root {
                tokens: tokens.to_vec(),
                loader_succeeded: true,
                cfg: Some(&cfg),
            },
        );
        return Some(AnalyzeOutcome {
            module,
            pipeline: Some(Ok(lowered_into_input(lowered, pool))),
            documents: Arc::new(documents),
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
        Some(documents),
    ))
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
    pipeline_documents: Option<DocumentMap>,
) -> AnalyzeOutcome {
    let (documents, document_id) = document_map_for_source(&grammar_path, &text);
    let document = documents.document(document_id);
    let mut shared = ast::SharedAst::new(text.len() / 30);
    // The parser interns every declaration name into this pool, so it has to
    // outlive the parse and feed the `StringTable` below.
    let mut pool = RulePool::default();
    let module_ctx =
        match nativedsl::parser::Parser::new(&tokens, document, &mut shared, pool.strs_mut())
            .parse()
        {
            Ok(ctx) => ctx,
            Err(parse_err) => {
                tracing::warn!("analyze: parse failed for {uri}");
                let rope = Rope::from_str(&text);
                let mut module = Module::empty(grammar_path, text, rope);
                module.tokens = Some(tokens);
                return AnalyzeOutcome {
                    module,
                    // Prefer the original pipeline error if we have one;
                    // otherwise surface the parse failure.
                    pipeline: Some(Err(pipeline_err.unwrap_or_else(|| parse_err.into()))),
                    documents: Arc::new(pipeline_documents.unwrap_or(documents)),
                };
            }
        };

    let modules: Vec<nativedsl::Module> = Vec::new();
    let shared = Arc::new(shared);
    // Names are interned even on this path (`Node::Rule`/`Let`/`Forward` hold
    // a `StrId`), so the table has to be built from the parser's own pool -
    // an empty one would blank out every definition name.
    let strings = Arc::new(build_string_table(pool.strs()));
    let module = extract_module(
        &shared,
        &modules,
        &module_ctx,
        &documents,
        &strings,
        ExtractKind::Root {
            tokens,
            loader_succeeded: false,
            cfg: None,
        },
    );
    AnalyzeOutcome {
        module,
        // `None` reflects "the loader never ran" (canonicalize failed).
        // The manual-parse fallback still produced a partial Module.
        pipeline: pipeline_err.map(Err),
        documents: Arc::new(pipeline_documents.unwrap_or(documents)),
    }
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
            let (documents, id) = document_map_for_source(&path, &source);
            std::hint::black_box(
                nativedsl::lexer::Lexer::new(documents.document(id))
                    .tokenize()
                    .unwrap(),
            );
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

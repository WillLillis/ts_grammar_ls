//! Warn when a rule aliases a supertype symbol.
//!
//! `alias(X, Y)` makes a rule appear under a different name in the parse
//! tree. When `X` is a supertype, the aliased `Y` is a *distinct* symbol -
//! it doesn't carry the `.supertype` metadata bit, isn't in the runtime
//! supertype map, and won't transparently match the supertype's subtypes.
//! This is virtually always a footgun.
//!
//! See tree-sitter/tree-sitter#5270 for the underlying issue.
//!
//! # Two-pass detection
//!
//! Direct cases (`rule r { alias(_expr, foo) }`) live in the source AST as
//! a `Node::Alias` whose `content` references a supertype `Ident`. Pass 1
//! walks the AST and emits a finding with the alias call's precise span.
//!
//! Macro-mediated cases (`rule r { wrap(_expr) }` where `macro wrap(x) {
//! alias(x, foo) }`) only become detectable after expression-macro
//! substitution during lowering. Pass 2 walks the lowered `InputGrammar`
//! using the JSON-pipeline algorithm from the upstream issue's handoff;
//! since the lowered form has no source spans, these findings attach to
//! the enclosing rule's definition `full_span` (coarse but actionable).
//!
//! Pass-1 findings are also bucket-counted by `(rule, supertype, alias)`;
//! pass 2 decrements the bucket and only emits when its count exceeds
//! pass 1's, avoiding double-reporting on directly-visible instances.

use std::path::{Path, PathBuf};

use rustc_hash::{FxHashMap, FxHashSet};
use tree_sitter_generate::nativedsl::InputGrammar;
use tree_sitter_generate::nativedsl::ast::{IdentKind, Node, NodeId, SharedAst};
use tree_sitter_generate::nativedsl::{Rule, RuleId, RulePool, StrId};

use crate::document::{BindingLocation, Module};

use super::walker::push_children;
use super::{LintContext, LintFinding, LintId, LintMeta, Severity};

pub const META: LintMeta = LintMeta {
    name: "alias-over-supertype",
    severity: Severity::Warning,
    unnecessary: false,
    description: "\
Aliasing a supertype symbol produces a distinct node type that does not \
carry supertype identity. Queries on the aliased name won't transparently \
match the supertype's subtypes.",
};

/// Bucket key for dedup between pass 1 (AST) and pass 2 (InputGrammar):
/// `(enclosing_rule, supertype, alias)`. Pass 1 increments; pass 2
/// decrements and only emits when its instance count exceeds pass 1's.
type DedupKey = (String, String, String);

pub fn run(id: LintId, ctx: &LintContext, out: &mut Vec<LintFinding>) {
    let Some(grammar) = ctx.grammar else {
        return;
    };
    if grammar.supertype_names.is_empty() {
        return;
    }
    let supertypes: FxHashSet<&str> = grammar
        .supertype_names
        .iter()
        .map(|&n| grammar.pool.resolve(n))
        .collect();

    // Walk root + every transitively-reachable inherit / import. Lint
    // findings carry the path of the source file they belong to so the
    // LSP publisher can group by URI and the CLI can print the right
    // location.
    let mut counts: FxHashMap<DedupKey, u32> = FxHashMap::default();
    for module in ctx.module.reachable_modules() {
        pass_one_ast(module, &supertypes, id, out, &mut counts);
    }
    pass_two_grammar(ctx.module, grammar, &supertypes, id, out, &mut counts);
}

// ---------------------------------------------------------------------------
// Pass 1: AST walk for precise spans on direct cases
// ---------------------------------------------------------------------------

fn pass_one_ast(
    module: &Module,
    supertypes: &FxHashSet<&str>,
    id: LintId,
    out: &mut Vec<LintFinding>,
    counts: &mut FxHashMap<DedupKey, u32>,
) {
    let shared = module.shared.as_ref();
    let source = module.source.as_str();

    for &root_id in &module.root_items {
        // Only Rule/ExpandedRule roots contribute - aliases-of-literal-
        // supertypes inside macros (rare) are out of scope for pass 1; if
        // they ever inline, pass 2 catches them via InputGrammar.
        // Both name forms are interned now, so both go through the module's
        // `StringTable`.
        let (name_id, body) = match *shared.arena.get(root_id) {
            Node::Rule { name, body, .. } => (name, body),
            Node::ExpandedRule(eid) => {
                let expansion = shared.pools.get_expansion(eid);
                (expansion.name, expansion.body)
            }
            _ => continue,
        };
        let rule_name = module.strings.get(name_id).to_owned();

        walk_for_aliases(
            shared,
            source,
            &module.path,
            supertypes,
            body,
            &rule_name,
            id,
            out,
            counts,
        );
    }
}

#[expect(clippy::too_many_arguments)]
fn walk_for_aliases(
    shared: &SharedAst,
    source: &str,
    path: &Path,
    supertypes: &FxHashSet<&str>,
    root: NodeId,
    rule_name: &str,
    id: LintId,
    out: &mut Vec<LintFinding>,
    counts: &mut FxHashMap<DedupKey, u32>,
) {
    let mut stack = vec![root];
    while let Some(nid) = stack.pop() {
        let node = shared.arena.get(nid);
        if let Node::Alias { content, target } = *node {
            if let Some(super_name) = first_supertype_in(shared, source, supertypes, content) {
                let alias_name = ident_text(shared, source, target).unwrap_or("<unknown>");
                let span = shared.arena.span(nid);
                let related = ident_span(shared, content)
                    .into_iter()
                    .map(|s| (s, format!("supertype `{super_name}` referenced here")))
                    .collect();
                out.push(LintFinding {
                    lint: id,
                    path: path.to_path_buf(),
                    span,
                    message: format!(
                        "alias of supertype `{super_name}` as `{alias_name}`: the aliased \
                         `{alias_name}` is a distinct symbol and will not behave as a supertype \
                         of `{super_name}`'s subtypes",
                    ),
                    related,
                    fix: None,
                });
                *counts
                    .entry((rule_name.to_owned(), super_name.to_owned(), alias_name.to_owned()))
                    .or_default() += 1;
            }
        }
        push_children(node, shared, &mut stack);
    }
}

fn first_supertype_in<'s>(
    shared: &SharedAst,
    source: &'s str,
    supertypes: &FxHashSet<&str>,
    node_id: NodeId,
) -> Option<&'s str> {
    let mut stack = vec![node_id];
    while let Some(nid) = stack.pop() {
        let node = shared.arena.get(nid);
        if let Node::Ident(IdentKind::Rule(_)) = node {
            let span = shared.arena.span(nid);
            let name = &source[span.start as usize..span.end as usize];
            if supertypes.contains(name) {
                return Some(name);
            }
        }
        match *node {
            Node::SeqOrChoice { range, .. } => {
                for i in range.as_range() {
                    stack.push(shared.pools.children[i]);
                }
            }
            Node::Repeat { inner, .. } => stack.push(inner),
            Node::Prec { content, .. } => stack.push(content),
            Node::Field { content, .. } => stack.push(content),
            Node::Reserved { content, .. } => stack.push(content),
            Node::Token { inner, .. } => stack.push(inner),
            // A nested alias replaces the alias context entirely - its own
            // check is the outer walker's job. Stop here.
            Node::Alias { .. } => {}
            _ => {}
        }
    }
    None
}

fn ident_text<'s>(shared: &SharedAst, source: &'s str, node_id: NodeId) -> Option<&'s str> {
    match shared.arena.get(node_id) {
        Node::Ident(_) => {
            let span = shared.arena.span(node_id);
            Some(&source[span.start as usize..span.end as usize])
        }
        _ => None,
    }
}

fn ident_span(
    shared: &SharedAst,
    node_id: NodeId,
) -> Option<tree_sitter_generate::nativedsl::ast::Span> {
    let mut stack = vec![node_id];
    while let Some(nid) = stack.pop() {
        let node = shared.arena.get(nid);
        if matches!(node, Node::Ident(IdentKind::Rule(_))) {
            return Some(shared.arena.span(nid));
        }
        match *node {
            Node::SeqOrChoice { range, .. } => {
                for i in range.as_range() {
                    stack.push(shared.pools.children[i]);
                }
            }
            Node::Repeat { inner, .. } => stack.push(inner),
            Node::Prec { content, .. } => stack.push(content),
            Node::Field { content, .. } => stack.push(content),
            Node::Reserved { content, .. } => stack.push(content),
            Node::Token { inner, .. } => stack.push(inner),
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Pass 2: lowered InputGrammar walk for macro-mediated cases (coarse span)
// ---------------------------------------------------------------------------

fn pass_two_grammar(
    root: &Module,
    grammar: &InputGrammar,
    supertypes: &FxHashSet<&str>,
    id: LintId,
    out: &mut Vec<LintFinding>,
    counts: &mut FxHashMap<DedupKey, u32>,
) {
    for var in &grammar.variables {
        // Resolve the rule's defining module via the import / inherit
        // chain so helper-defined rules get attributed to the helper's
        // path, not the root's.
        let var_name = grammar.pool.resolve(var.name);
        let Some((path, def_span)) = resolve_rule_location(root, var_name) else {
            continue;
        };

        walk_input_rule(
            &grammar.pool,
            var.root,
            None,
            supertypes,
            &mut |super_name, alias_name| {
                let key = (
                    var_name.to_owned(),
                    super_name.to_owned(),
                    alias_name.to_owned(),
                );
                if let Some(c) = counts.get_mut(&key) {
                    if *c > 0 {
                        *c -= 1;
                        return; // pass 1 already covered this instance
                    }
                }
                out.push(LintFinding {
                    lint: id,
                    path: path.clone(),
                    span: def_span,
                    message: format!(
                        "alias of supertype `{super_name}` as `{alias_name}`: the aliased \
                         `{alias_name}` is a distinct symbol and will not behave as a \
                         supertype of `{super_name}`'s subtypes (introduced by macro \
                         expansion - exact location not available)",
                    ),
                    related: Vec::new(),
                    fix: None,
                });
            },
        );
    }
}

/// Find the source file + `full_span` for a rule named `name`, walking
/// `root`'s inherits / imports. Returns `None` only when no module in
/// the chain has a matching `Rule` / `OverrideRule` definition (e.g.
/// the rule was synthesized by macro expansion in a way we don't
/// recover).
fn resolve_rule_location(
    root: &Module,
    name: &str,
) -> Option<(PathBuf, tree_sitter_generate::nativedsl::ast::Span)> {
    let location = root.resolve_bare_name(name, None)?;
    let (module, def) = match location {
        BindingLocation::Local(def) => (root, def),
        BindingLocation::External { module, def } => (module, def),
    };
    if !matches!(
        def.kind,
        crate::document::DefKind::Rule | crate::document::DefKind::OverrideRule
    ) {
        return None;
    }
    Some((module.path.clone(), def.full_span))
}

/// Algorithm from the upstream JSON-pipeline handoff: walk a lowered
/// rule carrying `outer_alias` context. `Metadata`'s alias slot
/// overrides; otherwise the outer alias is inherited so that
/// `alias(choice(_super, _other), x)` still flags `_super`.
///
/// `outer_alias` is a `StrId` rather than a `&str` so the walk doesn't have
/// to keep a `&RulePool` borrow alive across the `emit` callback.
fn walk_input_rule(
    pool: &RulePool,
    root: RuleId,
    outer_alias: Option<StrId>,
    supertypes: &FxHashSet<&str>,
    emit: &mut dyn FnMut(&str, &str),
) {
    match pool.node(root) {
        Rule::Metadata { params, rule } => {
            let next_alias = pool.params(params).alias.map(|a| a.value).or(outer_alias);
            walk_input_rule(pool, rule, next_alias, supertypes, emit);
        }
        Rule::Reserved { rule, .. } => {
            walk_input_rule(pool, rule, outer_alias, supertypes, emit);
        }
        Rule::NamedSymbol(name) => {
            if let Some(alias) = outer_alias
                && supertypes.contains(pool.resolve(name))
            {
                emit(pool.resolve(name), pool.resolve(alias));
            }
        }
        Rule::Choice(elements) | Rule::Seq(elements) => {
            for &element in pool.child_slice(elements) {
                walk_input_rule(pool, element, outer_alias, supertypes, emit);
            }
        }
        Rule::Repeat(inner) => walk_input_rule(pool, inner, outer_alias, supertypes, emit),
        Rule::Blank
        | Rule::String(_)
        | Rule::Pattern(_, _)
        | Rule::Sym { .. }
        | Rule::Eof => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::analysis::analyze;
    use crate::lints::{LintContext, LintSet, run_all};
    use tower_lsp::lsp_types::Url;

    /// One finding's user-visible shape. Source-derived `byte_offset`
    /// pins the precise-vs-rule-level distinction without coupling the
    /// test to exact byte positions that shift when the fixture text
    /// changes - we resolve `marker` from the grammar source.
    #[derive(Debug, PartialEq, Eq)]
    struct Snap {
        /// Substring the finding's `span.start` points at. For pass-1
        /// findings this is the leading `alias` keyword; for pass-2 it's
        /// the leading `rule` keyword of the enclosing rule decl.
        anchor: String,
        message: String,
    }

    /// One finding with multi-file attribution. Extends `Snap` with the
    /// owning file's basename so cross-file tests can assert that a
    /// finding lands in the right source file (e.g. base.tsg vs
    /// grammar.tsg under `inherit`).
    #[derive(Debug, PartialEq, Eq)]
    struct AttributedSnap {
        file: String,
        anchor: String,
        message: String,
    }

    /// Run the lint on `grammar_src` and snapshot findings in source
    /// order. `extra_files` are sibling files written alongside the
    /// grammar (e.g. helpers for import tests); keys are filenames.
    fn run_lint(grammar_src: &str, extra_files: &[(&str, &str)]) -> Vec<Snap> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grammar.tsg");
        std::fs::write(&path, grammar_src).unwrap();
        for (name, contents) in extra_files {
            std::fs::write(dir.path().join(name), contents).unwrap();
        }
        let uri = Url::from_file_path(&path).unwrap();

        let outcome = analyze(grammar_src.to_owned(), &uri).expect("analyze");
        // `InputGrammar` isn't `Debug`, so report the error side only.
        let grammar = match outcome.pipeline {
            Some(Ok(g)) => g,
            Some(Err(e)) => panic!("pipeline failed: {e:?}"),
            None => panic!("pipeline did not run"),
        };

        // Lints walk reachable modules themselves and tag each finding
        // with its source file; just look up the right source for each.
        let modules = outcome.module.reachable_modules();
        let ctx = LintContext {
            module: &outcome.module,
            grammar: Some(&grammar),
        };
        let findings = run_all(&ctx, &LintSet::default());
        findings
            .into_iter()
            .map(|f| {
                let source = modules
                    .iter()
                    .find(|m| m.path == f.path)
                    .map(|m| m.source.as_str())
                    .unwrap_or("");
                Snap {
                    anchor: leading_word(source, f.span.start),
                    message: f.message,
                }
            })
            .collect()
    }

    /// Return the leading identifier/keyword starting at `offset` in
    /// `source`. Used by snapshots to anchor a finding without baking
    /// in byte offsets.
    fn leading_word(source: &str, offset: u32) -> String {
        let start = offset as usize;
        let bytes = source.as_bytes();
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        source[start..end].to_owned()
    }

    const PREAMBLE: &str = r#"
        rule program { repeat(statement) }
        rule statement { choice(_expression, ";") }
        rule _expression { choice(identifier, number) }
        rule identifier { regexp("[a-z]+") }
        rule number { regexp("[0-9]+") }
        rule other { "x" }
    "#;

    fn with_super(rules: &str) -> String {
        format!(
            r#"grammar {{
                language: "t",
                supertypes: [_expression],
            }}
            {PREAMBLE}
            {rules}"#
        )
    }

    fn warn_msg(supertype: &str, alias: &str) -> String {
        format!(
            "alias of supertype `{supertype}` as `{alias}`: the aliased \
             `{alias}` is a distinct symbol and will not behave as a supertype of \
             `{supertype}`'s subtypes"
        )
    }

    fn warn_msg_macro(supertype: &str, alias: &str) -> String {
        format!(
            "{} (introduced by macro expansion - exact location not available)",
            warn_msg(supertype, alias)
        )
    }

    // -------- Handoff matrix --------

    #[test]
    fn case_1_direct_alias_of_supertype_warns() {
        let snaps = run_lint(
            &with_super("rule problem { alias(_expression, renamed) }"),
            &[],
        );
        assert_eq!(
            snaps,
            vec![Snap {
                anchor: "alias".to_owned(),
                message: warn_msg("_expression", "renamed"),
            }]
        );
    }

    #[test]
    fn case_2_no_alias_silent() {
        let snaps = run_lint(&with_super("rule r { seq(\"s\", _expression) }"), &[]);
        assert_eq!(snaps, Vec::<Snap>::new());
    }

    #[test]
    fn case_3_alias_of_non_supertype_silent() {
        let snaps = run_lint(&with_super("rule r { alias(identifier, renamed) }"), &[]);
        assert_eq!(snaps, Vec::<Snap>::new());
    }

    #[test]
    fn case_5_alias_under_seq_prec_warns() {
        let snaps = run_lint(
            &with_super(
                r#"rule problem { seq("x", prec(2, alias(_expression, renamed)), "y") }"#,
            ),
            &[],
        );
        assert_eq!(
            snaps,
            vec![Snap {
                anchor: "alias".to_owned(),
                message: warn_msg("_expression", "renamed"),
            }]
        );
    }

    #[test]
    fn case_6_alias_wraps_compound_with_supertype_warns() {
        let snaps = run_lint(
            &with_super("rule problem { alias(choice(_expression, other), x) }"),
            &[],
        );
        assert_eq!(
            snaps,
            vec![Snap {
                anchor: "alias".to_owned(),
                message: warn_msg("_expression", "x"),
            }]
        );
    }

    #[test]
    fn case_7_two_supertypes_aliased_in_two_rules_two_warns() {
        let src = format!(
            r#"grammar {{
                language: "t",
                supertypes: [_expression, _statement],
            }}
            {PREAMBLE}
            rule _statement {{ choice("a", "b") }}
            rule problem_a {{ alias(_expression, ra) }}
            rule problem_b {{ alias(_statement, rb) }}"#
        );
        let snaps = run_lint(&src, &[]);
        assert_eq!(
            snaps,
            vec![
                Snap {
                    anchor: "alias".to_owned(),
                    message: warn_msg("_expression", "ra"),
                },
                Snap {
                    anchor: "alias".to_owned(),
                    message: warn_msg("_statement", "rb"),
                },
            ]
        );
    }

    #[test]
    fn case_8_alias_wraps_repeat_of_supertype_warns() {
        let snaps = run_lint(
            &with_super("rule problem { alias(repeat(_expression), x) }"),
            &[],
        );
        assert_eq!(
            snaps,
            vec![Snap {
                anchor: "alias".to_owned(),
                message: warn_msg("_expression", "x"),
            }]
        );
    }

    #[test]
    fn case_9_hidden_supertype_warns() {
        // _expression already starts with `_` in PREAMBLE; case 9 just
        // confirms supertype-ness is independent of the leading
        // underscore. Covered by case 1 - keep an explicit test for the
        // matrix.
        let snaps = run_lint(
            &with_super("rule problem { alias(_expression, renamed) }"),
            &[],
        );
        assert_eq!(snaps.len(), 1);
    }

    // -------- Negative edges --------

    #[test]
    fn alias_target_not_in_supertypes_silent_even_with_supertype_in_grammar() {
        // Same alias name "renamed" appears elsewhere on a non-supertype;
        // that occurrence should not warn.
        let snaps = run_lint(
            &with_super(
                r#"rule fine { alias(identifier, renamed) }
                   rule other_decl { alias(number, renamed) }"#,
            ),
            &[],
        );
        assert_eq!(snaps, Vec::<Snap>::new());
    }

    #[test]
    fn no_supertypes_declared_short_circuits() {
        let src = r#"grammar { language: "t" }
            rule program { alias(identifier, renamed) }
            rule identifier { regexp("[a-z]+") }"#;
        let snaps = run_lint(src, &[]);
        assert_eq!(snaps, Vec::<Snap>::new());
    }

    // -------- Macro-mediated case (pass 2) --------

    #[test]
    fn macro_wrapped_alias_warns_with_rule_anchor() {
        let helpers = "macro wrap(expr: rule_t) rule_t { alias(expr, wrapped) }\n";
        let grammar = format!(
            r#"let h = import("helpers.tsg")
            grammar {{
                language: "t",
                supertypes: [_expression],
            }}
            {PREAMBLE}
            rule wrapper {{ h::wrap(_expression) }}"#
        );
        let snaps = run_lint(&grammar, &[("helpers.tsg", helpers)]);
        // Pass 2 only - pass 1 doesn't see through MacroParam. Anchor is
        // the rule keyword (full_span starts there) since precise location
        // isn't recoverable from the lowered grammar.
        assert_eq!(
            snaps,
            vec![Snap {
                anchor: "rule".to_owned(),
                message: warn_msg_macro("_expression", "wrapped"),
            }]
        );
    }

    /// Like `run_lint` but returns findings tagged with the owning
    /// file's basename for cross-file attribution tests.
    fn run_lint_attributed(
        grammar_src: &str,
        extra_files: &[(&str, &str)],
    ) -> Vec<AttributedSnap> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grammar.tsg");
        std::fs::write(&path, grammar_src).unwrap();
        for (name, contents) in extra_files {
            std::fs::write(dir.path().join(name), contents).unwrap();
        }
        let uri = Url::from_file_path(&path).unwrap();

        let outcome = analyze(grammar_src.to_owned(), &uri).expect("analyze");
        let grammar = outcome
            .pipeline
            .as_ref()
            .and_then(|p| p.as_ref().ok())
            .expect("pipeline must produce a grammar");

        let modules = outcome.module.reachable_modules();
        let ctx = LintContext {
            module: &outcome.module,
            grammar: Some(&grammar),
        };
        run_all(&ctx, &LintSet::default())
            .into_iter()
            .map(|f| {
                let module = modules.iter().find(|m| m.path == f.path);
                AttributedSnap {
                    file: f
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    anchor: leading_word(
                        module.map(|m| m.source.as_str()).unwrap_or(""),
                        f.span.start,
                    ),
                    message: f.message,
                }
            })
            .collect()
    }

    #[test]
    fn inherited_base_alias_attributes_to_base_file() {
        let base = r#"grammar {
                language: "base",
                supertypes: [_expression],
            }
            rule program { repeat(statement) }
            rule statement { choice(_expression, ";") }
            rule _expression { choice(identifier, number) }
            rule identifier { regexp("[a-z]+") }
            rule number { regexp("[0-9]+") }
            rule base_problem { alias(_expression, base_renamed) }"#;
        let derived = r#"let base = inherit("base.tsg")
            grammar {
                language: "derived",
                inherits: base,
            }"#;
        let snaps = run_lint_attributed(derived, &[("base.tsg", base)]);
        assert_eq!(
            snaps,
            vec![AttributedSnap {
                file: "base.tsg".to_owned(),
                anchor: "alias".to_owned(),
                message: warn_msg("_expression", "base_renamed"),
            }]
        );
    }

    #[test]
    fn direct_and_macro_in_same_grammar_no_double_report() {
        let helpers = "macro wrap(expr: rule_t) rule_t { alias(expr, w) }\n";
        let grammar = format!(
            r#"let h = import("helpers.tsg")
            grammar {{
                language: "t",
                supertypes: [_expression],
            }}
            {PREAMBLE}
            rule direct {{ alias(_expression, renamed_direct) }}
            rule via_macro {{ h::wrap(_expression) }}"#
        );
        let snaps = run_lint(&grammar, &[("helpers.tsg", helpers)]);
        // Exactly two: one precise (direct), one rule-level (macro).
        // Pass-1 finding for `direct` must dedup against pass 2 - no
        // third entry.
        assert_eq!(
            snaps,
            vec![
                Snap {
                    anchor: "alias".to_owned(),
                    message: warn_msg("_expression", "renamed_direct"),
                },
                Snap {
                    anchor: "rule".to_owned(),
                    message: warn_msg_macro("_expression", "w"),
                },
            ]
        );
    }
}

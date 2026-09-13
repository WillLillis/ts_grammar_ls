//! Warn when a rule listed in `extras: [...]` can match the empty string.
//!
//! Tree-sitter inserts extras between every token; a rule that matches
//! empty is therefore valid at every position - including between the
//! same two positions in a chain - which lets the parser loop forever.
//! Core's check is shallow: it strips one `Metadata` layer and looks at
//! the immediate `String("")` or `Pattern(p, _)` whose regex matches
//! empty. We deepen: compute the full *nullable* set of rules with a
//! fixpoint, then warn for any extra whose rule is nullable.
//!
//! Nullable rule (matches empty):
//! - `String("")`, `Blank`, `Repeat(_)` are unconditionally nullable
//! - `Pattern(p, _)` is nullable iff `Regex::new(p).is_match("")`
//! - `Choice` is nullable iff *any* branch is
//! - `Seq` is nullable iff *all* members are
//! - `Metadata { rule, .. }` / `Reserved { rule, .. }` defer to inner
//! - `NamedSymbol(name)` defers to the variable definition's nullable
//!
//! Fixpoint: initialize every name as non-nullable, then iterate until
//! no change. Worst case is O(rules^2) but rules counts are tiny.

use rustc_hash::FxHashMap;
use tree_sitter_generate::nativedsl::InputGrammar;
use tree_sitter_generate::nativedsl::{Rule, RuleId, RulePool, StrId};

use crate::document::{BindingLocation, Module};

use super::{LintContext, LintFinding, LintId, LintMeta, Severity};

pub const META: LintMeta = LintMeta {
    name: "empty-matching-extra",
    severity: Severity::Warning,
    unnecessary: false,
    description: "\
A rule listed in `extras` matches the empty string. Tree-sitter inserts \
extras between every token, so an empty-matching extra produces an \
infinite parse loop. Inline the rule's content where it's actually \
needed instead of routing it through `extras`.",
};

pub fn run(id: LintId, ctx: &LintContext, out: &mut Vec<LintFinding>) {
    let Some(grammar) = ctx.grammar else {
        return;
    };
    if grammar.extra_roots.is_empty() {
        return;
    }

    let nullable = compute_nullable(grammar);

    for &extra in &grammar.extra_roots {
        // Extras are most commonly `NamedSymbol("comment")`-style refs;
        // inline `String("")` would also be empty but is already a
        // grammar mistake (an empty literal extra) and not what this
        // lint targets - we focus on referenced rules.
        let Rule::NamedSymbol(name_id) = grammar.pool.node(extra) else {
            continue;
        };
        if !nullable.get(&name_id).copied().unwrap_or(false) {
            continue;
        }
        let name = grammar.pool.resolve(name_id);
        let Some((path, span)) = resolve_rule_location(ctx.module, name) else {
            continue;
        };
        out.push(LintFinding {
            lint: id,
            path,
            span,
            message: format!(
                "rule `{name}` is listed in `extras` but can match the empty string; \
                 tree-sitter would loop forever inserting it between tokens. Inline \
                 the rule's content where it's needed instead.",
            ),
            related: Vec::new(),
            fix: None,
        });
    }
}

/// Fixpoint over `grammar.variables`. Result maps every variable name
/// to its nullable status. Unknown names (referenced but not defined)
/// stay absent and are treated as non-nullable - safer to under-warn
/// than over-warn given the lint signals an infinite-loop bug.
fn compute_nullable(grammar: &InputGrammar) -> FxHashMap<StrId, bool> {
    let mut nullable: FxHashMap<StrId, bool> =
        grammar.variables.iter().map(|v| (v.name, false)).collect();

    loop {
        let mut changed = false;
        for var in &grammar.variables {
            if nullable.get(&var.name).copied().unwrap_or(false) {
                // Already known nullable; can only ever stay nullable.
                continue;
            }
            if rule_nullable(&grammar.pool, var.root, &nullable) {
                nullable.insert(var.name, true);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    nullable
}

/// Recursively decide whether the rule at `id` matches the empty string given
/// the current `nullable` map. For `NamedSymbol`, we consult the map; for
/// `Sym` (terminal index), we conservatively return false because terminals
/// here represent extracted tokens with no easy way to re-derive their pattern
/// from a lowered `InputGrammar`. `Eof` matches only at end of input, never
/// the empty string mid-parse.
fn rule_nullable(pool: &RulePool, id: RuleId, nullable: &FxHashMap<StrId, bool>) -> bool {
    match pool.node(id) {
        Rule::Blank => true,
        Rule::String(s) => pool.resolve(s).is_empty(),
        Rule::Pattern(pattern, _flags) => {
            // Mirror core's check: try to compile, ask `is_match("")`.
            // Compilation failures fall through as non-nullable (the
            // user has bigger problems than this lint will surface).
            regex::Regex::new(pool.resolve(pattern)).is_ok_and(|r| r.is_match(""))
        }
        Rule::Repeat(_) => true,
        Rule::Choice(elements) => pool
            .child_slice(elements)
            .iter()
            .any(|&e| rule_nullable(pool, e, nullable)),
        Rule::Seq(elements) => pool
            .child_slice(elements)
            .iter()
            .all(|&e| rule_nullable(pool, e, nullable)),
        Rule::Metadata { rule, .. } | Rule::Reserved { rule, .. } => {
            rule_nullable(pool, rule, nullable)
        }
        Rule::NamedSymbol(name) => nullable.get(&name).copied().unwrap_or(false),
        Rule::Sym { .. } | Rule::Eof => false,
    }
}

/// Find the source file + `full_span` for a rule named `name`. Walks
/// `root`'s inherits / imports via `resolve_bare_name`. Same shape as
/// the helper in `alias_over_supertype`; intentionally duplicated for
/// now since hoisting would create a tiny shared module for one
/// function. If a third lint needs it, hoist.
fn resolve_rule_location(
    root: &Module,
    name: &str,
) -> Option<(std::path::PathBuf, tree_sitter_generate::nativedsl::ast::Span)> {
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

#[cfg(test)]
mod tests {
    use crate::analysis::analyze;
    use crate::lints::{LintContext, LintSet, run_all};
    use tower_lsp::lsp_types::Url;

    #[derive(Debug, PartialEq, Eq)]
    struct Snap {
        anchor: String,
        message: String,
    }

    fn run_lint(grammar_src: &str) -> Vec<Snap> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grammar.tsg");
        std::fs::write(&path, grammar_src).unwrap();
        let uri = Url::from_file_path(&path).unwrap();
        let outcome = analyze(grammar_src.to_owned(), &uri).expect("analyze");
        let grammar = outcome
            .pipeline
            .as_ref()
            .and_then(|p| p.as_ref().ok())
            .expect("pipeline must succeed");
        let ctx = LintContext {
            module: &outcome.module,
            grammar: Some(&grammar),
        };
        run_all(&ctx, &LintSet::default())
            .into_iter()
            .filter(|f| f.lint.name() == super::META.name)
            .map(|f| {
                let src = &outcome.module.source;
                let start = f.span.start as usize;
                let bytes = src.as_bytes();
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                Snap {
                    anchor: src[start..end].to_owned(),
                    message: f.message,
                }
            })
            .collect()
    }

    #[test]
    fn empty_literal_extra_warns() {
        let src = r#"grammar {
                language: "t",
                extras: [whitespace],
            }
            rule program { "x" }
            rule whitespace { "" }"#;
        let snaps = run_lint(src);
        assert_eq!(snaps.len(), 1, "{snaps:?}");
        assert_eq!(snaps[0].anchor, "rule");
    }

    #[test]
    fn nonempty_extra_silent() {
        let src = r#"grammar {
                language: "t",
                extras: [whitespace],
            }
            rule program { "x" }
            rule whitespace { regexp(r"\s+") }"#;
        let snaps = run_lint(src);
        assert_eq!(snaps, Vec::<Snap>::new());
    }

    #[test]
    fn deep_walk_through_choice_warns() {
        // Core's shallow check would miss this: outer body is a Choice,
        // not a String/Pattern. One branch matches empty → nullable.
        let src = r#"grammar {
                language: "t",
                extras: [maybe_ws],
            }
            rule program { "x" }
            rule maybe_ws { choice("", regexp(r"\s+")) }"#;
        let snaps = run_lint(src);
        assert_eq!(snaps.len(), 1, "{snaps:?}");
    }

    #[test]
    fn deep_walk_through_seq_silent_when_one_member_nonempty() {
        // Seq is nullable only if all members are. `regexp(r"\s")`
        // matches at least one char, so the seq is non-nullable even
        // though it's wrapped around an empty string.
        let src = r#"grammar {
                language: "t",
                extras: [ws_then_required],
            }
            rule program { "x" }
            rule ws_then_required { seq("", regexp(r"\s+")) }"#;
        let snaps = run_lint(src);
        assert_eq!(snaps, Vec::<Snap>::new());
    }

    #[test]
    fn deep_walk_through_repeat_warns() {
        // `repeat(X)` is always nullable (0 reps = empty).
        let src = r#"grammar {
                language: "t",
                extras: [zero_or_more_ws],
            }
            rule program { "x" }
            rule zero_or_more_ws { repeat(regexp(r"\s")) }"#;
        let snaps = run_lint(src);
        assert_eq!(snaps.len(), 1);
    }

    #[test]
    fn cross_rule_nullability_warns() {
        // Extras references _outer, which references _inner, which is
        // empty. Core's shallow check would miss this entirely; we
        // chase the chain via the fixpoint.
        let src = r#"grammar {
                language: "t",
                extras: [_outer],
            }
            rule program { "x" }
            rule _outer { _inner }
            rule _inner { "" }"#;
        let snaps = run_lint(src);
        assert_eq!(snaps.len(), 1, "{snaps:?}");
    }

    #[test]
    fn empty_matching_regex_warns() {
        // Regex `a?` matches empty.
        let src = r#"grammar {
                language: "t",
                extras: [maybe_a],
            }
            rule program { "x" }
            rule maybe_a { regexp("a?") }"#;
        let snaps = run_lint(src);
        assert_eq!(snaps.len(), 1, "{snaps:?}");
    }

    #[test]
    fn no_extras_short_circuits() {
        let src = r#"grammar { language: "t" }
            rule program { "x" }
            rule unused { "" }"#;
        let snaps = run_lint(src);
        assert_eq!(snaps, Vec::<Snap>::new());
    }
}

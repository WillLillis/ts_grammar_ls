//! Warn on `seq(X)` / `choice(X)` calls with a single element.
//!
//! These are always semantically equivalent to just `X` - the seq/choice
//! wrapper adds nothing. Tree-sitter's core flags this only when the
//! single child is a string or pattern literal; we broaden to any child
//! kind because the wrapping is universally redundant (no surrounding
//! construct - field, alias, prec, ... - changes behavior based on the
//! seq/choice being present rather than directly the inner node).
//!
//! Empirically, looking at every other `Node::SeqOrChoice` shape we
//! considered (extras attachment, prec wrappers, fields), the single-
//! element form has no semantic effect distinct from the inner node.
//! If we find a counterexample we'll narrow the lint.

use std::path::Path;

use tree_sitter_generate::nativedsl::ast::{Node, NodeId, SharedAst};

use crate::document::Module;

use super::walker::push_children;
use super::{LintContext, LintFinding, LintId, LintMeta, Severity};

pub const META: LintMeta = LintMeta {
    name: "single-element-collection",
    severity: Severity::Warning,
    unnecessary: false,
    description: "\
A `seq(...)` or `choice(...)` call with a single element is equivalent \
to that element alone. The wrapper adds no semantics.",
};

pub fn run(id: LintId, ctx: &LintContext, out: &mut Vec<LintFinding>) {
    for module in ctx.module.reachable_modules() {
        scan_module(module, id, out);
    }
}

fn scan_module(module: &Module, id: LintId, out: &mut Vec<LintFinding>) {
    let shared = module.shared.as_ref();
    // We walk all nodes owned by this module via root_items; aliases of
    // the same idea (rule bodies, macro bodies, let values) all
    // transitively reach via push_children.
    for &root_id in &module.root_items {
        walk(shared, &module.path, root_id, id, out);
    }
}

fn walk(
    shared: &SharedAst,
    path: &Path,
    root: NodeId,
    id: LintId,
    out: &mut Vec<LintFinding>,
) {
    let mut stack = vec![root];
    while let Some(nid) = stack.pop() {
        let node = shared.arena.get(nid);
        if let Node::SeqOrChoice { seq, range } = *node {
            if range.len == 1 {
                let kind = if seq { "seq" } else { "choice" };
                let span = shared.arena.span(nid);
                out.push(LintFinding {
                    lint: id,
                    path: path.to_path_buf(),
                    span,
                    message: format!(
                        "`{kind}(...)` with a single element is equivalent to that element \
                         alone; the wrapper has no effect"
                    ),
                    related: Vec::new(),
                    // Fix would replace the seq/choice call with the
                    // inner node's source. Deferred until the code-action
                    // / --fix infrastructure is wired up.
                    fix: None,
                });
            }
        }
        push_children(node, shared, &mut stack);
    }
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
                let bytes = outcome.module.source.as_bytes();
                let start = f.span.start as usize;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                Snap {
                    anchor: outcome.module.source[start..end].to_owned(),
                    message: f.message,
                }
            })
            .collect()
    }

    fn grammar(rules: &str) -> String {
        format!(
            r#"grammar {{ language: "t" }}
            rule program {{ repeat(statement) }}
            rule statement {{ "x" }}
            {rules}"#
        )
    }

    #[test]
    fn single_seq_string_warns() {
        let snaps = run_lint(&grammar(r#"rule r { seq("a") }"#));
        assert_eq!(
            snaps,
            vec![Snap {
                anchor: "seq".to_owned(),
                message: "`seq(...)` with a single element is equivalent to that element \
                          alone; the wrapper has no effect"
                    .to_owned(),
            }]
        );
    }

    #[test]
    fn single_choice_named_symbol_also_warns() {
        // Broader-than-core: we also fire for NamedSymbol children.
        let snaps = run_lint(&grammar(r#"rule r { choice(statement) }"#));
        assert_eq!(
            snaps,
            vec![Snap {
                anchor: "choice".to_owned(),
                message: "`choice(...)` with a single element is equivalent to that element \
                          alone; the wrapper has no effect"
                    .to_owned(),
            }]
        );
    }

    #[test]
    fn multi_element_seq_silent() {
        let snaps = run_lint(&grammar(r#"rule r { seq("a", "b") }"#));
        assert_eq!(snaps, Vec::<Snap>::new());
    }

    #[test]
    fn multi_element_choice_silent() {
        let snaps = run_lint(&grammar(r#"rule r { choice("a", "b") }"#));
        assert_eq!(snaps, Vec::<Snap>::new());
    }

    #[test]
    fn nested_single_element_seq_warns_for_each() {
        // seq(seq("a")) - inner and outer both single-element.
        let snaps = run_lint(&grammar(r#"rule r { seq(seq("a")) }"#));
        assert_eq!(snaps.len(), 2, "{snaps:?}");
        assert!(snaps.iter().all(|s| s.anchor == "seq"));
    }

    #[test]
    fn single_seq_wrapped_in_field_warns() {
        // Field's name arg is an ident, not a string. The lint still
        // fires on the inner seq.
        let snaps = run_lint(&format!(
            r#"{}
            rule k {{ "k" }}
            rule r {{ field(k, seq("a")) }}"#,
            grammar("")
        ));
        assert_eq!(snaps.len(), 1);
    }
}

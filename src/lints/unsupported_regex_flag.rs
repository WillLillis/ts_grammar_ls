//! Warn on regex flag characters that tree-sitter doesn't recognize.
//!
//! Tree-sitter's regex flag parsing accepts `i` (case-insensitive) and
//! silently drops `u` / `v` (the JavaScript Unicode-mode flags) for
//! source-grammar compatibility. Anything else is dropped *with* a
//! warning - which means an author who writes `regexp("foo", "im")`
//! thinking they get multiline mode actually gets just case-insensitive
//! mode, no diagnostic at edit time.
//!
//! We surface this at lint time so the bad flag is visible while typing
//! rather than only at generate.

use std::path::Path;

use tree_sitter_generate::nativedsl::ast::{Node, NodeId, SharedAst, Span};

use crate::document::Module;

use super::walker::push_children;
use super::{LintContext, LintFinding, LintId, LintMeta, Severity};

pub const META: LintMeta = LintMeta {
    name: "unsupported-regex-flag",
    severity: Severity::Warning,
    unnecessary: false,
    description: "\
A flag character passed to `regexp(pattern, flags)` is not recognized \
by tree-sitter and will be silently dropped. Only `i` (case-insensitive) \
has any effect; `u` and `v` are accepted-but-ignored for compatibility.",
};

pub fn run(id: LintId, ctx: &LintContext, out: &mut Vec<LintFinding>) {
    for module in ctx.module.reachable_modules() {
        scan_module(module, id, out);
    }
}

fn scan_module(module: &Module, id: LintId, out: &mut Vec<LintFinding>) {
    let shared = module.shared.as_ref();
    let source = module.source.as_str();
    for &root_id in &module.root_items {
        walk(shared, source, &module.path, root_id, id, out);
    }
}

fn walk(
    shared: &SharedAst,
    source: &str,
    path: &Path,
    root: NodeId,
    id: LintId,
    out: &mut Vec<LintFinding>,
) {
    let mut stack = vec![root];
    while let Some(nid) = stack.pop() {
        let node = shared.arena.get(nid);
        if let Node::DynRegex { flags: Some(flags_id), .. } = *node {
            check_flags(shared, source, path, flags_id, id, out);
        }
        push_children(node, shared, &mut stack);
    }
}

/// Inspect the `flags` argument of a `regexp()` call. We only handle
/// the literal-string case (`Node::StringLit`);
/// dynamic flag strings (let-bound, computed) are skipped - those would
/// need to evaluate the expression to a value, which is lowering's job.
fn check_flags(
    shared: &SharedAst,
    source: &str,
    path: &Path,
    flags_id: NodeId,
    id: LintId,
    out: &mut Vec<LintFinding>,
) {
    let flags_span = shared.arena.span(flags_id);
    let (content_span, content) = match shared.arena.get(flags_id) {
        Node::StringLit(_) => {
            let literal = &source[flags_span.start as usize..flags_span.end as usize];
            let is_quoted_content = flags_span.start > 0
                && source.as_bytes()[flags_span.start as usize - 1] == b'"';
            let inner = if is_quoted_content {
                flags_span
            } else {
                let quote = literal.find('"').unwrap_or(0);
                let suffix = literal
                    .len()
                    .saturating_sub(literal.rfind('"').unwrap_or(literal.len()));
                Span::new(
                    flags_span.start.saturating_add(quote as u32 + 1),
                    flags_span.end.saturating_sub(suffix as u32),
                )
            };
            (
                inner,
                &source[inner.start as usize..inner.end as usize],
            )
        }
        _ => return,
    };

    for (offset_in_content, ch) in content.char_indices() {
        if matches!(ch, 'i' | 'u' | 'v') {
            continue;
        }
        // 1-byte ASCII chars give exact spans; non-ASCII would need
        // char-width computation. Limit precise spans to ASCII and
        // fall back to the whole content span for the rest.
        let char_span = if ch.is_ascii() {
            let start = content_span.start + offset_in_content as u32;
            Span::new(start, start + 1)
        } else {
            content_span
        };
        out.push(LintFinding {
            lint: id,
            path: path.to_path_buf(),
            span: char_span,
            message: format!(
                "unsupported regex flag `{ch}`; tree-sitter only recognizes `i` \
                 (case-insensitive). `{ch}` will be silently dropped.",
            ),
            related: Vec::new(),
            fix: None,
        });
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
                let start = f.span.start as usize;
                let end = f.span.end as usize;
                Snap {
                    anchor: outcome.module.source[start..end].to_owned(),
                    message: f.message,
                }
            })
            .collect()
    }

    fn grammar(rule: &str) -> String {
        format!(
            r#"grammar {{ language: "t" }}
            rule program {{ {rule} }}"#
        )
    }

    #[test]
    fn no_flags_silent() {
        let snaps = run_lint(&grammar(r#"regexp("[a-z]+")"#));
        assert_eq!(snaps, Vec::<Snap>::new());
    }

    #[test]
    fn supported_flag_i_silent() {
        let snaps = run_lint(&grammar(r#"regexp("[a-z]+", "i")"#));
        assert_eq!(snaps, Vec::<Snap>::new());
    }

    #[test]
    fn flags_u_and_v_silent() {
        // Compatibility-accepted flags - silently dropped by core, no
        // diagnostic from us either.
        let snaps = run_lint(&grammar(r#"regexp("[a-z]+", "uv")"#));
        assert_eq!(snaps, Vec::<Snap>::new());
    }

    #[test]
    fn unsupported_flag_m_warns() {
        let snaps = run_lint(&grammar(r#"regexp("[a-z]+", "m")"#));
        assert_eq!(
            snaps,
            vec![Snap {
                anchor: "m".to_owned(),
                message: "unsupported regex flag `m`; tree-sitter only recognizes `i` \
                          (case-insensitive). `m` will be silently dropped."
                    .to_owned(),
            }]
        );
    }

    #[test]
    fn mixed_flags_warn_only_unsupported() {
        // `im` - i is OK, m is not. Only one finding.
        let snaps = run_lint(&grammar(r#"regexp("[a-z]+", "im")"#));
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].anchor, "m");
    }

    #[test]
    fn multiple_unsupported_flags_warn_per_char() {
        let snaps = run_lint(&grammar(r#"regexp("[a-z]+", "ms")"#));
        let anchors: Vec<_> = snaps.iter().map(|s| s.anchor.clone()).collect();
        assert_eq!(anchors, vec!["m".to_owned(), "s".to_owned()]);
    }

    #[test]
    fn raw_string_flags_handled() {
        // Raw-string literal: r#"m"# - we strip the r-prefix + hashes.
        let snaps = run_lint(&grammar(r##"regexp("[a-z]+", r#"m"#)"##));
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].anchor, "m");
    }
}

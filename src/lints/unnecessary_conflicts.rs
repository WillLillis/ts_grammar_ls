//! Surface tree-sitter's "unnecessary conflicts" warning.
//!
//! Unlike the other lints, this one can't be computed from the AST: the parser
//! generator only knows a declared `conflicts:` entry is unnecessary after
//! building the full LR table. Core returns that as a structured
//! `Diagnostic::UnnecessaryConflicts(Vec<Vec<String>>)` from the generate
//! pipeline; [`findings_from_conflicts`] maps each group back onto the
//! `conflicts:` declaration that produced it.
//!
//! The raw groups are obtained from the generate path - the `generate-check`
//! subprocess for the live server, an in-process `generate_parser_for_grammar`
//! call for the `lint` CLI - so this lint's [`run`] is intentionally a no-op
//! and `run_all` never produces it.

use rustc_hash::FxHashSet;
use tower_lsp::lsp_types::TextEdit;
use tree_sitter_generate::nativedsl::ast::Span;

use crate::document::{ConflictDecl, Module};

use super::{LintContext, LintFinding, LintFix, LintId, LintMeta, Severity};

pub const META: LintMeta = LintMeta {
    name: "unnecessary-conflicts",
    severity: Severity::Warning,
    unnecessary: true,
    description: "\
A `conflicts:` entry the generated parser never needs. The declared conflict \
isn't reachable in the LR tables, so listing it only masks ambiguities that \
might later appear at that position. Remove the entry.",
};

/// No-op: findings come from the generate pipeline (see module docs), never
/// from walking `ctx`.
pub const fn run(_id: LintId, _ctx: &LintContext, _out: &mut Vec<LintFinding>) {}

/// Map unnecessary-conflict groups onto the `conflicts:` declarations.
///
/// Each group (rule names, from core's `Diagnostic::UnnecessaryConflicts`) is
/// matched to the declaration in `module` that lists exactly those names,
/// producing a finding anchored on that entry. A group with no matching
/// declaration falls back to the grammar block (then, defensively, file start).
#[must_use]
pub fn findings_from_conflicts<G, S>(groups: &[G], module: &Module) -> Vec<LintFinding>
where
    G: AsRef<[S]>,
    S: AsRef<str>,
{
    // If *every* declared conflict is unnecessary, removing them all would empty
    // the list, so each fix drops the whole `conflicts:` field instead of
    // leaving `conflicts: []`. The fixes are then identical and coalesce when
    // applied together (or applied one at a time, each still removes the field).
    let remove_field = !module.conflict_decls.is_empty() && groups.len() == module.conflict_decls.len();
    groups
        .iter()
        .map(|group| {
            let group = group.as_ref();
            // When we can pin the group to its `conflicts:` entry, anchor there
            // and offer a fix. Otherwise fall back to the grammar block with no
            // fix (we can't safely edit what we can't locate exactly).
            let decl = match_decl(module, group);
            let span = decl
                .map(|d| d.span)
                .or(module.grammar_span)
                .unwrap_or(Span::new(0, 0));
            let fix = decl.map(|d| removal_fix(module, d, group, remove_field));
            LintFinding {
                lint: LintId::UnnecessaryConflicts,
                path: module.path.clone(),
                span,
                message: format!(
                    "unnecessary conflict: `[{}]` is never needed by the generated parser; \
                     remove it from `conflicts`",
                    display_group(group)
                ),
                related: Vec::new(),
                fix,
            }
        })
        .collect()
}

/// The `conflicts:` entry whose rule-name set equals `group`.
fn match_decl<'a, S: AsRef<str>>(
    module: &'a Module,
    group: &[S],
) -> Option<&'a ConflictDecl> {
    let want: FxHashSet<&str> = group.iter().map(AsRef::as_ref).collect();
    module
        .conflict_decls
        .iter()
        .find(|d| d.names.len() == want.len() && d.names.iter().all(|n| want.contains(n.as_str())))
}

/// A fix that deletes the conflict from the `conflicts:` list. When
/// `remove_field` is set (every declared conflict is unnecessary), it drops the
/// whole `conflicts:` field rather than leaving it empty; otherwise it removes
/// just the `[a, b]` entry and one adjacent comma. Both go through the shared
/// [`crate::edits`] primitives.
fn removal_fix<S: AsRef<str>>(
    module: &Module,
    decl: &ConflictDecl,
    group: &[S],
    remove_field: bool,
) -> LintFix {
    let item = if remove_field {
        module
            .conflicts_value_span
            .map_or(decl.span, |v| crate::edits::object_field_span(&module.source, v))
    } else {
        decl.span
    };
    let range = crate::edits::comma_item_deletion(&module.source, item);
    let lsp_range = crate::text::span_to_range(
        &module.rope,
        Span::new(range.start as u32, range.end as u32),
    );
    LintFix {
        title: format!("Remove unnecessary conflict `[{}]`", display_group(group)),
        edits: vec![TextEdit {
            range: lsp_range,
            new_text: String::new(),
        }],
    }
}

fn display_group<S: AsRef<str>>(group: &[S]) -> String {
    group
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::findings_from_conflicts;
    use crate::analysis::analyze;
    use crate::lints::LintId;
    use tower_lsp::lsp_types::{DiagnosticSeverity, DiagnosticTag, Url};
    use tree_sitter_generate::nativedsl::serialize::grammar_to_json;
    use tree_sitter_generate::{Diagnostic, OptLevel};

    /// `[a, b]` is declared as a conflict, but `a`/`b` start with distinct
    /// tokens so the generated parser has no conflict there - tree-sitter
    /// reports it as unnecessary, and the finding lands on the declaration.
    #[test]
    fn unnecessary_conflict_anchored_on_declaration() {
        let src = "grammar {\n    language: \"test\",\n    conflicts: [[a, b]],\n}\n\
                   rule program { choice(a, b) }\nrule a { \"x\" }\nrule b { \"y\" }\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grammar.tsg");
        std::fs::write(&path, src).unwrap();
        let uri = Url::from_file_path(&path).unwrap();

        let outcome = analyze(src.to_owned(), &uri).expect("analyze");
        // By value: `normalize` consumes it and `InputGrammar` isn't `Clone`.
        let Some(Ok(grammar)) = outcome.pipeline else {
            panic!("pipeline must succeed");
        };

        // Generate in-process to obtain tree-sitter's conflict groups.
        let json =
            serde_json::to_string(&grammar_to_json(&grammar.normalize(&mut Vec::new()))).unwrap();
        let mut diags = Vec::new();
        let _ = tree_sitter_generate::generate_parser_for_grammar(&json, None, OptLevel::default(), &mut diags);
        let groups = diags
            .iter()
            .find_map(|d| match d {
                Diagnostic::UnnecessaryConflicts(g) => Some(g.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected an unnecessary-conflict diagnostic, got {diags:?}"));

        let findings = findings_from_conflicts(&groups, &outcome.module);

        // Exactly one finding, anchored on the `[a, b]` declaration (not the
        // grammar-block fallback), tagged as the L005 lint.
        let decl = &outcome.module.conflict_decls;
        assert_eq!(decl.len(), 1);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.lint, LintId::UnnecessaryConflicts);
        assert_eq!(f.path, path);
        assert_eq!(f.span, decl[0].span);
        assert_eq!(&src[f.span.start as usize..f.span.end as usize], "[a, b]");
        assert_eq!(
            f.message,
            format!(
                "unnecessary conflict: `[{}]` is never needed by the generated parser; \
                 remove it from `conflicts`",
                groups[0].join(", ")
            )
        );
        assert!(f.related.is_empty());

        // It's the sole declared conflict, so the fix drops the whole
        // `conflicts:` field rather than leaving an empty list.
        let fix = f.fix.as_ref().expect("fix present");
        assert_eq!(
            fix.title,
            format!("Remove unnecessary conflict `[{}]`", groups[0].join(", "))
        );
        assert_eq!(fix.edits.len(), 1);
        let edit = &fix.edits[0];
        assert_eq!(edit.new_text, "");
        let rope = &outcome.module.rope;
        let es = crate::text::position_to_offset(rope, edit.range.start).unwrap() as usize;
        let ee = crate::text::position_to_offset(rope, edit.range.end).unwrap() as usize;
        // Takes the field's whole line (leading newline + indent through the
        // trailing comma), leaving no dangling blank/indent before `}`.
        assert_eq!(&src[es..ee], "\n    conflicts: [[a, b]],");
        let mut fixed = src.to_string();
        fixed.replace_range(es..ee, "");
        assert!(!fixed.contains("conflicts"), "fixed:\n{fixed}");
        assert!(fixed.contains("language: \"test\",\n}"), "fixed:\n{fixed}");

        // Rendered to the editor as a faded (UNNECESSARY-tagged) warning that
        // carries the fix in `data` for the quick-fix handler.
        let diag = crate::lints::finding_to_diagnostic(f, rope, &uri);
        assert_eq!(diag.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(diag.tags, Some(vec![DiagnosticTag::UNNECESSARY]));
        assert!(diag.data.is_some());
    }
}

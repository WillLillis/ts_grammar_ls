//! Author-facing lints that run after the DSL pipeline succeeds.
//!
//! Each lint lives in its own module under `lints/`. The `define_lints!`
//! macro at the bottom of this file stitches them into a single `LintId`
//! enum + dispatch table - adding a lint means writing one module with a
//! `META` const + a `run` function, then appending one entry to the macro.
//!
//! `LintId`'s discriminant order is the stable code numbering (`L001`,
//! `L002`, ...). NEVER reorder existing entries or insert in the middle:
//! always append.

use std::path::PathBuf;

use rustc_hash::FxHashSet;
use tree_sitter_generate::nativedsl::InputGrammar;
use tree_sitter_generate::nativedsl::ast::Span;

use crate::document::Module;

// Shared helpers used by multiple lints. Lives outside the
// `define_lints!` macro since it isn't a lint itself.
pub mod walker;

/// Severity bucket for a lint finding. Mirrors the subset of LSP severities
/// that make sense for lints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

/// Static metadata for a lint. Declared as a `const META: LintMeta` in each
/// lint's own module so name / severity / description live next to the
/// detection logic.
#[derive(Clone, Copy, Debug)]
pub struct LintMeta {
    pub name: &'static str,
    pub severity: Severity,
    pub description: &'static str,
    /// Whether the finding marks inert, safely-removable code (an unused
    /// import-style "delete this, nothing changes"). Surfaced to editors as
    /// `DiagnosticTag::UNNECESSARY` so the span renders faded.
    pub unnecessary: bool,
}

/// One emitted finding from a lint. The `LintId` carries name/code/default
/// severity, so callers can render without re-deriving those.
#[derive(Clone, Debug)]
pub struct LintFinding {
    pub lint: LintId,
    /// Source file the finding belongs to. May differ from the root
    /// grammar's path when the finding lives in an inherited / imported
    /// helper module. Used by the LSP publisher to group findings by URI
    /// and by the CLI to print the right path in compiler-style output.
    pub path: PathBuf,
    pub span: Span,
    pub message: String,
    /// Optional secondary locations (e.g. "supertype declared here"). Each
    /// entry is `(span, message)` interpreted against the same source
    /// file as `span` / `path`.
    pub related: Vec<(Span, String)>,
    /// Optional auto-fix proposal. When `Some`, the LSP can surface it as
    /// a code action and the CLI `--fix` flag can apply it. `None` means
    /// the lint has no mechanical fix (most authoring footguns).
    pub fix: Option<LintFix>,
}

/// A mechanical edit a lint suggests for its finding. Uses `lsp_types`
/// directly so the LSP code-action path is zero-conversion; the CLI
/// `--fix` flag converts LSP positions back to byte offsets via the
/// document's rope. Multiple edits in one fix MUST be non-overlapping.
///
/// Round-trips through `Diagnostic.data`: [`finding_to_diagnostic`] serializes
/// it there, and the code-action handler deserializes it back to build the
/// quick-fix, so the fix travels with the published diagnostic (no need to
/// re-run the lint - which matters for generate-sourced lints like L005).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LintFix {
    /// Short label shown in the code-action menu (e.g. "Remove alias").
    pub title: String,
    pub edits: Vec<tower_lsp::lsp_types::TextEdit>,
}

/// Inputs available to a lint. A lint picks whichever fields it needs.
/// `grammar` is `None` when the lower pipeline didn't run to completion
/// (most lints will simply early-return in that case).
pub struct LintContext<'a> {
    pub module: &'a Module,
    pub grammar: Option<&'a InputGrammar>,
}

/// Set of lints disabled for a given run (CLI `--allow`, suppression
/// comments later, etc.). Empty set = all lints enabled.
pub type LintSet = FxHashSet<LintId>;

/// Run every enabled lint against `ctx`. Findings are appended in
/// `LintId::ALL` order.
#[must_use]
pub fn run_all(ctx: &LintContext, disabled: &LintSet) -> Vec<LintFinding> {
    let mut out = Vec::new();
    for &id in LintId::ALL {
        if disabled.contains(&id) {
            continue;
        }
        id.run(ctx, &mut out);
    }
    // TODO: comment-based suppression hook goes here once we add a
    // `Suppressions` scanner. Filter out findings whose span is covered by
    // an `// ts_grammar_ls: allow(NAME)` directive in `ctx.module.source`.
    out
}

// ---------------------------------------------------------------------------
// Registry macro
// ---------------------------------------------------------------------------

/// Generate the `LintId` enum, the `ALL` slice, and the central `meta` /
/// `run` dispatch from a list of `Variant => module` entries. Each named
/// module must expose `pub const META: LintMeta` and a
/// `pub fn run(id: LintId, ctx: &LintContext, out: &mut Vec<LintFinding>)`.
macro_rules! define_lints {
    ( $( $variant:ident => $module:ident ),* $(,)? ) => {
        $( pub mod $module; )*

        /// Stable identifier for a lint. The discriminant (1-indexed via
        /// `code()`) is the public code shown in diagnostics; never reorder
        /// existing variants.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u16)]
        pub enum LintId {
            $( $variant, )*
        }

        impl LintId {
            /// All lints, in declaration order. Iterate this for "do X for
            /// every lint" patterns (`--list`, `run_all`, etc.).
            pub const ALL: &'static [Self] = &[ $( Self::$variant ),* ];

            fn meta(self) -> &'static LintMeta {
                match self { $( Self::$variant => &$module::META, )* }
            }

            /// Dispatch into the lint's `run` function.
            pub fn run(self, ctx: &LintContext, out: &mut Vec<LintFinding>) {
                match self { $( Self::$variant => $module::run(self, ctx, out), )* }
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Variant-agnostic methods on LintId (don't grow with new lints)
// ---------------------------------------------------------------------------

impl LintId {
    /// Numeric code: discriminant + 1 so the first lint is L001 (not L000).
    #[must_use]
    pub fn code(self) -> u16 {
        (self as u16) + 1
    }

    /// Display code, e.g. "L001". Used as the diagnostic's `code` field.
    #[must_use]
    pub fn code_str(self) -> String {
        format!("L{:03}", self.code())
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        self.meta().name
    }

    #[must_use]
    pub fn severity(self) -> Severity {
        self.meta().severity
    }

    /// Whether findings should carry `DiagnosticTag::UNNECESSARY` (faded).
    #[must_use]
    pub fn is_unnecessary(self) -> bool {
        self.meta().unnecessary
    }

    #[must_use]
    pub fn description(self) -> &'static str {
        self.meta().description
    }

    /// Resolve a lint by its public name (e.g. `alias-over-supertype`).
    /// Used by CLI flags and (later) suppression comments.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|l| l.name() == name)
    }

    /// Resolve a lint by its numeric code. Accepts the bare `u16` (`1`),
    /// not the `"L001"` form - callers can strip the `L` prefix first.
    #[must_use]
    pub fn from_code(code: u16) -> Option<Self> {
        Self::ALL.iter().copied().find(|l| l.code() == code)
    }
}

// ---------------------------------------------------------------------------
// LSP conversion
// ---------------------------------------------------------------------------

impl Severity {
    #[must_use]
    pub const fn to_lsp(self) -> tower_lsp::lsp_types::DiagnosticSeverity {
        use tower_lsp::lsp_types::DiagnosticSeverity as D;
        match self {
            Self::Error => D::ERROR,
            Self::Warning => D::WARNING,
            Self::Info => D::INFORMATION,
            Self::Hint => D::HINT,
        }
    }
}

/// Convert a finding to an LSP `Diagnostic`. Related spans are resolved in
/// the same module's rope/URI as the primary span.
#[must_use]
pub fn finding_to_diagnostic(
    finding: &LintFinding,
    rope: &ropey::Rope,
    uri: &tower_lsp::lsp_types::Url,
) -> tower_lsp::lsp_types::Diagnostic {
    use tower_lsp::lsp_types::{
        Diagnostic, DiagnosticRelatedInformation, DiagnosticTag, Location, NumberOrString,
    };

    let tags = finding
        .lint
        .is_unnecessary()
        .then(|| vec![DiagnosticTag::UNNECESSARY]);

    let related = if finding.related.is_empty() {
        None
    } else {
        Some(
            finding
                .related
                .iter()
                .map(|(span, msg)| DiagnosticRelatedInformation {
                    location: Location {
                        uri: uri.clone(),
                        range: crate::text::span_to_range(rope, *span),
                    },
                    message: msg.clone(),
                })
                .collect(),
        )
    };

    Diagnostic {
        range: crate::text::span_to_range(rope, finding.span),
        severity: Some(finding.lint.severity().to_lsp()),
        code: Some(NumberOrString::String(finding.lint.code_str())),
        source: Some("ts_grammar_ls".into()),
        message: finding.message.clone(),
        related_information: related,
        tags,
        // Stash the fix (if any) so the code-action handler can offer it as a
        // quick-fix without re-running the lint. Preserved by the editor
        // between publishDiagnostics and codeAction requests.
        data: finding
            .fix
            .as_ref()
            .and_then(|fix| serde_json::to_value(fix).ok()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// The lints themselves
// ---------------------------------------------------------------------------

define_lints! {
    AliasOverSupertype => alias_over_supertype,
    SingleElementCollection => single_element_collection,
    UnsupportedRegexFlag => unsupported_regex_flag,
    EmptyMatchingExtra => empty_matching_extra,
    // Sourced from the generate pipeline, not AST-walking (see the module);
    // its `run` is a no-op, so `run_all` never produces it. Appended last to
    // keep the existing L00x code numbering stable.
    UnnecessaryConflicts => unnecessary_conflicts,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_unique() {
        let codes: Vec<u16> = LintId::ALL.iter().map(|l| l.code()).collect();
        let unique: FxHashSet<u16> = codes.iter().copied().collect();
        assert_eq!(codes.len(), unique.len(), "duplicate lint codes");
        // First lint is L001.
        assert_eq!(LintId::ALL[0].code(), 1);
    }

    #[test]
    fn from_name_roundtrips() {
        for &id in LintId::ALL {
            assert_eq!(LintId::from_name(id.name()), Some(id));
        }
    }

    #[test]
    fn from_code_roundtrips() {
        for &id in LintId::ALL {
            assert_eq!(LintId::from_code(id.code()), Some(id));
        }
    }
}

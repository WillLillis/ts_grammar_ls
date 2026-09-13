//! `textDocument/codeLens` for REPL input buffers.
//!
//! Returns lenses at line 0 for changing the current rule and returning to the
//! owning grammar. Tree-side buffers don't get a lens; the tree format toggle is exposed as a code
//! action on the input buffer instead (see
//! `handlers::code_action::build_toggle_repl_format_action`), because
//! neovim's codelens auto-fetch doesn't fire for unfocused buffers
//! and there's no non-deprecated client-side workaround.

use tower_lsp::lsp_types::{CodeLens, CodeLensParams, Command, Position, Range};

use crate::repl::{ReplInputUri, ReplMeta};
use crate::server::Backend;

#[must_use]
pub fn code_lens(backend: &Backend, params: &CodeLensParams) -> Option<Vec<CodeLens>> {
    let input_uri = ReplInputUri::try_from_uri(&params.text_document.uri)?;
    input_buffer_lenses(backend, &input_uri)
}

fn input_buffer_lenses(backend: &Backend, input_uri: &ReplInputUri) -> Option<Vec<CodeLens>> {
    // Rule comes from the in-memory session if this process owns one,
    // otherwise from the sibling metadata file. Either way the buffer
    // text is irrelevant - the rule binding is server-owned state.
    let rule = backend
        .repl_sessions
        .get(input_uri)
        .map(|s| s.lock().unwrap().current_rule.clone())
        .or_else(|| ReplMeta::read_for(input_uri).map(|m| m.current_rule))?;

    let range = Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: 0,
            character: 0,
        },
    };
    let mut lenses = vec![CodeLens {
        range,
        command: Some(Command {
            title: format!("Rule: {rule}"),
            command: crate::handlers::repl::SET_REPL_RULE_COMMAND.into(),
            arguments: Some(vec![
                serde_json::json!({ "uri": input_uri.as_url().to_string() }),
            ]),
        }),
        data: None,
    }];

    if let Some(meta) = ReplMeta::read_for(input_uri) {
        lenses.push(CodeLens {
            range,
            command: Some(Command {
                title: "Return to grammar".into(),
                command: crate::handlers::repl::RETURN_TO_GRAMMAR_COMMAND.into(),
                // Include every URI a client override needs to focus the
                // grammar and close the two REPL windows without another
                // server round trip. The server fallback only reads `uri`.
                arguments: Some(vec![serde_json::json!({
                    "uri": input_uri.as_url().to_string(),
                    "input_uri": input_uri.as_url().to_string(),
                    "tree_uri": input_uri.tree_uri().as_url().to_string(),
                    "grammar_uri": meta.grammar_uri.to_string(),
                })]),
            }),
            data: None,
        });
    }

    Some(lenses)
}

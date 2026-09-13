use std::sync::Arc;

use serde_json::to_value;
use tower::{Service, ServiceExt};
use tower_lsp::LspService;
use tower_lsp::jsonrpc::Request;
use tower_lsp::lsp_types::notification::{
    DidChangeConfiguration, DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument,
    DidOpenTextDocument, DidSaveTextDocument, Initialized,
};
use tower_lsp::lsp_types::request::{
    CodeActionRequest, Completion, DocumentHighlightRequest, DocumentSymbolRequest, ExecuteCommand,
    Formatting, GotoDefinition, HoverRequest, Initialize, PrepareRenameRequest, References, Rename,
    SemanticTokensFullRequest,
};
use tower_lsp::lsp_types::*;

use ts_grammar_ls::config::{Config, DiagnosticConfig};
use ts_grammar_ls::hover_docs;
use ts_grammar_ls::server::Backend;

const ID: i64 = 1;

/// Shared log of `textDocument/publishDiagnostics` notifications observed
/// during a test, in order. Tests inspect this to verify cross-file
/// republishing behavior.
type PublishLog = Arc<tokio::sync::Mutex<Vec<PublishDiagnosticsParams>>>;

async fn init(documents: &[(Url, &str)]) -> LspService<Backend> {
    init_with_capture(documents, None).await.0
}

/// Spin up a service, optionally advertising a workspace folder (which
/// triggers the `initialized` -> watcher registration + workspace scan
/// flow). Returns the service plus a log of all `publishDiagnostics`
/// notifications observed since startup.
async fn init_with_capture(
    documents: &[(Url, &str)],
    workspace_root: Option<&std::path::Path>,
) -> (LspService<Backend>, PublishLog) {
    let config = Config {
        diagnostics: DiagnosticConfig {
            generate_diagnostics: false,
        },
        ..Default::default()
    };
    let repl_cache_dir = tempfile::tempdir().expect("repl cache tempdir");
    let (mut service, socket) = LspService::build(|client| Backend {
        client,
        document_map: Arc::new(dashmap::DashMap::new()),
        publish_handle: Arc::new(dashmap::DashMap::new()),
        generate_child: Arc::default(),
        dependents: Arc::new(dashmap::DashMap::new()),
        closed_file_deps: Arc::new(dashmap::DashMap::new()),
        workspace_roots: Arc::default(),
        config: Arc::new(config.into()),
        repl_cache: Arc::new(ts_grammar_ls::repl::ReplCache::new(
            repl_cache_dir.path().to_path_buf(),
        )),
        repl_sessions: Arc::new(dashmap::DashMap::new()),
    })
    .finish();
    // Keep the tempdir alive for the service's lifetime by leaking it -
    // dropping would delete the dir mid-test if any REPL compile lands.
    std::mem::forget(repl_cache_dir);

    // Drain server-to-client messages so internal buffers don't fill up
    // (multiple `publish_diagnostics` calls would otherwise deadlock).
    // While draining, capture publishDiagnostics notifications for tests
    // that need to verify cross-file republishing.
    let publish_log: PublishLog = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let log_writer = Arc::clone(&publish_log);
    let mut socket = socket;
    tokio::spawn(async move {
        use futures_util::stream::StreamExt;
        while let Some(msg) = socket.next().await {
            if msg.method() == "textDocument/publishDiagnostics"
                && let Some(params) = msg.params()
                && let Ok(parsed) =
                    serde_json::from_value::<PublishDiagnosticsParams>(params.clone())
            {
                log_writer.lock().await.push(parsed);
            }
        }
    });

    let workspace_folders = workspace_root.map(|p| {
        vec![WorkspaceFolder {
            uri: Url::from_file_path(p).unwrap(),
            name: "test-workspace".into(),
        }]
    });

    lsp_request::<Initialize>(
        &mut service,
        InitializeParams {
            capabilities: ClientCapabilities::default(),
            initialization_options: Some(serde_json::json!({
                "diagnostics": { "generate_diagnostics": false }
            })),
            workspace_folders,
            ..Default::default()
        },
    )
    .await;

    if workspace_root.is_some() {
        // Triggers `initialized` -> watcher registration + workspace scan.
        // The scan is spawn_blocking; sleep gives it time to finish before
        // tests inspect the resulting index.
        lsp_notify::<Initialized>(&mut service, InitializedParams {}).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    for (uri, text) in documents {
        lsp_notify::<DidOpenTextDocument>(
            &mut service,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "tsg".into(),
                    version: 0,
                    text: text.to_string(),
                },
            },
        )
        .await;
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (service, publish_log)
}

async fn lsp_request<R: tower_lsp::lsp_types::request::Request>(
    service: &mut LspService<Backend>,
    params: R::Params,
) -> R::Result {
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(
            Request::build(R::METHOD)
                .id(ID)
                .params(to_value(params).unwrap())
                .finish(),
        )
        .await
        .unwrap()
        .unwrap();
    serde_json::from_value(resp.result().unwrap().clone()).unwrap()
}

async fn lsp_notify<R: tower_lsp::lsp_types::notification::Notification>(
    service: &mut LspService<Backend>,
    params: R::Params,
) {
    service
        .ready()
        .await
        .unwrap()
        .call(
            Request::build(R::METHOD)
                .params(to_value(params).unwrap())
                .finish(),
        )
        .await
        .unwrap();
}

fn test_uri() -> Url {
    let path = "/tmp/test_grammar.tsg";
    // Ensure the file exists so parse_native_dsl can canonicalize it.
    if !std::path::Path::new(path).exists() {
        std::fs::write(path, "").ok();
    }
    Url::parse("file:///tmp/test_grammar.tsg").unwrap()
}

// ---------------------------------------------------------------------------
// Request helpers - cut boilerplate for common LSP requests
// ---------------------------------------------------------------------------

async fn hover_at(service: &mut LspService<Backend>, uri: Url, pos: Position) -> Option<Hover> {
    lsp_request::<HoverRequest>(
        service,
        HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
}

async fn goto_def_at(
    service: &mut LspService<Backend>,
    uri: Url,
    pos: Position,
) -> Option<GotoDefinitionResponse> {
    lsp_request::<GotoDefinition>(
        service,
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
}

async fn references_at(
    service: &mut LspService<Backend>,
    uri: Url,
    pos: Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    lsp_request::<References>(
        service,
        ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration,
            },
        },
    )
    .await
}

async fn highlights_at(
    service: &mut LspService<Backend>,
    uri: Url,
    pos: Position,
) -> Option<Vec<DocumentHighlight>> {
    lsp_request::<DocumentHighlightRequest>(
        service,
        DocumentHighlightParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
}

async fn completions_at(
    service: &mut LspService<Backend>,
    uri: Url,
    pos: Position,
) -> Vec<CompletionItem> {
    let result: Option<CompletionResponse> = lsp_request::<Completion>(
        service,
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: None,
        },
    )
    .await;
    match result {
        Some(CompletionResponse::Array(items)) => items,
        _ => Vec::new(),
    }
}

#[expect(clippy::unnecessary_wraps, reason = "match usage in assertions")]
fn make_hover(value: &str) -> Option<Hover> {
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: value.to_string(),
        }),
        range: None,
    })
}

const SIMPLE_GRAMMAR: &str = r#"
grammar { language: "test" }

macro commaSep1(item: rule_t) rule_t {
    seq(item, repeat(seq(",", item)))
}

macro commaSep(item: rule_t) rule_t { optional(commaSep1(item)) }

let PREC = { ADD: 1, MUL: 2 }

rule program { repeat(_statement) }

rule _statement { choice(expression_statement, return_statement) }

rule expression_statement { seq(_expression, ";") }

rule return_statement { seq("return", optional(_expression), ";") }

rule _expression {
    choice(
        binary_expression,
        identifier,
        number,
    )
}

rule binary_expression {
    prec_left(PREC.ADD, seq(field(left, _expression), "+", field(right, _expression)))
}

rule identifier { regexp(r"[a-zA-Z_]\w*") }
rule number { regexp(r"[0-9]+") }
"#;

// ---------------------------------------------------------------------------
// Hover tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn hover_builtin_combinator() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Hover over "seq" in the commaSep1 function body (line 4, col 4)
    let result = hover_at(&mut service, test_uri(), Position::new(4, 4)).await;

    assert_eq!(result, make_hover(hover_docs::SEQ));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_user_defined_rule() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Hover over "program" reference in _statement's body won't work since
    // it's not referenced. Instead hover on the rule name itself.
    // "rule program" is on line 11
    let result = hover_at(&mut service, test_uri(), Position::new(11, 6)).await;

    assert_eq!(result, make_hover("```\nrule program\n```"));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_user_defined_function() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // `macro commaSep1` is on line 3; col 6 is the `c` of `commaSep1`.
    let result = hover_at(&mut service, test_uri(), Position::new(3, 6)).await;

    assert_eq!(
        result,
        make_hover("```\nmacro commaSep1(item: rule_t) rule_t\n```")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_keyword() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Hover over "let" on line 9
    let result = hover_at(&mut service, test_uri(), Position::new(9, 0)).await;

    assert_eq!(result, make_hover(hover_docs::KW_LET));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_returns_none_on_whitespace() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let result = hover_at(&mut service, test_uri(), Position::new(0, 0)).await;

    assert!(result.is_none());
}

// ---------------------------------------------------------------------------
// Go-to-definition tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn goto_def_rule_reference() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // In _statement (line 13), "expression_statement" is a rule reference at col 24.
    let result = goto_def_at(&mut service, test_uri(), Position::new(13, 28)).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, test_uri());
    // Should point to the name span of `rule expression_statement` on line 15.
    assert_eq!(loc.range.start.line, 15);
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_function_reference() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // In commaSep body, "commaSep1" is a function call.
    // Line 7: macro commaSep(item: rule_t) rule_t { optional(commaSep1(item)) }
    // "commaSep1" starts at col 53
    let result = goto_def_at(&mut service, test_uri(), Position::new(7, 53)).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, test_uri());
    // Should point to macro commaSep1 definition on line 3
    assert_eq!(loc.range.start.line, 3);
}

// ---------------------------------------------------------------------------
// Completion tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn completion_includes_rules_and_builtins() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let result: Option<CompletionResponse> = lsp_request::<Completion>(
        &mut service,
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: test_uri() },
                position: Position::new(12, 0),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: None,
        },
    )
    .await;

    let CompletionResponse::Array(items) = result.unwrap() else {
        panic!("expected array response");
    };

    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

    // Should include user-defined rules
    assert!(labels.contains(&"program"));
    assert!(labels.contains(&"binary_expression"));
    // Should include user-defined functions
    assert!(labels.contains(&"commaSep1"));
    assert!(labels.contains(&"commaSep"));
    // Should include user-defined let bindings
    assert!(labels.contains(&"PREC"));
    // Should include builtin combinators
    assert!(labels.contains(&"seq"));
    assert!(labels.contains(&"choice"));
    assert!(labels.contains(&"token_immediate"));
    // Should include keywords
    assert!(labels.contains(&"rule"));
    assert!(labels.contains(&"macro"));
    // Should include type keywords
    assert!(labels.contains(&"rule_t"));
    assert!(labels.contains(&"str_t"));
}

// ---------------------------------------------------------------------------
// Document symbol tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_lists_all_definitions() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let DocumentSymbolResponse::Nested(symbols) = result.unwrap() else {
        panic!("expected nested symbols");
    };

    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

    assert!(names.contains(&"program"));
    assert!(names.contains(&"_statement"));
    assert!(names.contains(&"commaSep1"));
    assert!(names.contains(&"commaSep"));
    assert!(names.contains(&"PREC"));
    assert!(names.contains(&"binary_expression"));

    // Check kinds
    let fn_sym = symbols.iter().find(|s| s.name == "commaSep1").unwrap();
    assert_eq!(fn_sym.kind, SymbolKind::FUNCTION);

    let rule_sym = symbols.iter().find(|s| s.name == "program").unwrap();
    assert_eq!(rule_sym.kind, SymbolKind::CLASS);

    let let_sym = symbols.iter().find(|s| s.name == "PREC").unwrap();
    assert_eq!(let_sym.kind, SymbolKind::VARIABLE);
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_nest_object_keys_under_let() {
    // Object literal keys are surfaced as `Field` children of their owning
    // `Let` binding, so the outline view lets users navigate to e.g. PREC.ADD.
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1, MUL: 2 }
rule program { "x" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    #[expect(
        deprecated,
        reason = "DocumentSymbol::deprecated is deprecated but required by the struct"
    )]
    let expected = DocumentSymbolResponse::Nested(vec![
        DocumentSymbol {
            name: "PREC".into(),
            kind: SymbolKind::VARIABLE,
            detail: None,
            range: Range::new(Position::new(2, 0), Position::new(2, 29)),
            selection_range: Range::new(Position::new(2, 4), Position::new(2, 8)),
            tags: None,
            deprecated: None,
            children: Some(vec![
                DocumentSymbol {
                    name: "ADD".into(),
                    kind: SymbolKind::FIELD,
                    detail: None,
                    range: Range::new(Position::new(2, 13), Position::new(2, 16)),
                    selection_range: Range::new(Position::new(2, 13), Position::new(2, 16)),
                    tags: None,
                    deprecated: None,
                    children: None,
                },
                DocumentSymbol {
                    name: "MUL".into(),
                    kind: SymbolKind::FIELD,
                    detail: None,
                    range: Range::new(Position::new(2, 21), Position::new(2, 24)),
                    selection_range: Range::new(Position::new(2, 21), Position::new(2, 24)),
                    tags: None,
                    deprecated: None,
                    children: None,
                },
            ]),
        },
        DocumentSymbol {
            name: "program".into(),
            kind: SymbolKind::CLASS,
            detail: None,
            range: Range::new(Position::new(3, 0), Position::new(3, 20)),
            selection_range: Range::new(Position::new(3, 5), Position::new(3, 12)),
            tags: None,
            deprecated: None,
            children: None,
        },
    ]);

    assert_eq!(result, Some(expected));
}

// ---------------------------------------------------------------------------
// Diagnostics tests
// ---------------------------------------------------------------------------

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn diagnostics_clean_grammar_no_errors() {
    let service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(doc.dsl_diagnostics, vec![]);
}

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn diagnostics_syntax_error() {
    let broken = r#"grammar { language: "test" } rule program {"#;
    let service = init(&[(test_uri(), broken)]).await;

    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(
        doc.dsl_diagnostics,
        vec![Diagnostic {
            range: Range::new(Position::new(0, 43), Position::new(0, 43)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("ts_grammar_ls".into()),
            message: "expected expression".into(),
            ..Default::default()
        }]
    );
}

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn diagnostics_type_error() {
    let bad_types = r#"
        grammar { language: "test" }
        macro bad(x: rule_t) int_t { x }
        rule program { "x" }
    "#;
    let service = init(&[(test_uri(), bad_types)]).await;

    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(
        doc.dsl_diagnostics,
        vec![Diagnostic {
            range: Range::new(Position::new(2, 37), Position::new(2, 38)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("ts_grammar_ls".into()),
            message: "expected int_t, got rule_t".into(),
            ..Default::default()
        }]
    );
}

// ---------------------------------------------------------------------------
// Semantic token tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn semantic_tokens_includes_parameter_declaration() {
    // Parameter declaration sites must be emitted as VARIABLE+DECLARATION,
    // matching how parameter uses inside the body are emitted (just as
    // VARIABLE without DECLARATION). Asserts the full decoded token list.
    let grammar = "grammar { language: \"test\" }\nmacro foo(x: rule_t) rule_t { x }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    let result: Option<SemanticTokensResult> = lsp_request::<SemanticTokensFullRequest>(
        &mut service,
        SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
        panic!("expected full tokens");
    };

    // Decode delta-encoded tokens to (line, col, length, type, mod).
    let mut line = 0u32;
    let mut col = 0u32;
    let decoded: Vec<(u32, u32, u32, u32, u32)> = tokens
        .data
        .iter()
        .map(|t| {
            line += t.delta_line;
            if t.delta_line > 0 {
                col = 0;
            }
            col += t.delta_start;
            (line, col, t.length, t.token_type, t.token_modifiers_bitset)
        })
        .collect();

    // Legend: 0=FUNCTION, 1=VARIABLE, 2=TYPE, 3=CLASS. Modifier bit 0 = DECLARATION.
    assert_eq!(
        decoded,
        vec![
            (1, 6, 3, 0, 1),  // `foo` (function decl)
            (1, 10, 1, 1, 1), // `x` parameter decl
            (1, 13, 6, 2, 0), // `rule_t` param type
            (1, 21, 6, 2, 0), // `rule_t` return type
            (1, 30, 1, 1, 0), // `x` parameter use
        ],
    );
}

#[tokio::test(flavor = "current_thread")]
async fn semantic_tokens_classifies_identifiers() {
    let grammar = r#"
grammar { language: "test" }
macro helper(x: rule_t) rule_t { x }
rule program { helper(identifier) }
rule identifier { regexp(r"[a-z]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let result: Option<SemanticTokensResult> = lsp_request::<SemanticTokensFullRequest>(
        &mut service,
        SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
        panic!("expected full tokens");
    };

    // Should have semantic tokens for identifiers only (not keywords/strings/etc).
    assert!(!tokens.data.is_empty(), "expected some semantic tokens");

    // Token types from the legend:
    // 0 = function, 1 = variable, 2 = type, 3 = class
    let token_types: Vec<u32> = tokens.data.iter().map(|t| t.token_type).collect();

    // Should contain function tokens (helper def + helper call)
    assert!(token_types.contains(&0), "expected function tokens");
    // Should contain class tokens (rule names: program, identifier)
    assert!(token_types.contains(&3), "expected class tokens");
    // Should contain type tokens (rule_t annotations)
    assert!(token_types.contains(&2), "expected type tokens");
}

// ---------------------------------------------------------------------------
// Hover with type info
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn hover_let_binding_shows_type() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1, MUL: 2 }
rule program { prec(PREC.ADD, "x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Hover over "PREC" on line 2
    let result = hover_at(&mut service, test_uri(), Position::new(2, 5)).await;

    assert_eq!(result, make_hover("```\nlet PREC: obj_t<int_t>\n```"));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_let_binding_recovers_annotation_and_reference_types() {
    let grammar = r#"
grammar { language: "test" }
let words: list_t<str_t> = []
let copy = words
rule program { "x" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    assert_eq!(
        hover_at(&mut service, test_uri(), Position::new(2, 5)).await,
        make_hover("```\nlet words: list_t<str_t>\n```")
    );
    assert_eq!(
        hover_at(&mut service, test_uri(), Position::new(3, 5)).await,
        make_hover("```\nlet copy: list_t<str_t>\n```")
    );
}

// ---------------------------------------------------------------------------
// Go-to-definition: object fields
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn goto_def_object_field() {
    // PREC is on line 2, ADD is at col 16
    // Usage of PREC.ADD is on line 3
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1, MUL: 2 }
rule program { prec(PREC.ADD, "x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "ADD" in "PREC.ADD" on line 3
    // line 3: `rule program { prec(PREC.ADD, "x") }`
    //                                   ^ col 25
    let result = goto_def_at(&mut service, test_uri(), Position::new(3, 25)).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, test_uri());
    // Should point to "ADD" in the object literal on line 2
    assert_eq!(loc.range.start.line, 2);
}

// ---------------------------------------------------------------------------
// Go-to-definition: inherited rules and inherit path
// ---------------------------------------------------------------------------

/// Create a temporary directory with a base grammar and a derived grammar
/// that inherits from it.
struct InheritFixture {
    _dir: tempfile::TempDir,
    base_path: std::path::PathBuf,
    _base_text: String,
    derived_path: std::path::PathBuf,
    derived_text: String,
}

fn create_inherit_fixture() -> InheritFixture {
    let dir = tempfile::tempdir().unwrap();

    let base_text = r#"
grammar { language: "base_lang" }
rule program { repeat(_statement) }
rule _statement { choice(expression, "x") }
rule expression { choice(identifier, number) }
rule identifier { regexp(r"[a-z]+") }
rule number { regexp(r"[0-9]+") }
"#;
    let base_path = dir.path().join("base.tsg");
    std::fs::write(&base_path, base_text).unwrap();

    let derived_text = format!(
        r#"
let base = inherit("{}")
grammar {{
    language: "derived_lang",
    inherits: base,
    extras: grammar_config(base, extras),
}}
override rule _statement {{ choice(base::_statement, new_rule) }}
rule new_rule {{ seq("new", expression) }}
"#,
        base_path.display()
    );
    let derived_path = dir.path().join("derived.tsg");
    std::fs::write(&derived_path, &derived_text).unwrap();

    InheritFixture {
        _dir: dir,
        base_path,
        _base_text: base_text.to_string(),
        derived_path,
        derived_text,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_inherited_rule() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // "expression" on the last line: `rule new_rule { seq("new", expression) }`
    // This is an inherited rule - goto-def should jump to the base grammar.
    let expr_offset = fix.derived_text.find("expression").unwrap();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, expr_offset as u32);

    let result = goto_def_at(&mut service, derived_uri, pos).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, base_uri, "should jump to base grammar");
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_base_inline_rule() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // "base::_statement" - cursor on "_statement" after "::"
    let base_stmt_offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, base_stmt_offset as u32);

    let result = goto_def_at(&mut service, derived_uri, pos).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, base_uri, "base:: should jump to base grammar");
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_inherit_path() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor on the path string inside inherit("...")
    let path_offset = fix
        .derived_text
        .find(&fix.base_path.display().to_string())
        .unwrap();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, path_offset as u32);

    let result = goto_def_at(&mut service, derived_uri, pos).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, base_uri, "should jump to base grammar file");
}

// ---------------------------------------------------------------------------
// Additional hover coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn hover_type_keyword() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Hover over "rule_t" in macro signature (line 3: macro commaSep1(item: rule_t) ...)
    // "rule_t" starts at col 22.
    let result = hover_at(&mut service, test_uri(), Position::new(3, 23)).await;

    assert_eq!(result, make_hover(hover_docs::TYPE_RULE_T));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_variable_reference() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1 }
rule program { prec(PREC.ADD, "x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Hover over "PREC" in usage on line 3 (not definition)
    // line 3: `rule program { prec(PREC.ADD, "x") }`
    //                              ^ col 20
    let result = hover_at(&mut service, test_uri(), Position::new(3, 21)).await;

    assert_eq!(result, make_hover("```\nlet PREC: obj_t<int_t>\n```"));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_object_field_shows_value() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 10, MUL: 20 }
rule program { prec(PREC.ADD, "x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Hover over "ADD" in `PREC.ADD` on line 3
    // line 3: `rule program { prec(PREC.ADD, "x") }`
    //                                   ^ col 25
    let result = hover_at(&mut service, test_uri(), Position::new(3, 25)).await;

    assert_eq!(result, make_hover("```\nPREC.ADD = 10\n```"));
}

// ---------------------------------------------------------------------------
// Additional go-to-definition coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn goto_def_let_variable_reference() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1 }
rule program { prec(PREC.ADD, "x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "PREC" in usage (line 3, col 21)
    let result = goto_def_at(&mut service, test_uri(), Position::new(3, 21)).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, test_uri());
    // Should jump to "PREC" definition on line 2
    assert_eq!(loc.range.start.line, 2);
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_returns_none_for_builtin() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Cursor on "seq" builtin (line 4, col 4)
    let result = goto_def_at(&mut service, test_uri(), Position::new(4, 5)).await;

    assert!(result.is_none(), "builtins should not have a definition");
}

// ---------------------------------------------------------------------------
// Additional completion coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn completion_item_kinds_are_correct() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let result: Option<CompletionResponse> = lsp_request::<Completion>(
        &mut service,
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: test_uri() },
                position: Position::new(12, 0),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: None,
        },
    )
    .await;

    let CompletionResponse::Array(items) = result.unwrap() else {
        panic!("expected array response");
    };

    let find = |name: &str| items.iter().find(|i| i.label == name).unwrap();

    assert_eq!(find("program").kind, Some(CompletionItemKind::CLASS));
    assert_eq!(find("commaSep1").kind, Some(CompletionItemKind::FUNCTION));
    assert_eq!(find("PREC").kind, Some(CompletionItemKind::VARIABLE));
    assert_eq!(find("seq").kind, Some(CompletionItemKind::FUNCTION));
    assert_eq!(find("rule").kind, Some(CompletionItemKind::KEYWORD));
    assert_eq!(
        find("rule_t").kind,
        Some(CompletionItemKind::TYPE_PARAMETER)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn completion_object_fields_after_dot() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1, MUL: 2, SUB: 3 }
rule program { prec(PREC., "x") }"#;
    //                   ^ cursor after the dot
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor right after "PREC."
    let items = completions_at(&mut service, test_uri(), Position::new(3, 25)).await;

    let ci = |label: &str| CompletionItem {
        label: label.into(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(format!("PREC.{label}")),
        ..Default::default()
    };
    assert_eq!(items, vec![ci("ADD"), ci("MUL"), ci("SUB")]);
}

#[tokio::test(flavor = "current_thread")]
async fn completion_base_rules_after_double_colon() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Find "base::" and place cursor right after it.
    let offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let mut items = completions_at(&mut service, derived_uri, pos).await;
    items.sort_by(|a, b| a.label.cmp(&b.label));

    let ci = |label: &str| CompletionItem {
        label: label.into(),
        kind: Some(CompletionItemKind::CLASS),
        detail: Some(format!("rule {label} (base)")),
        ..Default::default()
    };
    assert_eq!(
        items,
        vec![
            ci("_statement"),
            ci("expression"),
            ci("identifier"),
            ci("number"),
            ci("program"),
        ]
    );
}

// ---------------------------------------------------------------------------
// Additional document symbol coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_have_valid_ranges() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let DocumentSymbolResponse::Nested(symbols) = result.unwrap() else {
        panic!("expected nested symbols");
    };

    for sym in &symbols {
        // selection_range should be within range
        assert!(
            sym.selection_range.start >= sym.range.start,
            "{}: selection start before range start",
            sym.name
        );
        assert!(
            sym.selection_range.end <= sym.range.end,
            "{}: selection end after range end",
            sym.name
        );
        // selection_range should be non-empty (name span)
        assert!(
            sym.selection_range.start != sym.selection_range.end
                || sym.selection_range.start.line != sym.selection_range.end.line,
            "{}: empty selection range",
            sym.name
        );
    }
}

// ---------------------------------------------------------------------------
// Additional diagnostics coverage
// ---------------------------------------------------------------------------

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn diagnostics_resolve_error() {
    let bad = r#"
        grammar { language: "test" }
        rule program { undefined_rule }
    "#;
    let service = init(&[(test_uri(), bad)]).await;

    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(
        doc.dsl_diagnostics,
        vec![Diagnostic {
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("ts_grammar_ls".into()),
            message: "unknown identifier 'undefined_rule'".into(),
            range: Range::new(Position::new(2, 23), Position::new(2, 37)),
            ..Default::default()
        }]
    );
}

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn diagnostics_lex_error() {
    let bad = r#"grammar { language: "test } rule program { "x" }"#;
    let service = init(&[(test_uri(), bad)]).await;

    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(
        doc.dsl_diagnostics,
        vec![Diagnostic {
            range: Range::new(Position::new(0, 45), Position::new(0, 48)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("ts_grammar_ls".into()),
            message: "unterminated string literal".into(),
            ..Default::default()
        }]
    );
}

// ---------------------------------------------------------------------------
// Additional semantic token coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn semantic_tokens_skips_non_ident_tokens() {
    let grammar = r#"
grammar { language: "test" }
rule program { seq("hello", repeat("world")) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let result: Option<SemanticTokensResult> = lsp_request::<SemanticTokensFullRequest>(
        &mut service,
        SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
        panic!("expected full tokens");
    };

    // This grammar has very few identifiers (just "program" at definition site).
    // Keywords (grammar, language, rule, seq, repeat) and strings ("hello", "world", "test")
    // should NOT produce semantic tokens.
    // The only semantic tokens should be for the "program" identifier.
    assert!(
        tokens.data.len() <= 2,
        "expected very few semantic tokens (only idents), got {}",
        tokens.data.len()
    );
}

// ---------------------------------------------------------------------------
// References tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn references_finds_all_usages() {
    // _expression is defined on line 5 and referenced on lines 3, 4, 7
    let grammar = r#"
grammar { language: "test" }
rule program { repeat(_expression) }
rule return_statement { seq("return", optional(_expression)) }

rule _expression { choice(identifier, number) }

rule identifier { regexp(r"[a-z]+") }
rule number { regexp(r"[0-9]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "_expression" definition (line 5, col 5)
    let result = references_at(&mut service, test_uri(), Position::new(5, 6), true).await;

    let mut locs = result.unwrap();
    locs.sort_by_key(|l| l.range.start.line);
    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    // Definition (line 5) + 2 references (lines 2, 3)
    assert_eq!(locs, vec![loc(2, 22, 33), loc(3, 47, 58), loc(5, 5, 16),]);
}

#[tokio::test(flavor = "current_thread")]
async fn references_finds_usages_inside_grammar_block() {
    // `identifier` is used in grammar fields (word, extras, inline, conflicts,
    // precedences) as well as in rule bodies.
    let grammar = r#"
grammar {
    language: "test",
    word: identifier,
    extras: [identifier, whitespace],
    inline: [identifier],
    conflicts: [[identifier, expression]],
    precedences: [[identifier, expression]],
}
rule program { seq(identifier, "x") }
rule expression { identifier }
rule identifier { regexp(r"[a-z]+") }
rule whitespace { regexp(r"\s+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on `identifier` in the definition (line 11).
    let result = references_at(&mut service, test_uri(), Position::new(11, 6), true).await;

    let mut locs = result.unwrap();
    locs.sort_by_key(|l| (l.range.start.line, l.range.start.character));

    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    assert_eq!(
        locs,
        vec![
            loc(3, 10, 20),  // word: identifier,
            loc(4, 13, 23),  // extras: [identifier, ...
            loc(5, 13, 23),  // inline: [identifier],
            loc(6, 17, 27),  // conflicts: [[identifier, ...
            loc(7, 19, 29),  // precedences: [[identifier, ...
            loc(9, 19, 29),  // rule program { seq(identifier, ...
            loc(10, 18, 28), // rule expression { identifier }
            loc(11, 5, 15),  // rule identifier { ... } (definition)
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_without_declaration() {
    let grammar = r#"
grammar { language: "test" }
rule program { repeat(_expression) }
rule return_statement { seq("return", optional(_expression)) }

rule _expression { choice(identifier, number) }

rule identifier { regexp(r"[a-z]+") }
rule number { regexp(r"[0-9]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let result = references_at(&mut service, test_uri(), Position::new(5, 6), false).await;

    let mut locs = result.unwrap();
    locs.sort_by_key(|l| l.range.start.line);
    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    assert_eq!(locs, vec![loc(2, 22, 33), loc(3, 47, 58),]);
}

#[tokio::test(flavor = "current_thread")]
async fn references_returns_none_for_unknown() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Cursor on whitespace
    let result = references_at(&mut service, test_uri(), Position::new(0, 0), true).await;

    assert_eq!(result, None);
}

// ---------------------------------------------------------------------------
// didChangeConfiguration tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn did_change_configuration_updates_config() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Default config has generate_diagnostics = true, but our test init
    // overrides it to false. Verify that.
    assert!(
        !service
            .inner()
            .config
            .read()
            .await
            .diagnostics
            .generate_diagnostics
    );

    // Send a config change enabling it.
    lsp_notify::<DidChangeConfiguration>(
        &mut service,
        DidChangeConfigurationParams {
            settings: serde_json::json!({
                "diagnostics": { "generate_diagnostics": true }
            }),
        },
    )
    .await;

    assert!(
        service
            .inner()
            .config
            .read()
            .await
            .diagnostics
            .generate_diagnostics
    );
}

#[tokio::test(flavor = "current_thread")]
async fn did_change_configuration_ignores_invalid() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let before = service
        .inner()
        .config
        .read()
        .await
        .diagnostics
        .generate_diagnostics;

    // Send garbage - should be silently ignored.
    lsp_notify::<DidChangeConfiguration>(
        &mut service,
        DidChangeConfigurationParams {
            settings: serde_json::json!("not an object"),
        },
    )
    .await;

    let after = service
        .inner()
        .config
        .read()
        .await
        .diagnostics
        .generate_diagnostics;

    assert_eq!(before, after);
}

// ---------------------------------------------------------------------------
// Additional hover coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn hover_grammar_config_field() {
    let grammar = r#"
grammar { language: "test" }
rule program { "x" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Hover on "language" inside the grammar block should show config field docs.
    let result = hover_at(&mut service, test_uri(), Position::new(1, 11)).await;

    assert_eq!(result, make_hover(hover_docs::CFG_LANGUAGE));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_reserved_config_field_not_builtin() {
    let grammar = r#"
grammar {
    language: "test",
    reserved: { default: ["if", "else"] },
}
rule program { "x" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Hover on "reserved" inside the grammar block should show config field
    // docs, NOT the reserved() builtin combinator docs.
    let result = hover_at(&mut service, test_uri(), Position::new(3, 6)).await;

    assert_eq!(result, make_hover(hover_docs::CFG_RESERVED));
}

#[tokio::test(flavor = "current_thread")]
async fn references_reserved_builtin_excludes_config_field() {
    let grammar = r#"
grammar {
    language: "test",
    reserved: { default: ["if"] },
}
rule program { reserved("default", identifier) }
rule identifier { regexp(r"[a-z]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // References on "reserved" in the rule body (builtin usage) should NOT
    // include the "reserved:" config field in the grammar block.
    let result = references_at(&mut service, test_uri(), Position::new(5, 17), false).await;

    let locs = result.expect("should find references");
    // Should only have the one usage in the rule body, not the config field.
    assert_eq!(
        locs,
        vec![Location {
            uri: test_uri(),
            range: Range::new(Position::new(5, 15), Position::new(5, 23)),
        }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_on_config_field_returns_none() {
    let grammar = r#"
grammar {
    language: "test",
    reserved: { default: ["if"] },
}
rule program { reserved("default", identifier) }
rule identifier { regexp(r"[a-z]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // References on "reserved:" config field should return None,
    // not show builtin reserved() usages.
    let result = references_at(&mut service, test_uri(), Position::new(3, 6), true).await;

    assert_eq!(result, None);
}

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_on_config_field_returns_none() {
    let grammar = r#"
grammar {
    language: "test",
    reserved: { default: ["if"] },
}
rule program { reserved("default", identifier) }
rule identifier { regexp(r"[a-z]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Highlight on "reserved:" config field should return None.
    let result = highlights_at(&mut service, test_uri(), Position::new(3, 6)).await;

    assert_eq!(result, None);
}

#[tokio::test(flavor = "current_thread")]
async fn hover_override_rule() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Find "override rule _statement" and hover on "_statement"
    let name_offset = fix.derived_text.find("_statement").unwrap();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, name_offset as u32);

    let result = hover_at(&mut service, derived_uri, pos).await;

    assert_eq!(result, make_hover("```\noverride rule _statement\n```"));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_base_rule_shows_base_definition() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Hover on "_statement" in "base::_statement" should show the base
    // grammar's rule, not the local override.
    let base_ref_offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, base_ref_offset as u32);

    let result = hover_at(&mut service, derived_uri, pos).await;

    assert_eq!(result, make_hover("```\nrule _statement\n```"));
}

#[tokio::test(flavor = "current_thread")]
async fn references_base_rule_excludes_override() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // References on "_statement" in "base::_statement" should show:
    // 1. The base grammar's definition of _statement
    // 2. Usages of _statement in the base grammar (e.g. in rule program)
    // 3. The base::_statement reference in the derived grammar
    // But NOT the override rule _statement definition.
    let base_ref_offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, base_ref_offset as u32);

    let result = references_at(&mut service, derived_uri.clone(), pos, true).await;

    let locations = result.expect("should find references");

    // Should have locations in the base file (definition + usage in program)
    // and the derived file (the base::_statement reference).
    assert_eq!(
        locations,
        vec![
            Location {
                uri: base_uri.clone(),
                range: Range {
                    start: Position {
                        line: 3,
                        character: 5,
                    },
                    end: Position {
                        line: 3,
                        character: 15,
                    }
                }
            },
            Location {
                uri: base_uri,
                range: Range {
                    start: Position {
                        line: 2,
                        character: 22,
                    },
                    end: Position {
                        line: 2,
                        character: 32,
                    }
                }
            },
            Location {
                uri: derived_uri,
                range: Range {
                    start: Position {
                        line: 7,
                        character: 40,
                    },
                    end: Position {
                        line: 7,
                        character: 50,
                    }
                }
            }
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_base_rule_excludes_declaration_when_disabled() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // With `include_declaration: false`, references on `base::_statement`
    // should NOT include the base grammar's definition - only usages.
    let base_ref_offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, base_ref_offset as u32);

    let result = references_at(&mut service, derived_uri.clone(), pos, false).await;

    assert_eq!(
        result.expect("should find references"),
        vec![
            // Usage of _statement in base's `rule program`.
            Location {
                uri: base_uri,
                range: Range::new(Position::new(2, 22), Position::new(2, 32)),
            },
            // The base::_statement reference in derived.
            Location {
                uri: derived_uri,
                range: Range::new(Position::new(7, 40), Position::new(7, 50)),
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_base_rule_excludes_override() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Highlight on "_statement" in "base::_statement" should only highlight
    // other base::_statement references, not the override rule definition.
    let base_ref_offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, base_ref_offset as u32);

    let result = highlights_at(&mut service, derived_uri, pos).await;

    let highlights = result.expect("should find highlights");

    // Should only contain the base::_statement reference (READ), not
    // the override rule definition (which would be WRITE).
    assert_eq!(
        highlights.len(),
        1,
        "should highlight only the base::_statement reference"
    );
    assert_eq!(highlights[0].kind, Some(DocumentHighlightKind::READ));
}

/// Create a fixture where the base grammar has a function, and the derived
/// grammar calls it via `base::wrap(...)`.
struct InheritFnFixture {
    _dir: tempfile::TempDir,
    base_path: std::path::PathBuf,
    derived_path: std::path::PathBuf,
    derived_text: String,
}

fn create_inherit_fn_fixture() -> InheritFnFixture {
    let dir = tempfile::tempdir().unwrap();

    let base_text = r#"
grammar { language: "base_lang" }
macro wrap(x: rule_t) rule_t { seq("(", x, ")") }
rule program { wrap("x") }
"#;
    let base_path = dir.path().join("base.tsg");
    std::fs::write(&base_path, base_text).unwrap();

    let derived_text = format!(
        r#"
let base = inherit("{}")
grammar {{
    language: "derived_lang",
    inherits: base,
}}
override rule program {{ base::wrap("y") }}
"#,
        base_path.display()
    );
    let derived_path = dir.path().join("derived.tsg");
    std::fs::write(&derived_path, &derived_text).unwrap();

    InheritFnFixture {
        _dir: dir,
        base_path,
        derived_path,
        derived_text,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_base_qualified_call() {
    let fix = create_inherit_fn_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor on "wrap" in `base::wrap("y")`.
    let wrap_offset = fix.derived_text.find("base::wrap").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, wrap_offset as u32);

    let result = goto_def_at(&mut service, derived_uri, pos).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(
        loc.uri, base_uri,
        "base::wrap() should jump to base grammar"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_base_qualified_call() {
    let fix = create_inherit_fn_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor on "wrap" in `base::wrap("y")`.
    let wrap_offset = fix.derived_text.find("base::wrap").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, wrap_offset as u32);

    let result = references_at(&mut service, derived_uri.clone(), pos, true).await;
    let locations = result.expect("should find references");

    // Should include: base definition, base usage in `rule program`, derived `base::wrap` ref.
    let base_locations: Vec<_> = locations.iter().filter(|l| l.uri == base_uri).collect();
    let derived_locations: Vec<_> = locations.iter().filter(|l| l.uri == derived_uri).collect();

    assert_eq!(
        base_locations.len(),
        2,
        "base: definition + usage in program"
    );
    assert_eq!(derived_locations.len(), 1, "derived: base::wrap reference");
}

#[tokio::test(flavor = "current_thread")]
async fn highlight_base_qualified_call() {
    let fix = create_inherit_fn_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor on "wrap" in `base::wrap("y")`.
    let wrap_offset = fix.derived_text.find("base::wrap").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, wrap_offset as u32);

    let result = highlights_at(&mut service, derived_uri, pos).await;
    let highlights = result.expect("should find highlights");

    assert_eq!(highlights.len(), 1, "should highlight the base::wrap call");
    assert_eq!(highlights[0].kind, Some(DocumentHighlightKind::READ));
}

#[tokio::test(flavor = "current_thread")]
async fn rename_base_qualified_call() {
    let fix = create_inherit_fn_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor on "wrap" in `base::wrap("y")`.
    let wrap_offset = fix.derived_text.find("base::wrap").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, wrap_offset as u32);

    let edit = rename_at(&mut service, derived_uri.clone(), pos, "enclose")
        .await
        .unwrap();
    let changes = edit.changes.unwrap();

    // Base file: definition of `wrap` + usage in `rule program { wrap("x") }`.
    let base_edits = &changes[&base_uri];
    assert_eq!(base_edits.len(), 2);
    assert!(base_edits.iter().all(|e| e.new_text == "enclose"));

    // Derived file: the `base::wrap` reference.
    let derived_edits = &changes[&derived_uri];
    assert_eq!(derived_edits.len(), 1);
    assert_eq!(derived_edits[0].new_text, "enclose");
}

#[tokio::test(flavor = "current_thread")]
async fn references_override_rule_excludes_base() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // References on "_statement" at the override rule definition should NOT
    // include locations from the base grammar or the base::_statement reference.
    let override_offset =
        fix.derived_text.find("override rule _statement").unwrap() + "override rule ".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, override_offset as u32);

    let result = references_at(&mut service, derived_uri.clone(), pos, true).await;

    let locations = result.expect("should find references");
    assert_eq!(
        locations,
        vec![Location {
            uri: derived_uri,
            range: Range {
                start: Position {
                    line: 7,
                    character: 14
                },
                end: Position {
                    line: 7,
                    character: 24
                }
            }
        }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_override_rule_excludes_base_ref() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Highlight on "_statement" at the override rule definition should NOT
    // highlight the base::_statement reference.
    let override_offset =
        fix.derived_text.find("override rule _statement").unwrap() + "override rule ".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, override_offset as u32);

    let result = highlights_at(&mut service, derived_uri, pos).await;

    let highlights = result.expect("should find highlights");
    // Should only have the override definition (WRITE), not the base::_statement ref.
    //left: [DocumentHighlight { range: Range { start: Position { line: 7, character: 14 }, end: Position { line: 7, character: 24 } }, kind: Some(Write) }
    assert_eq!(
        highlights,
        vec![DocumentHighlight {
            range: Range {
                start: Position {
                    line: 7,
                    character: 14,
                },
                end: Position {
                    line: 7,
                    character: 24,
                }
            },
            kind: Some(DocumentHighlightKind::WRITE)
        }]
    );
}

// ---------------------------------------------------------------------------
// Additional go-to-definition coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn goto_def_parameter_name() {
    let grammar = r#"
grammar { language: "test" }
macro helper(item: rule_t) rule_t { seq(item, item) }
rule program { helper("x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // "item" usage inside fn body - line 2: `seq(item, item)`
    // First "item" in seq starts at col 40
    // Cursor on first "item" inside fn body
    let result = goto_def_at(&mut service, test_uri(), Position::new(2, 41)).await;

    // Should go to the parameter definition in the macro signature.
    assert_eq!(
        result,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: test_uri(),
            range: Range::new(Position::new(2, 13), Position::new(2, 17)),
        }))
    );
}

// ---------------------------------------------------------------------------
// Additional references coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn references_function_name() {
    let grammar = r#"
grammar { language: "test" }
macro helper(x: rule_t) rule_t { x }
rule program { helper(helper("x")) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "helper" definition (line 2, col 6 = `h` of `helper`).
    let result = references_at(&mut service, test_uri(), Position::new(2, 7), true).await;

    let mut locs = result.unwrap();
    locs.sort_by(|a, b| {
        a.range
            .start
            .line
            .cmp(&b.range.start.line)
            .then(a.range.start.character.cmp(&b.range.start.character))
    });
    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    // Definition (line 2) + 2 call sites (line 3)
    assert_eq!(locs, vec![loc(2, 6, 12), loc(3, 15, 21), loc(3, 22, 28),]);
}

// ---------------------------------------------------------------------------
// Additional semantic tokens coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn semantic_tokens_variable_and_class_refs() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1 }
rule _expression { choice(identifier, number) }
rule identifier { regexp(r"[a-z]+") }
rule number { regexp(r"[0-9]+") }
rule program { prec(PREC.ADD, _expression) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let result: Option<SemanticTokensResult> = lsp_request::<SemanticTokensFullRequest>(
        &mut service,
        SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
        panic!("expected full tokens");
    };

    // Token types: 0=function, 1=variable, 2=type, 3=class
    let token_types: Vec<u32> = tokens.data.iter().map(|t| t.token_type).collect();

    // Should have variable tokens (PREC def + PREC ref)
    assert!(token_types.contains(&1), "expected variable tokens");
    // Should have class tokens (rule name defs + rule refs like _expression, identifier, number)
    assert!(token_types.contains(&3), "expected class tokens");
}

// ---------------------------------------------------------------------------
// Additional diagnostics coverage
// ---------------------------------------------------------------------------

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn diagnostics_lower_error() {
    // Calling a rule as a function is caught at typecheck.
    let bad = r#"
        grammar { language: "test" }
        rule foo { "x" }
        rule program { foo("y") }
    "#;
    let service = init(&[(test_uri(), bad)]).await;

    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(
        doc.dsl_diagnostics,
        vec![Diagnostic {
            range: Range::new(Position::new(3, 23), Position::new(3, 31)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("ts_grammar_ls".into()),
            message: "undefined macro 'foo'".into(),
            ..Default::default()
        }]
    );
}

// ---------------------------------------------------------------------------
// Additional document symbol coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_override_rule() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: derived_uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let DocumentSymbolResponse::Nested(symbols) = result.unwrap() else {
        panic!("expected nested symbols");
    };

    let names_kinds: Vec<(&str, SymbolKind)> =
        symbols.iter().map(|s| (s.name.as_str(), s.kind)).collect();
    assert_eq!(
        names_kinds,
        [
            ("base", SymbolKind::MODULE),
            ("_statement", SymbolKind::CLASS),
            ("new_rule", SymbolKind::CLASS),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_function_has_detail() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let DocumentSymbolResponse::Nested(symbols) = result.unwrap() else {
        panic!("expected nested symbols");
    };

    let fn_sym = symbols.iter().find(|s| s.name == "commaSep1").unwrap();
    assert_eq!(
        fn_sym.detail,
        Some("macro commaSep1(item: rule_t) rule_t".into())
    );
}

// ---------------------------------------------------------------------------
// Document highlight tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_rule() {
    // _expression defined on line 3, referenced on lines 2 and 4
    let grammar = r#"
grammar { language: "test" }
rule program { _expression }
rule _expression { choice(identifier, number) }
rule identifier { prec(1, _expression) }
rule number { regexp(r"[0-9]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "_expression" definition (line 3, col 5)
    let result = highlights_at(&mut service, test_uri(), Position::new(3, 6)).await;

    let mut highlights = result.unwrap();
    highlights.sort_by_key(|h| (h.range.start.line, h.range.start.character));

    assert_eq!(
        highlights,
        vec![
            DocumentHighlight {
                range: Range::new(Position::new(2, 15), Position::new(2, 26)),
                kind: Some(DocumentHighlightKind::READ),
            },
            DocumentHighlight {
                range: Range::new(Position::new(3, 5), Position::new(3, 16)),
                kind: Some(DocumentHighlightKind::WRITE),
            },
            DocumentHighlight {
                range: Range::new(Position::new(4, 26), Position::new(4, 37)),
                kind: Some(DocumentHighlightKind::READ),
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_rule_inside_grammar_block() {
    // `identifier` is used in grammar fields (word, extras, inline, conflicts,
    // precedences) as well as in rule bodies.
    let grammar = r#"
grammar {
    language: "test",
    word: identifier,
    extras: [identifier, whitespace],
    inline: [identifier],
    conflicts: [[identifier, expression]],
    precedences: [[identifier, expression]],
}
rule program { seq(identifier, "x") }
rule expression { identifier }
rule identifier { regexp(r"[a-z]+") }
rule whitespace { regexp(r"\s+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on `identifier` in the definition (line 11).
    let result = highlights_at(&mut service, test_uri(), Position::new(11, 6)).await;

    let mut highlights = result.unwrap();
    highlights.sort_by_key(|h| (h.range.start.line, h.range.start.character));

    let read = |line, start, end| DocumentHighlight {
        range: Range::new(Position::new(line, start), Position::new(line, end)),
        kind: Some(DocumentHighlightKind::READ),
    };
    let write = |line, start, end| DocumentHighlight {
        range: Range::new(Position::new(line, start), Position::new(line, end)),
        kind: Some(DocumentHighlightKind::WRITE),
    };

    assert_eq!(
        highlights,
        vec![
            read(3, 10, 20),  // word: identifier,
            read(4, 13, 23),  // extras: [identifier, ...
            read(5, 13, 23),  // inline: [identifier],
            read(6, 17, 27),  // conflicts: [[identifier, ...
            read(7, 19, 29),  // precedences: [[identifier, ...
            read(9, 19, 29),  // rule program { seq(identifier, ...
            read(10, 18, 28), // rule expression { identifier }
            write(11, 5, 15), // rule identifier { ... } (definition)
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_object_field() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1, MUL: 2 }
rule a { prec(PREC.ADD, "x") }
rule b { prec(PREC.ADD, "y") }
rule program { choice(a, b) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "ADD" in PREC.ADD on line 3
    let result = highlights_at(&mut service, test_uri(), Position::new(3, 19)).await;

    let mut highlights = result.unwrap();
    highlights.sort_by_key(|h| (h.range.start.line, h.range.start.character));

    // Should highlight definition (line 2) and both uses (lines 3 and 4)
    assert_eq!(
        highlights,
        vec![
            DocumentHighlight {
                range: Range::new(Position::new(2, 13), Position::new(2, 16)),
                kind: Some(DocumentHighlightKind::WRITE),
            },
            DocumentHighlight {
                range: Range::new(Position::new(3, 19), Position::new(3, 22)),
                kind: Some(DocumentHighlightKind::READ),
            },
            DocumentHighlight {
                range: Range::new(Position::new(4, 19), Position::new(4, 22)),
                kind: Some(DocumentHighlightKind::READ),
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_builtin() {
    let grammar = r#"
grammar { language: "test" }
rule program { seq("a", seq("b", "c")) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on first "seq" (line 2, col 15)
    let result = highlights_at(&mut service, test_uri(), Position::new(2, 15)).await;

    let highlights = result.unwrap();
    // Should highlight both uses of seq on line 2
    assert_eq!(highlights.len(), 2);
    assert!(
        highlights
            .iter()
            .all(|h| h.kind == Some(DocumentHighlightKind::READ))
    );
}

// ---------------------------------------------------------------------------
// Builtin references tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn references_builtin() {
    let grammar = r#"
grammar { language: "test" }
rule a { repeat("x") }
rule b { repeat(repeat("y")) }
rule program { choice(a, b) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "repeat" (line 2, col 9)
    let result = references_at(&mut service, test_uri(), Position::new(2, 10), false).await;

    let mut locs = result.unwrap();
    locs.sort_by(|a, b| {
        a.range
            .start
            .line
            .cmp(&b.range.start.line)
            .then(a.range.start.character.cmp(&b.range.start.character))
    });

    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    // 3 uses of repeat: line 2 (1), line 3 (2)
    assert_eq!(locs, vec![loc(2, 9, 15), loc(3, 9, 15), loc(3, 16, 22),]);
}

// ---------------------------------------------------------------------------
// Object field references test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn references_object_field() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1, MUL: 2 }
rule a { prec(PREC.ADD, "x") }
rule b { prec(PREC.ADD, "y") }
rule program { choice(a, b) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "ADD" in PREC.ADD on line 3
    let result = references_at(&mut service, test_uri(), Position::new(3, 19), false).await;

    let mut locs = result.unwrap();
    locs.sort_by_key(|l| (l.range.start.line, l.range.start.character));

    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    assert_eq!(locs, vec![loc(3, 19, 22), loc(4, 19, 22),]);
}

// ---------------------------------------------------------------------------
// Parameter scoping tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn references_parameter_scoped() {
    // Two functions with same parameter name "item" - references should
    // only return matches within the same function.
    let grammar = r#"
grammar { language: "test" }
macro f1(item: rule_t) rule_t { seq(item, item) }
macro f2(item: rule_t) rule_t { repeat(item) }
rule program { f1(f2("x")) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on first "item" usage inside f1 body (line 2, col 36)
    let result = references_at(&mut service, test_uri(), Position::new(2, 36), true).await;

    let mut locs = result.unwrap();
    locs.sort_by(|a, b| a.range.start.character.cmp(&b.range.start.character));

    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    // Should only find references within f1: param def + 2 usages, NOT f2's "item".
    assert_eq!(locs, vec![loc(2, 9, 13), loc(2, 36, 40), loc(2, 42, 46)]);
}

#[tokio::test(flavor = "current_thread")]
async fn highlight_parameter_scoped() {
    let grammar = r#"
grammar { language: "test" }
macro f1(item: rule_t) rule_t { seq(item, item) }
macro f2(item: rule_t) rule_t { repeat(item) }
rule program { f1(f2("x")) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on first "item" usage inside f1 body (line 2, col 36)
    let result = highlights_at(&mut service, test_uri(), Position::new(2, 36)).await;

    let mut highlights = result.unwrap();
    highlights.sort_by_key(|h| h.range.start.character);
    // Should only highlight within f1, not f2.
    assert_eq!(
        highlights,
        vec![
            DocumentHighlight {
                range: Range::new(Position::new(2, 9), Position::new(2, 13)),
                kind: Some(DocumentHighlightKind::WRITE),
            },
            DocumentHighlight {
                range: Range::new(Position::new(2, 36), Position::new(2, 40)),
                kind: Some(DocumentHighlightKind::READ),
            },
            DocumentHighlight {
                range: Range::new(Position::new(2, 42), Position::new(2, 46)),
                kind: Some(DocumentHighlightKind::READ),
            },
        ]
    );
}

// ---------------------------------------------------------------------------
// For-loop binding tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn goto_def_for_loop_binding() {
    let grammar = r#"
grammar { language: "test" }
rule program {
    choice(for (op: str_t, p: int_t) in [("&&", 2), ("||", 1)] {
        prec_left(p, seq("x", op, "x"))
    })
}
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "op" usage in seq on line 4, col 30
    let result = goto_def_at(&mut service, test_uri(), Position::new(4, 30)).await;

    // Should point to the "op" binding in the for-loop on line 3, col 16.
    assert_eq!(
        result,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: test_uri(),
            range: Range::new(Position::new(3, 16), Position::new(3, 18)),
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_for_loop_binding() {
    let grammar = r#"
grammar { language: "test" }
rule program {
    choice(for (op: str_t, p: int_t) in [("&&", 2), ("||", 1)] {
        prec_left(p, seq("x", op, "x"))
    })
}
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "p" usage in prec_left (line 4)
    let result = references_at(&mut service, test_uri(), Position::new(4, 18), true).await;

    let mut locs = result.unwrap();
    locs.sort_by_key(|l| (l.range.start.line, l.range.start.character));
    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    // Binding def (line 3, "p" at col 27) + usage (line 4, "p" at col 18)
    assert_eq!(locs, vec![loc(3, 27, 28), loc(4, 18, 19)]);
}

#[tokio::test(flavor = "current_thread")]
async fn references_parameter_nested_usage() {
    // Regression: item used inside nested seq/repeat should still be found.
    let grammar = r#"
grammar { language: "test" }
macro comma_sep1(item: rule_t) rule_t { seq(item, repeat(seq(",", item))) }
rule program { comma_sep1("x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "item" parameter name in signature (line 2, col 17 = `i` of `item`).
    let result = references_at(&mut service, test_uri(), Position::new(2, 17), true).await;

    let mut locs = result.unwrap();
    locs.sort_by_key(|l| l.range.start.character);
    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    // Parameter def (col 17) + 2 usages (col 44, col 66)
    assert_eq!(locs, vec![loc(2, 17, 21), loc(2, 44, 48), loc(2, 66, 70)]);
}

// ---------------------------------------------------------------------------
// Lifecycle handlers (did_save, did_close)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn did_close_removes_document_from_map() {
    let grammar = "grammar { language: \"test\" }\nrule program { \"x\" }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Before close: hover finds `program` as a rule.
    let before = hover_at(&mut service, test_uri(), Position::new(1, 6)).await;
    assert_eq!(before, make_hover("```\nrule program\n```"));

    lsp_notify::<DidCloseTextDocument>(
        &mut service,
        DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
        },
    )
    .await;

    // After close: hover returns None because the doc is gone from the map.
    let after = hover_at(&mut service, test_uri(), Position::new(1, 6)).await;
    assert_eq!(after, None);
}

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn did_save_refreshes_cached_diagnostics() {
    // did_save re-runs the diagnostic pipeline. We can observe this by
    // stuffing the document's cached diagnostics with a sentinel, then
    // triggering did_save and checking that the pipeline overwrote them.
    let grammar = "grammar { language: \"test\" }\nrule program { \"x\" }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    let sentinel = Diagnostic {
        range: Range::new(Position::new(99, 0), Position::new(99, 0)),
        message: "stale".into(),
        ..Default::default()
    };
    service
        .inner()
        .document_map
        .get_mut(&test_uri())
        .unwrap()
        .dsl_diagnostics = vec![sentinel.clone()];

    lsp_notify::<DidSaveTextDocument>(
        &mut service,
        DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            text: None,
        },
    )
    .await;

    // Pipeline ran: cached diagnostics reflect the clean grammar, not the sentinel.
    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(doc.dsl_diagnostics, vec![]);
}

#[tokio::test(flavor = "current_thread")]
async fn did_save_on_unknown_uri_is_noop() {
    // Saving a URI that was never opened should early-return without panicking.
    let mut service = init(&[]).await;

    let unknown = Url::parse("file:///tmp/never-opened.tsg").unwrap();
    lsp_notify::<DidSaveTextDocument>(
        &mut service,
        DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: unknown.clone(),
            },
            text: None,
        },
    )
    .await;

    // Subsequent hover on the unknown URI still returns None (no doc).
    let result = hover_at(&mut service, unknown, Position::new(0, 0)).await;
    assert_eq!(result, None);
}

#[tokio::test(flavor = "current_thread")]
async fn did_close_on_unknown_uri_is_noop() {
    let mut service = init(&[]).await;

    let unknown = Url::parse("file:///tmp/never-opened.tsg").unwrap();
    lsp_notify::<DidCloseTextDocument>(
        &mut service,
        DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: unknown },
        },
    )
    .await;
    // If this returns without panic, the test passes.
}

// ---------------------------------------------------------------------------
// Formatting tests
// ---------------------------------------------------------------------------

async fn format_request(service: &mut LspService<Backend>, uri: Url) -> Option<Vec<TextEdit>> {
    lsp_request::<Formatting>(
        service,
        DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            options: FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                ..Default::default()
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn formatting_reformats_messy_source() {
    // Messy input with no padding around braces/colons; expect a single edit
    // replacing the whole document with the formatter's canonical output.
    let messy = "grammar{language:\"test\"}\n\nrule program{\"x\"}\n";
    let mut service = init(&[(test_uri(), messy)]).await;

    let result = format_request(&mut service, test_uri()).await;

    let expected_text = "grammar {\n    language: \"test\",\n}\n\nrule program { \"x\" }\n";
    assert_eq!(
        result,
        Some(vec![TextEdit {
            range: Range::new(Position::new(0, 0), Position::new(3, 0)),
            new_text: expected_text.into(),
        }])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn formatting_already_clean_returns_empty_edits() {
    // If the document is already formatted, return an empty edit list rather
    // than a no-op replacement. Saves the client from applying a redundant diff.
    let clean = "grammar {\n    language: \"test\",\n}\n\nrule program { \"x\" }\n";
    let mut service = init(&[(test_uri(), clean)]).await;

    let result = format_request(&mut service, test_uri()).await;

    assert_eq!(result, Some(Vec::new()));
}

#[tokio::test(flavor = "current_thread")]
async fn formatting_unparseable_returns_none() {
    // Incomplete source - parser fails, formatter returns None, handler
    // returns None. The client will leave the buffer unchanged.
    let broken = "grammar { language: \"test\" } rule program {";
    let mut service = init(&[(test_uri(), broken)]).await;

    let result = format_request(&mut service, test_uri()).await;

    assert_eq!(result, None);
}

#[tokio::test(flavor = "current_thread")]
async fn formatting_unknown_uri_returns_none() {
    // No document registered for this URI - handler must not panic.
    let mut service = init(&[]).await;

    let result = format_request(
        &mut service,
        Url::parse("file:///tmp/never-opened.tsg").unwrap(),
    )
    .await;

    assert_eq!(result, None);
}

// ---------------------------------------------------------------------------
// Code action tests
// ---------------------------------------------------------------------------

async fn code_actions_at(
    service: &mut LspService<Backend>,
    uri: Url,
    range: Range,
) -> Option<CodeActionResponse> {
    lsp_request::<CodeActionRequest>(
        service,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri },
            range,
            context: CodeActionContext {
                diagnostics: Vec::new(),
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
}

/// The "Open grammar REPL" action that the code-action handler offers
/// on every `.tsg` buffer with at least one rule. Tests include this in
/// their expected output rather than filtering it from results - that
/// way any change to the action's shape surfaces as a test diff.
fn open_grammar_repl_action(uri: &Url) -> CodeActionOrCommand {
    CodeActionOrCommand::CodeAction(CodeAction {
        title: "Open grammar REPL".into(),
        kind: Some(CodeActionKind::EMPTY),
        command: Some(tower_lsp::lsp_types::Command {
            title: "Open grammar REPL".into(),
            command: ts_grammar_ls::handlers::repl::OPEN_REPL_COMMAND.into(),
            arguments: Some(vec![serde_json::json!({ "uri": uri.to_string() })]),
        }),
        edit: None,
        diagnostics: None,
        is_preferred: None,
        disabled: None,
        data: None,
    })
}

/// Build the expected `CodeActionResponse` for a "Convert to raw string" action
/// with a single text edit on `test_uri()`.
fn make_raw_string_action(edit_range: Range, new_text: &str) -> CodeActionResponse {
    vec![CodeActionOrCommand::CodeAction(CodeAction {
        title: "Convert to raw string".into(),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        edit: Some(WorkspaceEdit {
            changes: Some(std::collections::HashMap::from([(
                test_uri(),
                vec![TextEdit {
                    range: edit_range,
                    new_text: new_text.into(),
                }],
            )])),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: None,
        command: None,
        is_preferred: None,
        disabled: None,
        data: None,
    })]
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_not_offered_on_plain_string() {
    // "hello" has no escapes - converting to r"hello" is just noise.
    let grammar = r#"
grammar { language: "test" }
rule program { "hello" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let range = Range::new(Position::new(2, 16), Position::new(2, 16));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    // The always-available "Open grammar REPL" action is offered; the
    // raw-string refactor is not.
    assert_eq!(result, Some(vec![open_grammar_repl_action(&test_uri())]));
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_convert_escaped_backslash_to_raw() {
    // String with escaped backslash (regex pattern) - conversion is useful.
    // Source: "[a-z]+\\s*"  (\\  is an escape for a literal \)
    // Raw:    r"[a-z]+\s*"  (no hashes needed, no " in content)
    let grammar = "grammar { language: \"test\" }\nrule program { regexp(\"[a-z]+\\\\s*\") }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    let range = Range::new(Position::new(1, 23), Position::new(1, 23));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    // String literal spans line 1, col 22..34.
    let mut expected = make_raw_string_action(
        Range::new(Position::new(1, 22), Position::new(1, 34)),
        "r\"[a-z]+\\s*\"",
    );
    expected.push(open_grammar_repl_action(&test_uri()));
    assert_eq!(result, Some(expected));
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_convert_string_with_quotes_uses_hashes() {
    // Source: "he said \"hi\""  (escaped quotes inside)
    // Raw:    r#"he said "hi""#  (1 hash needed for embedded ")
    let grammar = "grammar { language: \"test\" }\nrule program { \"he said \\\"hi\\\"\" }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    let range = Range::new(Position::new(1, 16), Position::new(1, 16));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    // String literal spans line 1, col 15..31.
    let mut expected = make_raw_string_action(
        Range::new(Position::new(1, 15), Position::new(1, 31)),
        "r#\"he said \"hi\"\"#",
    );
    expected.push(open_grammar_repl_action(&test_uri()));
    assert_eq!(result, Some(expected));
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_not_offered_on_semantic_escapes() {
    // Strings with \n, \t, \r, \0 can't be faithfully converted to raw
    // (they'd become literal two-char sequences instead of the special char).
    let grammar = r#"
grammar { language: "test" }
rule program { "hey\nthere" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let range = Range::new(Position::new(2, 16), Position::new(2, 16));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    assert_eq!(result, Some(vec![open_grammar_repl_action(&test_uri())]));
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_not_offered_on_raw_string() {
    let grammar = "grammar { language: \"test\" }\nrule program { r\"hello\" }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    let range = Range::new(Position::new(1, 16), Position::new(1, 16));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    assert_eq!(result, Some(vec![open_grammar_repl_action(&test_uri())]));
}

/// Make a real on-disk grammar URI using `tempfile`. Each test gets a
/// unique path so concurrent tests don't race on the same hashed REPL
/// buffer location.
fn temp_grammar_uri(grammar: &str) -> (tempfile::NamedTempFile, Url) {
    let mut file = tempfile::Builder::new()
        .suffix(".tsg")
        .tempfile()
        .expect("tempfile");
    use std::io::Write as _;
    file.write_all(grammar.as_bytes()).unwrap();
    let uri = Url::from_file_path(file.path()).unwrap();
    (file, uri)
}

#[tokio::test(flavor = "current_thread")]
async fn open_repl_creates_input_buffer_with_default_rule() {
    let grammar =
        "grammar { language: \"test\" }\nrule program { \"x\" }\nrule expression { \"y\" }\n";
    let (_keep_alive, uri) = temp_grammar_uri(grammar);
    let mut service = init(&[(uri.clone(), grammar)]).await;

    // No position - defaults to first rule (`program`).
    let result = lsp_request::<ExecuteCommand>(
        &mut service,
        ExecuteCommandParams {
            command: "tsg.openRepl".into(),
            arguments: vec![serde_json::json!({ "uri": uri.to_string() })],
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("command returned a value");

    let obj = result.as_object().expect("object response");
    assert_eq!(obj.get("rule").and_then(|v| v.as_str()), Some("program"));
    let repl_uri =
        Url::parse(obj.get("uri").and_then(|v| v.as_str()).expect("uri field")).expect("valid url");
    let input_uri =
        ts_grammar_ls::repl::ReplInputUri::try_from_uri(&repl_uri).expect("REPL input URI");
    let path = input_uri.input_path();
    // Buffer starts empty: rule binding lives in the sibling metadata
    // file, not in the buffer text. The CodeLens shows the rule.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    let meta_path = input_uri.meta_path();
    let meta: ts_grammar_ls::repl::ReplMeta =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(meta.current_rule, "program");
    assert_eq!(meta.grammar_uri, uri);
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&meta_path).ok();
}

/// A derived grammar whose own first declaration is a hidden `override rule`
/// must fall back to the *base's* start rule, not that hidden one.
///
/// `InputGrammar::normalize` treats `variables[0]` as the implicit start, and
/// variables are ordered base-first, so for an inheriting grammar the start
/// lives upstream. Picking a `_`-prefixed rule instead makes codegen fail with
/// `HiddenStartRule`, which the REPL could only report as a `warn!`.
/// tree-sitter-cpp inheriting tree-sitter-c is the real-world shape: cpp never
/// declares `translation_unit`, and its first declaration is
/// `override rule _top_level_item`.
#[tokio::test(flavor = "current_thread")]
async fn open_repl_skips_hidden_rule_in_inheriting_grammar() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("base.tsg"),
        "grammar { language: \"base\" }\n\
         rule translation_unit { repeat(_item) }\n\
         rule _item { \"x\" }\n",
    )
    .unwrap();
    let derived = "let b = inherit(\"base.tsg\")\n\
                   grammar { language: \"derived\", inherits: b }\n\
                   override rule _item { \"y\" }\n\
                   rule extra_thing { \"z\" }\n";
    let derived_path = dir.path().join("derived.tsg");
    std::fs::write(&derived_path, derived).unwrap();
    let uri = Url::from_file_path(&derived_path).unwrap();
    let mut service = init(&[(uri.clone(), derived)]).await;

    let result = lsp_request::<ExecuteCommand>(
        &mut service,
        ExecuteCommandParams {
            command: "tsg.openRepl".into(),
            arguments: vec![serde_json::json!({ "uri": uri.to_string() })],
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("command returned a value");

    let obj = result.as_object().expect("object response");
    // Not `_item` (hidden, and first in source order), and not the derived
    // file's own `extra_thing` - the base's start rule.
    assert_eq!(
        obj.get("rule").and_then(|v| v.as_str()),
        Some("translation_unit"),
    );

    let repl_uri =
        Url::parse(obj.get("uri").and_then(|v| v.as_str()).expect("uri field")).expect("valid url");
    let input_uri =
        ts_grammar_ls::repl::ReplInputUri::try_from_uri(&repl_uri).expect("REPL input URI");
    std::fs::remove_file(input_uri.input_path()).ok();
    std::fs::remove_file(input_uri.meta_path()).ok();
}

#[tokio::test(flavor = "current_thread")]
async fn open_repl_picks_rule_from_cursor_position() {
    let grammar =
        "grammar { language: \"test\" }\nrule program { \"x\" }\nrule expression { \"y\" }\n";
    let (_keep_alive, uri) = temp_grammar_uri(grammar);
    let mut service = init(&[(uri.clone(), grammar)]).await;

    // Cursor at line 2, col 5 - inside `rule expression { ... }`.
    let result = lsp_request::<ExecuteCommand>(
        &mut service,
        ExecuteCommandParams {
            command: "tsg.openRepl".into(),
            arguments: vec![serde_json::json!({
                "uri": uri.to_string(),
                "position": { "line": 2, "character": 5 }
            })],
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("command returned a value");

    assert_eq!(
        result
            .as_object()
            .and_then(|o| o.get("rule"))
            .and_then(|v| v.as_str()),
        Some("expression")
    );
    let repl_uri = Url::parse(
        result
            .as_object()
            .and_then(|o| o.get("uri"))
            .and_then(|v| v.as_str())
            .unwrap(),
    )
    .unwrap();
    std::fs::remove_file(repl_uri.to_file_path().unwrap()).ok();
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_not_offered_on_identifier() {
    let grammar =
        "grammar { language: \"test\" }\nrule program { identifier }\nrule identifier { \"x\" }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    let range = Range::new(Position::new(1, 16), Position::new(1, 16));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    assert_eq!(result, Some(vec![open_grammar_repl_action(&test_uri())]));
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_inlines_rule_set_macro_call() {
    // A top-level rule-set macro generates a rule from the caller's args.
    // The code action expands the call into the resulting rule decl.
    let grammar = "\
grammar { language: \"test\" }

rules arith(prec_name: str_t, op: str_t) {
    rule @prec_name { seq(@prec_name, op, @prec_name) }
}

@arith(\"add\", \"+\")

rule program { \"x\" }
";
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on the `arith` identifier of the top-level call (line 6).
    let range = Range::new(Position::new(6, 2), Position::new(6, 2));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    let expected_edit_range = Range::new(Position::new(6, 0), Position::new(6, 18));
    assert_eq!(
        result,
        Some(vec![
            CodeActionOrCommand::CodeAction(CodeAction {
                title: "Inline macro call".into(),
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                edit: Some(WorkspaceEdit {
                    changes: Some(std::collections::HashMap::from([(
                        test_uri(),
                        vec![TextEdit {
                            range: expected_edit_range,
                            new_text: "rule add { seq(add, \"+\", add) }".into(),
                        }],
                    )])),
                    document_changes: None,
                    change_annotations: None,
                }),
                diagnostics: None,
                command: None,
                is_preferred: None,
                disabled: None,
                data: None,
            }),
            open_grammar_repl_action(&test_uri()),
        ])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_inlines_rule_set_macro_call_multi_rule() {
    // A rule-set macro that generates two rules from one call; both should
    // be emitted blank-line separated and replace the single call span.
    let grammar = "\
grammar { language: \"test\" }

rules pair(a: str_t, b: str_t) {
    rule @a { \"x\" }
    rule @b { \"y\" }
}

@pair(\"foo\", \"bar\")

rule program { \"x\" }
";
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on the `pair` identifier of the top-level call (line 7).
    let range = Range::new(Position::new(7, 2), Position::new(7, 2));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    let expected_edit_range = Range::new(Position::new(7, 0), Position::new(7, 19));
    assert_eq!(
        result,
        Some(vec![
            CodeActionOrCommand::CodeAction(CodeAction {
                title: "Inline macro call".into(),
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                edit: Some(WorkspaceEdit {
                    changes: Some(std::collections::HashMap::from([(
                        test_uri(),
                        vec![TextEdit {
                            range: expected_edit_range,
                            new_text: "rule foo { \"x\" }\n\nrule bar { \"y\" }".into(),
                        }],
                    )])),
                    document_changes: None,
                    change_annotations: None,
                }),
                diagnostics: None,
                command: None,
                is_preferred: None,
                disabled: None,
                data: None,
            }),
            open_grammar_repl_action(&test_uri()),
        ])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_inlines_expression_macro_call() {
    // A local expression-flavor macro called inside a rule body. The cursor
    // inside the call should produce an "Inline macro call" action that
    // replaces the call span with the body, args substituted.
    let grammar = "\
grammar { language: \"test\" }

macro greet(who: str_t) rule_t {
    seq(\"hello\", who)
}

rule program { greet(\"world\") }
";
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on the `greet` identifier inside the call on line 6.
    let range = Range::new(Position::new(6, 17), Position::new(6, 17));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    // `greet("world")` spans line 6, col 15..29.
    let expected_edit_range = Range::new(Position::new(6, 15), Position::new(6, 29));
    assert_eq!(
        result,
        Some(vec![
            CodeActionOrCommand::CodeAction(CodeAction {
                title: "Inline macro call".into(),
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                edit: Some(WorkspaceEdit {
                    changes: Some(std::collections::HashMap::from([(
                        test_uri(),
                        vec![TextEdit {
                            range: expected_edit_range,
                            new_text: "seq(\"hello\", \"world\")".into(),
                        }],
                    )])),
                    document_changes: None,
                    change_annotations: None,
                }),
                diagnostics: None,
                command: None,
                is_preferred: None,
                disabled: None,
                data: None,
            }),
            open_grammar_repl_action(&test_uri()),
        ])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_inlines_cross_module_expression_macro_call() {
    // `helpers::commaSep("x")` should inline to the helper macro body with
    // `item` substituted. Body / args span different module sources, so this
    // also exercises the formatter's per-frame source swap in Mode::Expansion.
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();
    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    // Locate `commaSep` in the qualified call inside grammar.tsg.
    let call_offset = fix.grammar_text.find("helpers::commaSep").unwrap();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(
        &rope,
        (call_offset + "helpers::comma".len()) as u32,
    );
    let range = Range::new(pos, pos);
    let result = code_actions_at(&mut service, grammar_uri.clone(), range).await;

    // The full `helpers::commaSep("x")` span gets replaced.
    let call_start = fix.grammar_text.find("helpers::commaSep").unwrap();
    let call_end = call_start + "helpers::commaSep(\"x\")".len();
    let expected_edit_range = Range::new(
        ts_grammar_ls::text::offset_to_position(&rope, call_start as u32),
        ts_grammar_ls::text::offset_to_position(&rope, call_end as u32),
    );
    assert_eq!(
        result,
        Some(vec![
            CodeActionOrCommand::CodeAction(CodeAction {
                title: "Inline macro call".into(),
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                edit: Some(WorkspaceEdit {
                    changes: Some(std::collections::HashMap::from([(
                        grammar_uri.clone(),
                        vec![TextEdit {
                            range: expected_edit_range,
                            new_text: "seq(\"x\", repeat(seq(\",\", \"x\")))".into(),
                        }],
                    )])),
                    document_changes: None,
                    change_annotations: None,
                }),
                diagnostics: None,
                command: None,
                is_preferred: None,
                disabled: None,
                data: None,
            }),
            open_grammar_repl_action(&grammar_uri),
        ])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_not_offered_on_non_macro_call() {
    // `seq` is a builtin combinator, not a macro - shouldn't be inlinable.
    let grammar = "\
grammar { language: \"test\" }
rule program { seq(\"a\", \"b\") }
";
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on `seq` (line 1, col 16).
    let range = Range::new(Position::new(1, 16), Position::new(1, 16));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    assert_eq!(result, Some(vec![open_grammar_repl_action(&test_uri())]));
}

// ---------------------------------------------------------------------------
// Import tests
// ---------------------------------------------------------------------------

struct ImportFixture {
    _dir: tempfile::TempDir,
    helper_path: std::path::PathBuf,
    grammar_path: std::path::PathBuf,
    grammar_text: String,
}

fn create_import_fixture() -> ImportFixture {
    let dir = tempfile::tempdir().unwrap();

    // Helper module:
    // line 1: macro commaSep(item: rule_t) rule_t {
    //         col 3 = "commaSep"
    // line 5: let PREC = { DEFAULT: 0, CALL: 1 }
    //         col 4 = "PREC"
    let helper_text = r#"
macro commaSep(item: rule_t) rule_t {
    seq(item, repeat(seq(",", item)))
}

let PREC = { DEFAULT: 0, CALL: 1 }
"#;
    let helper_path = dir.path().join("helpers.tsg");
    std::fs::write(&helper_path, helper_text).unwrap();

    let grammar_text = format!(
        r#"
let helpers = import("{}")
grammar {{ language: "test" }}
rule program {{ repeat(expression) }}
rule expression {{ helpers::commaSep("x") }}
"#,
        helper_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    ImportFixture {
        _dir: dir,
        helper_path,
        grammar_path,
        grammar_text,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_import_path() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();
    let helper_uri = Url::from_file_path(&fix.helper_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    let path_offset = fix
        .grammar_text
        .find(&fix.helper_path.display().to_string())
        .unwrap();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, path_offset as u32);

    let result = goto_def_at(&mut service, grammar_uri, pos).await;
    assert_eq!(
        result,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: helper_uri,
            range: Range::default(),
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_import_function() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();
    let helper_uri = Url::from_file_path(&fix.helper_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    // Cursor on "commaSep" in `helpers::commaSep("x")`
    let call_offset = fix.grammar_text.find("helpers::commaSep").unwrap() + "helpers::".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, call_offset as u32);

    let result = goto_def_at(&mut service, grammar_uri, pos).await;
    // "commaSep" is at line 1, col 6 in the helper file (after `macro `).
    assert_eq!(
        result,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: helper_uri,
            range: Range::new(Position::new(1, 6), Position::new(1, 14)),
        }))
    );
}

// Referencing an imported helper's plain *rule* via `mod::rule` (a bare
// qualified access, not a macro call) resolves in the core to `Node::ModuleRule`
// during the cross-module resolve pass, replacing the `QualifiedAccess` in place.
// Reference extraction must reconstruct the member reference from the preserved
// span or goto-definition silently breaks. Exercises the `is_import` (import)
// branch; `references_base_rule_excludes_override` covers the inherit branch.
#[tokio::test(flavor = "current_thread")]
async fn goto_def_imported_rule_via_qualified_access() {
    let dir = tempfile::tempdir().unwrap();

    // Helper exports a plain rule. "shared_token" is at line 1, col 5
    // (after `rule `), ending col 17.
    let helper_text = "\nrule shared_token { \"kw\" }\n";
    let helper_path = dir.path().join("helpers.tsg");
    std::fs::write(&helper_path, helper_text).unwrap();

    let grammar_text = format!(
        "\nlet helpers = import(\"{}\")\ngrammar {{ language: \"test\" }}\nrule program {{ helpers::shared_token }}\n",
        helper_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();
    let helper_uri = Url::from_file_path(&helper_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &grammar_text)]).await;

    // Cursor on "shared_token" in `helpers::shared_token`.
    let offset = grammar_text.find("helpers::shared_token").unwrap() + "helpers::".len();
    let rope = ropey::Rope::from_str(&grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let result = goto_def_at(&mut service, grammar_uri, pos).await;
    assert_eq!(
        result,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: helper_uri,
            range: Range::new(Position::new(1, 5), Position::new(1, 17)),
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_import_function() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    let call_offset = fix.grammar_text.find("helpers::commaSep").unwrap() + "helpers::".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, call_offset as u32);

    let result = hover_at(&mut service, grammar_uri, pos).await;
    assert_eq!(
        result,
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: "```\nmacro commaSep(item: rule_t) rule_t\n```".into(),
            }),
            range: None,
        })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn completion_import_members_after_double_colon() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    let colon_offset = fix.grammar_text.find("helpers::commaSep").unwrap() + "helpers::".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, colon_offset as u32);

    let mut items = completions_at(&mut service, grammar_uri, pos).await;
    items.sort_by(|a, b| a.label.cmp(&b.label));

    assert_eq!(
        items,
        vec![
            CompletionItem {
                label: "PREC".into(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some("let PREC (helpers)".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "commaSep".into(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some("macro commaSep(item: rule_t) rule_t (helpers)".into()),
                ..Default::default()
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbol_import_shows_as_module() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: grammar_uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let DocumentSymbolResponse::Nested(symbols) = result.unwrap() else {
        panic!("expected nested symbols");
    };

    let helpers_sym = symbols.iter().find(|s| s.name == "helpers").unwrap();
    assert_eq!(helpers_sym.kind, SymbolKind::MODULE);
}

// Verify that the inherit fixture's `base` let binding is still classified
// as a variable (not a module) in document symbols since inherit is not import.
#[tokio::test(flavor = "current_thread")]
async fn document_symbol_inherit_binding_is_module() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: derived_uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let DocumentSymbolResponse::Nested(symbols) = result.unwrap() else {
        panic!("expected nested symbols");
    };

    // `let base = inherit(...)` is a MODULE, same as import bindings.
    let base_sym = symbols.iter().find(|s| s.name == "base").unwrap();
    assert_eq!(base_sym.kind, SymbolKind::MODULE);
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_nested_import() {
    // Setup: grammar imports helpers, helpers imports utils.
    // grammar uses helpers::utils_fn which is actually utils::utils_fn.
    let dir = tempfile::tempdir().unwrap();

    // utils.tsg: defines utils_fn
    let utils_text = "
macro utils_fn(x: rule_t) rule_t { x }
";
    let utils_path = dir.path().join("utils.tsg");
    std::fs::write(&utils_path, utils_text).unwrap();

    // helpers.tsg: imports utils
    let helpers_text = format!(
        r#"
let utils = import("{}")
macro helper_fn(x: rule_t) rule_t {{ utils::utils_fn(x) }}
"#,
        utils_path.display()
    );
    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, &helpers_text).unwrap();

    // grammar.tsg: imports helpers, uses helpers::helper_fn
    let grammar_text = format!(
        r#"
let h = import("{}")
grammar {{ language: "test" }}
rule program {{ h::helper_fn("x") }}
"#,
        helpers_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();
    let helpers_uri = Url::from_file_path(&helpers_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &grammar_text)]).await;

    // Goto-def on "helper_fn" in `h::helper_fn("x")` should jump to helpers.tsg.
    // "helper_fn" is at line 2, col 6 in helpers.tsg (after `macro `).
    let offset = grammar_text.find("h::helper_fn").unwrap() + "h::".len();
    let rope = ropey::Rope::from_str(&grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    assert_eq!(
        goto_def_at(&mut service, grammar_uri.clone(), pos).await,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: helpers_uri,
            range: Range::new(Position::new(2, 6), Position::new(2, 15)),
        }))
    );

    // Completion after `h::` should include both helper_fn and utils (the sub-import).
    let cc_pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);
    let mut items = completions_at(&mut service, grammar_uri, cc_pos).await;
    items.sort_by(|a, b| a.label.cmp(&b.label));

    assert_eq!(
        items,
        vec![
            CompletionItem {
                label: "helper_fn".into(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some("macro helper_fn(x: rule_t) rule_t (h)".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "utils".into(),
                kind: Some(CompletionItemKind::MODULE),
                detail: Some("import utils (h)".into()),
                ..Default::default()
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_through_nested_sub_import() {
    // grammar -> imports h (helpers) -> imports utils.
    // Cursor on `utils_fn` in `h::utils::utils_fn(...)` must walk the chain
    // through `h`'s sub-import `utils` and land on utils.tsg.
    let dir = tempfile::tempdir().unwrap();

    let utils_text = "macro utils_fn(x: rule_t) rule_t { x }\n";
    let utils_path = dir.path().join("utils.tsg");
    std::fs::write(&utils_path, utils_text).unwrap();

    let helpers_text = format!(
        "let utils = import(\"{}\")\nmacro helper_fn(x: rule_t) rule_t {{ x }}\n",
        utils_path.display()
    );
    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, &helpers_text).unwrap();

    let grammar_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"test\" }}\nrule program {{ h::utils::utils_fn(\"x\") }}\n",
        helpers_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();
    let utils_uri = Url::from_file_path(&utils_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &grammar_text)]).await;

    let offset = grammar_text.find("h::utils::utils_fn").unwrap() + "h::utils::".len();
    let rope = ropey::Rope::from_str(&grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    assert_eq!(
        goto_def_at(&mut service, grammar_uri, pos).await,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: utils_uri,
            range: Range::new(Position::new(0, 6), Position::new(0, 14)),
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_through_nested_sub_import() {
    // Companion to goto_def_through_nested_sub_import. Hover on the tail
    // member of a 3-level chain must walk the full path - the old
    // qualifier-by-token-lookback could only see the immediate predecessor
    // and would silently miss.
    let dir = tempfile::tempdir().unwrap();

    let utils_text = "macro utils_fn(x: rule_t) rule_t { x }\n";
    let utils_path = dir.path().join("utils.tsg");
    std::fs::write(&utils_path, utils_text).unwrap();

    let helpers_text = format!(
        "let utils = import(\"{}\")\nmacro helper_fn(x: rule_t) rule_t {{ x }}\n",
        utils_path.display()
    );
    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, &helpers_text).unwrap();

    let grammar_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"test\" }}\nrule program {{ h::utils::utils_fn(\"x\") }}\n",
        helpers_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &grammar_text)]).await;

    let offset = grammar_text.find("h::utils::utils_fn").unwrap() + "h::utils::".len();
    let rope = ropey::Rope::from_str(&grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    assert_eq!(
        hover_at(&mut service, grammar_uri, pos).await,
        make_hover("```\nmacro utils_fn(x: rule_t) rule_t\n```"),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn import_cycle_does_not_hang() {
    // Two files that import each other. The LSP should not hang or crash.
    let dir = tempfile::tempdir().unwrap();

    let a_path = dir.path().join("a.tsg");
    let b_path = dir.path().join("b.tsg");

    let a_text = format!(
        "let b = import(\"{}\")\nmacro a_fn(x: rule_t) rule_t {{ x }}\n",
        b_path.display()
    );
    let b_text = format!(
        "let a = import(\"{}\")\nmacro b_fn(x: rule_t) rule_t {{ x }}\n",
        a_path.display()
    );
    std::fs::write(&a_path, &a_text).unwrap();
    std::fs::write(&b_path, &b_text).unwrap();

    let grammar_text = format!(
        r#"
let moda = import("{}")
grammar {{ language: "test" }}
rule program {{ moda::a_fn("x") }}
"#,
        a_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();

    // Initializing must not hang or crash. The Loader detects the cycle and
    // returns an error; cross-module features like goto-def can't follow the
    // chain past the cycle, but the server stays responsive.
    let mut service = init(&[(grammar_uri.clone(), &grammar_text)]).await;
    let offset = grammar_text.find("moda::a_fn").unwrap() + "moda::".len();
    let rope = ropey::Rope::from_str(&grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);
    let _ = goto_def_at(&mut service, grammar_uri, pos).await;
}

#[tokio::test(flavor = "current_thread")]
async fn references_imported_member() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    // Cursor on "commaSep" in `helpers::commaSep("x")`
    let offset = fix.grammar_text.find("helpers::commaSep").unwrap() + "helpers::".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    // Without declaration: just the usage in this file.
    let result = references_at(&mut service, grammar_uri.clone(), pos, false).await;
    assert_eq!(
        result,
        Some(vec![Location {
            uri: grammar_uri.clone(),
            range: Range::new(Position::new(4, 27), Position::new(4, 35)),
        }])
    );

    // With declaration: usage in grammar + definition in helper.
    let helper_uri = Url::from_file_path(&fix.helper_path).unwrap();
    let mut result = references_at(&mut service, grammar_uri, pos, true)
        .await
        .unwrap();
    result.sort_by(|a, b| {
        a.uri
            .as_str()
            .cmp(b.uri.as_str())
            .then(a.range.start.line.cmp(&b.range.start.line))
    });
    assert_eq!(
        result,
        vec![
            // grammar.tsg usage
            Location {
                uri: Url::from_file_path(&fix.grammar_path).unwrap(),
                range: Range::new(Position::new(4, 27), Position::new(4, 35)),
            },
            // helpers.tsg declaration - "commaSep" at line 1, col 6 (after `macro `).
            Location {
                uri: helper_uri,
                range: Range::new(Position::new(1, 6), Position::new(1, 14)),
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_imported_member_disambiguates_by_qualifier() {
    // Two imports both define `foo`. References on `a::foo` must include only
    // a's foo definition + a-qualified call sites - never b's.
    let dir = tempfile::tempdir().unwrap();

    let a_text = "macro foo(x: rule_t) rule_t { x }\n";
    let a_path = dir.path().join("a.tsg");
    std::fs::write(&a_path, a_text).unwrap();

    let b_text = "macro foo(x: rule_t) rule_t { x }\n";
    let b_path = dir.path().join("b.tsg");
    std::fs::write(&b_path, b_text).unwrap();

    let grammar_text = format!(
        "let a = import(\"{}\")\nlet b = import(\"{}\")\ngrammar {{ language: \"test\" }}\nrule program {{ seq(a::foo(\"x\"), b::foo(\"y\")) }}\n",
        a_path.display(),
        b_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();
    let a_uri = Url::from_file_path(&a_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &grammar_text)]).await;

    // Cursor on `foo` in `a::foo("x")`.
    let offset = grammar_text.find("a::foo").unwrap() + "a::".len();
    let rope = ropey::Rope::from_str(&grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let mut result = references_at(&mut service, grammar_uri.clone(), pos, true)
        .await
        .expect("references should succeed");
    result.sort_by(|x, y| {
        x.uri
            .as_str()
            .cmp(y.uri.as_str())
            .then(x.range.start.line.cmp(&y.range.start.line))
            .then(x.range.start.character.cmp(&y.range.start.character))
    });

    // a's def site + the a::foo call site only (b::foo and b's def excluded).
    let a_call_offset = grammar_text.find("a::foo").unwrap() + "a::".len();
    let a_call_start = ts_grammar_ls::text::offset_to_position(&rope, a_call_offset as u32);
    let a_call_end =
        ts_grammar_ls::text::offset_to_position(&rope, (a_call_offset + "foo".len()) as u32);

    assert_eq!(
        result,
        vec![
            Location {
                uri: a_uri,
                range: Range::new(Position::new(0, 6), Position::new(0, 9)),
            },
            Location {
                uri: grammar_uri,
                range: Range::new(a_call_start, a_call_end),
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn highlight_imported_member() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    // Cursor on "commaSep" in `helpers::commaSep("x")`
    let offset = fix.grammar_text.find("helpers::commaSep").unwrap() + "helpers::".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    assert_eq!(
        highlights_at(&mut service, grammar_uri, pos).await,
        Some(vec![DocumentHighlight {
            range: Range::new(Position::new(4, 27), Position::new(4, 35)),
            kind: Some(DocumentHighlightKind::READ),
        }])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_import_variable() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    // Cursor on "helpers" in `let helpers = import("...")`
    let offset = fix.grammar_text.find("let helpers").unwrap() + "let ".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    assert_eq!(
        hover_at(&mut service, grammar_uri, pos).await,
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: "```\nimport helpers\n```".into(),
            }),
            range: None,
        })
    );
}

// ---------------------------------------------------------------------------
// grammar_config tests
// ---------------------------------------------------------------------------

fn create_grammar_config_fixture() -> InheritFixture {
    let dir = tempfile::tempdir().unwrap();

    let base_text = r#"
grammar { language: "base_lang" }
rule program { repeat(_statement) }
rule _statement { choice(expression, "x") }
rule expression { choice(identifier, number) }
rule identifier { regexp(r"[a-z]+") }
rule number { regexp(r"[0-9]+") }
"#;
    let base_path = dir.path().join("base.tsg");
    std::fs::write(&base_path, base_text).unwrap();

    let derived_text = format!(
        r#"
let base = inherit("{}")
grammar {{
    language: "derived_lang",
    inherits: base,
    extras: grammar_config(base, extras),
}}
rule new_rule {{ "new" }}
"#,
        base_path.display()
    );
    let derived_path = dir.path().join("derived.tsg");
    std::fs::write(&derived_path, &derived_text).unwrap();

    InheritFixture {
        _dir: dir,
        base_path,
        _base_text: base_text.to_string(),
        derived_path,
        derived_text,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn completion_grammar_config_dot_fields() {
    let fix = create_grammar_config_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor right after `grammar_config(base, ` before "extras"
    let offset = fix
        .derived_text
        .find("grammar_config(base, extras)")
        .unwrap()
        + "grammar_config(base, ".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let mut items = completions_at(&mut service, derived_uri, pos).await;
    items.sort_by(|a, b| a.label.cmp(&b.label));

    let ci = |label: &str, ty: &str| CompletionItem {
        label: label.into(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(format!("{label}: {ty}")),
        ..Default::default()
    };
    assert_eq!(
        items,
        vec![
            ci("conflicts", "list_list_rule_t"),
            ci("externals", "list_rule_t"),
            ci("extras", "list_rule_t"),
            ci("inherits", "grammar"),
            ci("inline", "list_rule_t"),
            ci("language", "str_t"),
            ci("precedences", "list_list_rule_t"),
            ci("reserved", "{ [context]: list_rule_t }"),
            ci("start", "rule_t"),
            ci("supertypes", "list_rule_t"),
            ci("word", "rule_t"),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_grammar_config_builtin() {
    let fix = create_grammar_config_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor on "grammar_config" keyword
    let offset = fix
        .derived_text
        .find("grammar_config(base, extras)")
        .unwrap();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let result = hover_at(&mut service, derived_uri, pos).await;
    let hover = result.unwrap();
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected markup");
    };
    // Should show the grammar_config builtin docs
    assert_eq!(markup.value, ts_grammar_ls::hover_docs::GRAMMAR_CONFIG);
}

#[tokio::test(flavor = "current_thread")]
async fn hover_grammar_config_field_access() {
    let fix = create_grammar_config_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor on "extras" in `grammar_config(base, extras)`
    let gc_offset = fix
        .derived_text
        .find("grammar_config(base, extras)")
        .unwrap();
    let offset = gc_offset + "grammar_config(base, ".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    // Should show the same docs as hovering on `extras:` inside a grammar block.
    assert_eq!(
        hover_at(&mut service, derived_uri, pos).await,
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: ts_grammar_ls::hover_docs::CFG_EXTRAS.into(),
            }),
            range: None,
        })
    );
}

// ---------------------------------------------------------------------------
// Stale analysis reuse tests
// ---------------------------------------------------------------------------

/// Helper: send a didChange notification to simulate editing a document.
async fn did_change(service: &mut LspService<Backend>, uri: Url, version: i32, text: &str) {
    lsp_notify::<DidChangeTextDocument>(
        service,
        DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri, version },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: text.to_string(),
            }],
        },
    )
    .await;
    // Small sleep to let the document update propagate.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn completion_works_after_syntax_error() {
    let good_grammar = r#"
grammar { language: "test" }
rule program { repeat(expression) }
rule expression { "x" }
"#;
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), good_grammar)]).await;

    // Verify completion works on the good grammar.
    let rope = ropey::Rope::from_str(good_grammar);
    let pos = ts_grammar_ls::text::offset_to_position(
        &rope,
        good_grammar.find("expression").unwrap() as u32,
    );
    let items = completions_at(&mut service, uri.clone(), pos).await;
    assert!(
        items.iter().any(|i| i.label == "program"),
        "should have completions before edit: {items:?}"
    );

    // Edit to introduce a syntax error (truncate mid-rule).
    let broken = r#"
grammar { language: "test" }
rule program { repeat(expression) }
rule expression {
"#;
    did_change(&mut service, uri.clone(), 1, broken).await;

    // Completion should still work using the last good analysis.
    let broken_rope = ropey::Rope::from_str(broken);
    let pos = ts_grammar_ls::text::offset_to_position(
        &broken_rope,
        broken.find("expression").unwrap() as u32,
    );
    let items = completions_at(&mut service, uri, pos).await;
    assert!(
        items.iter().any(|i| i.label == "program"),
        "should still have completions after syntax error: {items:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_works_after_syntax_error() {
    let good_grammar = r#"
grammar { language: "test" }
macro helper(x: rule_t) rule_t { x }
rule program { helper("x") }
"#;
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), good_grammar)]).await;

    // Seed the analysis cache by triggering a handler on the good grammar.
    let rope = ropey::Rope::from_str(good_grammar);
    let pos = ts_grammar_ls::text::offset_to_position(
        &rope,
        good_grammar.find("program").unwrap() as u32,
    );
    assert!(hover_at(&mut service, uri.clone(), pos).await.is_some());

    // Edit to introduce a syntax error.
    let broken = r#"
grammar { language: "test" }
macro helper(x: rule_t) rule_t { x }
rule program { helper(
"#;
    did_change(&mut service, uri.clone(), 1, broken).await;

    // Hover on "helper" should still show the function signature
    // from the last good analysis.
    let broken_rope = ropey::Rope::from_str(broken);
    let pos = ts_grammar_ls::text::offset_to_position(
        &broken_rope,
        broken.rfind("helper").unwrap() as u32,
    );
    assert_eq!(
        hover_at(&mut service, uri, pos).await,
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: "```\nmacro helper(x: rule_t) rule_t\n```".into(),
            }),
            range: None,
        })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_works_after_syntax_error() {
    let good_grammar = r#"
grammar { language: "test" }
rule program { repeat(expression) }
rule expression { "x" }
"#;
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), good_grammar)]).await;

    // Seed the analysis cache by triggering a handler on the good grammar.
    let rope = ropey::Rope::from_str(good_grammar);
    let pos = ts_grammar_ls::text::offset_to_position(
        &rope,
        good_grammar.find("program").unwrap() as u32,
    );
    assert!(hover_at(&mut service, uri.clone(), pos).await.is_some());

    // Edit to introduce a syntax error.
    let broken = r#"
grammar { language: "test" }
rule program { repeat(expression) }
rule expression {
"#;
    did_change(&mut service, uri.clone(), 1, broken).await;

    // Goto-def on "expression" in `repeat(expression)` should still
    // jump to the rule definition from the last good analysis.
    let broken_rope = ropey::Rope::from_str(broken);
    let pos = ts_grammar_ls::text::offset_to_position(
        &broken_rope,
        broken.find("expression").unwrap() as u32,
    );
    let result = goto_def_at(&mut service, uri, pos).await;
    // Should find the definition - exact position from the stale analysis.
    assert!(result.is_some(), "goto-def should work after syntax error");
}

// ---------------------------------------------------------------------------
// Rename tests
// ---------------------------------------------------------------------------

async fn prepare_rename_at(
    service: &mut LspService<Backend>,
    uri: Url,
    pos: Position,
) -> Option<PrepareRenameResponse> {
    lsp_request::<PrepareRenameRequest>(
        service,
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position: pos,
        },
    )
    .await
}

async fn rename_at(
    service: &mut LspService<Backend>,
    uri: Url,
    pos: Position,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    lsp_request::<Rename>(
        service,
        RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: pos,
            },
            new_name: new_name.into(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn rename_rule() {
    let grammar = r#"
grammar { language: "test" }
rule program { repeat(expression) }
rule expression { "x" }
"#;
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), grammar)]).await;

    // Cursor on "expression" at its definition (line 3).
    let rope = ropey::Rope::from_str(grammar);
    let def_offset = grammar.rfind("expression").unwrap();
    let pos = ts_grammar_ls::text::offset_to_position(&rope, def_offset as u32);

    // prepare_rename should succeed and return the range of "expression".
    let prep = prepare_rename_at(&mut service, uri.clone(), pos).await;
    assert!(prep.is_some(), "prepare_rename should succeed on rule name");

    // Rename "expression" to "expr".
    let edit = rename_at(&mut service, uri.clone(), pos, "expr").await;
    let edit = edit.unwrap();
    let changes = edit.changes.unwrap();
    let edits = &changes[&uri];

    // Should have 2 edits: definition site + usage in `repeat(expression)`.
    assert_eq!(edits.len(), 2);
    assert!(edits.iter().all(|e| e.new_text == "expr"));
}

#[tokio::test(flavor = "current_thread")]
async fn rename_function() {
    let grammar = r#"
grammar { language: "test" }
macro helper(x: rule_t) rule_t { x }
rule program { helper("x") }
"#;
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), grammar)]).await;

    let rope = ropey::Rope::from_str(grammar);
    let offset = grammar.find("helper").unwrap();
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let edit = rename_at(&mut service, uri.clone(), pos, "wrap")
        .await
        .unwrap();
    let edits = &edit.changes.unwrap()[&uri];

    // Definition + usage = 2 edits.
    assert_eq!(edits.len(), 2);
    assert!(edits.iter().all(|e| e.new_text == "wrap"));
}

#[tokio::test(flavor = "current_thread")]
async fn rename_parameter_scoped() {
    let grammar = r#"
grammar { language: "test" }
macro foo(item: rule_t) rule_t { item }
macro bar(item: rule_t) rule_t { seq(item, item) }
rule program { foo("x") }
"#;
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), grammar)]).await;

    // Rename "item" inside foo - should NOT affect bar's "item".
    let rope = ropey::Rope::from_str(grammar);
    let foo_item_offset = grammar.find("item").unwrap();
    let pos = ts_grammar_ls::text::offset_to_position(&rope, foo_item_offset as u32);

    let edit = rename_at(&mut service, uri.clone(), pos, "x")
        .await
        .unwrap();
    let edits = &edit.changes.unwrap()[&uri];

    // foo's "item" appears twice: parameter + body usage.
    assert_eq!(edits.len(), 2);
    assert!(edits.iter().all(|e| e.new_text == "x"));

    // Verify the edits are in foo's range, not bar's.
    let foo_end = grammar.find("macro bar").unwrap() as u32;
    for edit in edits {
        let edit_byte = ts_grammar_ls::text::position_to_offset(&rope, edit.range.start).unwrap();
        assert!(
            edit_byte < foo_end,
            "edit should be in foo, not bar: byte {edit_byte}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn prepare_rename_under_shadowing_returns_innermost_range() {
    // A top-level `let item` is shadowed by a macro parameter `item`.
    // Clicking on the parameter must return the parameter's range, not the let's.
    let grammar = r#"
grammar { language: "test" }
let item = "x"
macro foo(item: rule_t) rule_t { item }
rule program { foo("x") }
"#;
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), grammar)]).await;

    let rope = ropey::Rope::from_str(grammar);
    let param_offset = (grammar.find("macro foo(").unwrap() + "macro foo(".len()) as u32;
    let pos = ts_grammar_ls::text::offset_to_position(&rope, param_offset);

    let prep = prepare_rename_at(&mut service, uri.clone(), pos)
        .await
        .expect("prepare_rename should succeed on parameter");
    let range = match prep {
        PrepareRenameResponse::Range(r) => r,
        other => panic!("expected Range response, got {other:?}"),
    };

    let returned_start = ts_grammar_ls::text::position_to_offset(&rope, range.start).unwrap();
    let returned_end = ts_grammar_ls::text::position_to_offset(&rope, range.end).unwrap();
    assert_eq!(
        (returned_start, returned_end),
        (param_offset, param_offset + "item".len() as u32),
        "prepare_rename should return the parameter's range, not the let's range"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_at_parameter_decl_under_shadowing() {
    // Top-level `let item` shadowed by macro parameter `item`. Clicking on the
    // parameter declaration must jump to the parameter, not the let.
    let grammar = r#"
grammar { language: "test" }
let item = "x"
macro foo(item: rule_t) rule_t { item }
"#;
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), grammar)]).await;

    let rope = ropey::Rope::from_str(grammar);
    let param_offset = (grammar.find("macro foo(").unwrap() + "macro foo(".len()) as u32;
    let pos = ts_grammar_ls::text::offset_to_position(&rope, param_offset);

    let resp = goto_def_at(&mut service, uri.clone(), pos)
        .await
        .expect("goto_def should succeed on parameter decl");
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location, got {resp:?}");
    };

    let expected_start = ts_grammar_ls::text::offset_to_position(&rope, param_offset);
    let expected_end =
        ts_grammar_ls::text::offset_to_position(&rope, param_offset + "item".len() as u32);
    assert_eq!(loc.uri, uri);
    assert_eq!(
        loc.range,
        Range::new(expected_start, expected_end),
        "goto_def should land on the parameter, not the let"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rename_under_shadowing_only_edits_innermost_binding() {
    // Top-level `let item` shadowed by macro parameter `item`.
    // Renaming the parameter must not touch the let or any references to it.
    let grammar = r#"
grammar { language: "test" }
let item = "x"
macro foo(item: rule_t) rule_t { item }
rule program { foo(item) }
"#;
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), grammar)]).await;

    let rope = ropey::Rope::from_str(grammar);
    let param_offset = (grammar.find("macro foo(").unwrap() + "macro foo(".len()) as u32;
    let pos = ts_grammar_ls::text::offset_to_position(&rope, param_offset);

    let edit = rename_at(&mut service, uri.clone(), pos, "x")
        .await
        .expect("rename should succeed on parameter");
    let edits = &edit.changes.unwrap()[&uri];

    // Exactly two edits: parameter declaration + parameter use inside macro body.
    // The top-level `let item` and its use in `rule program` must be untouched.
    assert_eq!(edits.len(), 2, "expected 2 edits, got: {edits:#?}");

    let macro_start = grammar.find("macro foo").unwrap() as u32;
    let macro_end = grammar.find("rule program").unwrap() as u32;
    for e in edits {
        let byte = ts_grammar_ls::text::position_to_offset(&rope, e.range.start).unwrap();
        assert!(
            byte >= macro_start && byte < macro_end,
            "edit at byte {byte} is outside the macro span [{macro_start}, {macro_end})"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rename_rejects_invalid_name() {
    let grammar = r#"
grammar { language: "test" }
rule program { "x" }
"#;
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), grammar)]).await;

    let rope = ropey::Rope::from_str(grammar);
    let offset = grammar.find("program").unwrap();
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    // Invalid names should return None.
    assert_eq!(
        rename_at(&mut service, uri.clone(), pos, "1bad").await,
        None
    );
    assert_eq!(rename_at(&mut service, uri.clone(), pos, "").await, None);
    assert_eq!(
        rename_at(&mut service, uri.clone(), pos, "has space").await,
        None
    );

    // DSL keywords are accepted as rename targets but auto-escaped: the
    // resulting TextEdit substitutes `r#keyword` so the source stays valid.
    for kw in [
        "grammar",
        "rule",
        "let",
        "macro",
        "for",
        "in",
        "inherit",
        "import",
        "override",
        "append",
        "grammar_config",
        "reserved",
        "seq",
        "choice",
        "repeat",
        "repeat1",
        "optional",
        "blank",
        "field",
        "alias",
        "token",
        "token_immediate",
        "concat",
        "regexp",
        "prec",
        "prec_left",
        "prec_right",
        "prec_dynamic",
    ] {
        let edit = rename_at(&mut service, uri.clone(), pos, kw)
            .await
            .unwrap_or_else(|| {
                panic!("rename to keyword `{kw}` should auto-escape, not be rejected")
            });
        let edits = &edit.changes.unwrap()[&uri];
        for e in edits {
            assert_eq!(
                e.new_text,
                format!("r#{kw}"),
                "keyword target `{kw}` should produce `r#{kw}`"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn prepare_rename_rejects_builtins() {
    let grammar = r#"
grammar { language: "test" }
rule program { seq("a", "b") }
"#;
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), grammar)]).await;

    // Cursor on "seq" - a builtin, not renameable.
    let rope = ropey::Rope::from_str(grammar);
    let offset = grammar.find("seq").unwrap();
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    assert_eq!(
        prepare_rename_at(&mut service, uri, pos).await,
        None,
        "builtins should not be renameable"
    );
}

// ---------------------------------------------------------------------------
// Cross-file dependency tracking
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn dependent_diagnostics_republish_on_helper_change() {
    // grammar imports helpers; both are open. Editing helpers (a no-op-ish
    // change that still bumps the version) should cause grammar's diagnostics
    // to be republished, even though grammar's own text didn't change.
    let dir = tempfile::tempdir().unwrap();

    let helpers_text_v1 = "macro foo(x: rule_t) rule_t { x }\n";
    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, helpers_text_v1).unwrap();

    let grammar_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"test\" }}\nrule program {{ h::foo(\"x\") }}\n",
        helpers_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let helpers_uri = Url::from_file_path(&helpers_path).unwrap();
    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();

    let (mut service, publish_log) = init_with_capture(
        &[
            (helpers_uri.clone(), helpers_text_v1),
            (grammar_uri.clone(), &grammar_text),
        ],
        None,
    )
    .await;

    // Force the grammar's analysis to run so the dependents index is populated
    // (analysis is lazy; otherwise no-one has loaded helpers as a dep yet).
    let _ = hover_at(&mut service, grammar_uri.clone(), Position::new(2, 17)).await;

    publish_log.lock().await.clear();

    // Now edit helpers (different but still valid).
    let helpers_text_v2 = "macro foo(x: rule_t) rule_t { seq(x, x) }\n";
    lsp_notify::<DidChangeTextDocument>(
        &mut service,
        DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: helpers_uri.clone(),
                version: 1,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: helpers_text_v2.into(),
            }],
        },
    )
    .await;

    // Give the spawned publish task time to run.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let log = publish_log.lock().await;
    let uris: Vec<&Url> = log.iter().map(|p| &p.uri).collect();
    assert!(
        uris.contains(&&helpers_uri),
        "helpers.tsg should be republished after its own didChange. got: {uris:?}"
    );
    assert!(
        uris.contains(&&grammar_uri),
        "grammar.tsg should be republished as a dependent of helpers.tsg. got: {uris:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rename_imported_member_edits_all_open_dependents() {
    // Two grammars (grammar_a, grammar_b) both import the same helpers file
    // and both call helpers::foo. Renaming foo from grammar_a should produce
    // edits in helpers, grammar_a, AND grammar_b in a single WorkspaceEdit.
    let dir = tempfile::tempdir().unwrap();

    let helpers_text = "macro foo(x: rule_t) rule_t { x }\n";
    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, helpers_text).unwrap();

    let grammar_a_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"a\" }}\nrule program {{ h::foo(\"x\") }}\n",
        helpers_path.display()
    );
    let grammar_a_path = dir.path().join("grammar_a.tsg");
    std::fs::write(&grammar_a_path, &grammar_a_text).unwrap();

    let grammar_b_text = format!(
        "let other = import(\"{}\")\ngrammar {{ language: \"b\" }}\nrule program {{ other::foo(\"y\") }}\n",
        helpers_path.display()
    );
    let grammar_b_path = dir.path().join("grammar_b.tsg");
    std::fs::write(&grammar_b_path, &grammar_b_text).unwrap();

    let helpers_uri = Url::from_file_path(&helpers_path).unwrap();
    let grammar_a_uri = Url::from_file_path(&grammar_a_path).unwrap();
    let grammar_b_uri = Url::from_file_path(&grammar_b_path).unwrap();

    let mut service = init(&[
        (grammar_a_uri.clone(), &grammar_a_text),
        (grammar_b_uri.clone(), &grammar_b_text),
    ])
    .await;

    // Force analysis on both so the dependents index has both URIs.
    let _ = hover_at(&mut service, grammar_a_uri.clone(), Position::new(2, 17)).await;
    let _ = hover_at(&mut service, grammar_b_uri.clone(), Position::new(2, 21)).await;

    // Cursor on `foo` in grammar_a's `h::foo("x")`.
    let offset = grammar_a_text.find("h::foo").unwrap() + "h::".len();
    let rope = ropey::Rope::from_str(&grammar_a_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let edit = rename_at(&mut service, grammar_a_uri.clone(), pos, "bar")
        .await
        .expect("rename should succeed");
    let changes = edit.changes.expect("changes present");

    // Three files edited: helpers (def site), grammar_a (h::foo call),
    // grammar_b (other::foo call).
    let mut keys: Vec<&Url> = changes.keys().collect();
    keys.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(
        keys,
        vec![&grammar_a_uri, &grammar_b_uri, &helpers_uri],
        "expected edits in all three files; got {:?}",
        changes.keys().collect::<Vec<_>>()
    );

    // Each file should have exactly one edit (one def or one call site),
    // all renaming to "bar".
    for (uri, edits) in &changes {
        assert_eq!(edits.len(), 1, "file {uri} should have one edit");
        assert_eq!(edits[0].new_text, "bar");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rename_does_not_touch_unrelated_imports_with_same_name() {
    // grammar_a imports helpers (with foo). grammar_b imports a DIFFERENT
    // module that also happens to expose a `foo`. Renaming helpers::foo from
    // grammar_a must not touch grammar_b's other_foo::foo at all (they are
    // different bindings even though the names match).
    let dir = tempfile::tempdir().unwrap();

    let helpers_text = "macro foo(x: rule_t) rule_t { x }\n";
    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, helpers_text).unwrap();

    let other_text = "macro foo(x: rule_t) rule_t { x }\n";
    let other_path = dir.path().join("other.tsg");
    std::fs::write(&other_path, other_text).unwrap();

    let grammar_a_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"a\" }}\nrule program {{ h::foo(\"x\") }}\n",
        helpers_path.display()
    );
    let grammar_a_path = dir.path().join("grammar_a.tsg");
    std::fs::write(&grammar_a_path, &grammar_a_text).unwrap();

    let grammar_b_text = format!(
        "let o = import(\"{}\")\ngrammar {{ language: \"b\" }}\nrule program {{ o::foo(\"y\") }}\n",
        other_path.display()
    );
    let grammar_b_path = dir.path().join("grammar_b.tsg");
    std::fs::write(&grammar_b_path, &grammar_b_text).unwrap();

    let helpers_uri = Url::from_file_path(&helpers_path).unwrap();
    let grammar_a_uri = Url::from_file_path(&grammar_a_path).unwrap();
    let grammar_b_uri = Url::from_file_path(&grammar_b_path).unwrap();

    let mut service = init(&[
        (grammar_a_uri.clone(), &grammar_a_text),
        (grammar_b_uri.clone(), &grammar_b_text),
    ])
    .await;

    let _ = hover_at(&mut service, grammar_a_uri.clone(), Position::new(2, 17)).await;
    let _ = hover_at(&mut service, grammar_b_uri.clone(), Position::new(2, 17)).await;

    let offset = grammar_a_text.find("h::foo").unwrap() + "h::".len();
    let rope = ropey::Rope::from_str(&grammar_a_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let edit = rename_at(&mut service, grammar_a_uri.clone(), pos, "bar")
        .await
        .expect("rename should succeed");
    let changes = edit.changes.expect("changes present");

    let mut keys: Vec<&Url> = changes.keys().collect();
    keys.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(
        keys,
        vec![&grammar_a_uri, &helpers_uri],
        "grammar_b uses a different module's foo and must not be edited"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn watched_files_change_republishes_open_dependents() {
    // helpers.tsg is NOT open in the editor; only grammar.tsg is. When the
    // client signals an on-disk change to helpers.tsg via didChangeWatchedFiles,
    // grammar.tsg's diagnostics should refresh (loader reads the new helpers
    // content from disk).
    let dir = tempfile::tempdir().unwrap();

    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, "macro foo(x: rule_t) rule_t { x }\n").unwrap();

    let grammar_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"test\" }}\nrule program {{ h::foo(\"x\") }}\n",
        helpers_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let helpers_uri = Url::from_file_path(&helpers_path).unwrap();
    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();

    let (mut service, publish_log) =
        init_with_capture(&[(grammar_uri.clone(), &grammar_text)], None).await;

    // Force grammar.tsg's analysis so the dependents index records helpers.
    let _ = hover_at(&mut service, grammar_uri.clone(), Position::new(2, 17)).await;

    publish_log.lock().await.clear();

    // Simulate the client telling us helpers.tsg changed on disk.
    lsp_notify::<DidChangeWatchedFiles>(
        &mut service,
        DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: helpers_uri.clone(),
                typ: FileChangeType::CHANGED,
            }],
        },
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let log = publish_log.lock().await;
    let uris: Vec<&Url> = log.iter().map(|p| &p.uri).collect();
    assert!(
        uris.contains(&&grammar_uri),
        "grammar.tsg should be republished after watcher event for helpers.tsg. got: {uris:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rename_includes_closed_workspace_dependents() {
    // Three grammars in a workspace, all import the same helper. Only one
    // is open in the editor; the workspace scan must discover the other two
    // from disk and rename should produce edits for all of them.
    let dir = tempfile::tempdir().unwrap();

    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, "macro foo(x: rule_t) rule_t { x }\n").unwrap();

    let parser_a_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"a\" }}\nrule program {{ h::foo(\"x\") }}\n",
        helpers_path.display()
    );
    let parser_a_path = dir.path().join("parser_a.tsg");
    std::fs::write(&parser_a_path, &parser_a_text).unwrap();

    let parser_b_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"b\" }}\nrule program {{ h::foo(\"y\") }}\n",
        helpers_path.display()
    );
    let parser_b_path = dir.path().join("parser_b.tsg");
    std::fs::write(&parser_b_path, &parser_b_text).unwrap();

    let parser_c_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"c\" }}\nrule program {{ h::foo(\"z\") }}\n",
        helpers_path.display()
    );
    let parser_c_path = dir.path().join("parser_c.tsg");
    std::fs::write(&parser_c_path, &parser_c_text).unwrap();

    let helpers_uri = Url::from_file_path(&helpers_path).unwrap();
    let parser_a_uri = Url::from_file_path(&parser_a_path).unwrap();
    let parser_b_uri = Url::from_file_path(&parser_b_path).unwrap();
    let parser_c_uri = Url::from_file_path(&parser_c_path).unwrap();

    // Only parser_a is open. Workspace scan should pick up b and c.
    let (mut service, _log) =
        init_with_capture(&[(parser_a_uri.clone(), &parser_a_text)], Some(dir.path())).await;

    // Force parser_a's analysis so dependents picks up its open-file deps too.
    let _ = hover_at(&mut service, parser_a_uri.clone(), Position::new(2, 17)).await;

    let offset = parser_a_text.find("h::foo").unwrap() + "h::".len();
    let rope = ropey::Rope::from_str(&parser_a_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let edit = rename_at(&mut service, parser_a_uri.clone(), pos, "bar")
        .await
        .expect("rename should succeed");
    let changes = edit.changes.expect("changes present");

    let mut keys: Vec<&Url> = changes.keys().collect();
    keys.sort_by(|x, y| x.as_str().cmp(y.as_str()));
    assert_eq!(
        keys,
        vec![&helpers_uri, &parser_a_uri, &parser_b_uri, &parser_c_uri],
        "rename must reach closed workspace dependents (parser_b, parser_c)"
    );
    for (uri, edits) in &changes {
        assert_eq!(edits.len(), 1, "{uri} should have one edit");
        assert_eq!(edits[0].new_text, "bar");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn for_loop_spread_into_list_extracts_binding() {
    // Core supports spreading a for-loop into list literals (extras, etc.).
    // Verify the LSP picks up the for-loop binding so goto-def on a use
    // inside the spread body lands on the binding.
    let grammar = r#"
let kws: list_t<str_t> = [for (kw: str_t) in ["if", "else"] { kw }]
grammar {
    language: "test",
    reserved: { global: kws },
}
rule program { "x" }
"#;
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), grammar)]).await;

    // Cursor on the body `kw` (the use inside the for body).
    let body_offset = grammar.find("{ kw }").unwrap() + "{ ".len();
    let rope = ropey::Rope::from_str(grammar);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, body_offset as u32);

    let resp = goto_def_at(&mut service, uri.clone(), pos)
        .await
        .expect("goto-def should resolve for-loop binding");
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    // Should land on the binding (`(kw: str_t)`).
    let binding_offset = grammar.find("(kw: str_t)").unwrap() + 1; // start of `kw`
    let expected_start = ts_grammar_ls::text::offset_to_position(&rope, binding_offset as u32);
    assert_eq!(loc.uri, uri);
    assert_eq!(loc.range.start, expected_start);
}

// ---------------------------------------------------------------------------
// Raw identifier syntax (`r#name`)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn hover_raw_ident_rule_on_bare_name_part() {
    // `r#let` defines a rule whose name is `let`. Hovering on the `let`
    // part should show "rule let".
    let grammar = "grammar { language: \"test\" }\nrule r#let { \"x\" }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on `l` of `r#let`.
    let l_offset = grammar.find("r#let").unwrap() + "r#".len();
    let rope = ropey::Rope::from_str(grammar);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, l_offset as u32);

    assert_eq!(
        hover_at(&mut service, test_uri(), pos).await,
        make_hover("```\nrule let\n```"),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_raw_ident_rule_on_prefix_part() {
    // Hovering on the `r` or `#` of `r#let` should still produce something
    // useful, since the user clicked on the rule's name.
    let grammar = "grammar { language: \"test\" }\nrule r#let { \"x\" }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    let r_offset = grammar.find("r#let").unwrap();
    let rope = ropey::Rope::from_str(grammar);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, r_offset as u32);

    assert_eq!(
        hover_at(&mut service, test_uri(), pos).await,
        make_hover("```\nrule let\n```"),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rename_raw_ident_rule_to_normal_name() {
    // Rename `r#let` (rule whose name is the keyword `let`) to a non-keyword.
    // The source should end up as just `bar`, not `r#bar` (since `bar` is
    // a regular identifier, the `r#` is redundant and should be dropped).
    let grammar = "grammar { language: \"test\" }\nrule r#let { \"x\" }\nrule program { r#let }\n";
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), grammar)]).await;

    let l_offset = (grammar.find("r#let").unwrap() + "r#".len()) as u32;
    let rope = ropey::Rope::from_str(grammar);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, l_offset);

    let edit = rename_at(&mut service, uri.clone(), pos, "bar")
        .await
        .expect("rename should succeed");
    let edits = &edit.changes.unwrap()[&uri];

    // Apply the edits to the source and verify it's clean.
    let mut after = grammar.to_owned();
    let mut sorted = edits.clone();
    sorted.sort_by(|a, b| b.range.start.cmp(&a.range.start));
    let after_rope = ropey::Rope::from_str(grammar);
    for e in &sorted {
        let s =
            ts_grammar_ls::text::position_to_offset(&after_rope, e.range.start).unwrap() as usize;
        let n = ts_grammar_ls::text::position_to_offset(&after_rope, e.range.end).unwrap() as usize;
        after.replace_range(s..n, &e.new_text);
    }
    assert_eq!(
        after,
        "grammar { language: \"test\" }\nrule bar { \"x\" }\nrule program { bar }\n"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rename_normal_rule_to_keyword_uses_raw_syntax() {
    // Renaming `foo` to `let` (a keyword) should produce `r#let` in source
    // so the result is still valid DSL.
    let grammar = "grammar { language: \"test\" }\nrule foo { \"x\" }\nrule program { foo }\n";
    let uri = test_uri();
    let mut service = init(&[(uri.clone(), grammar)]).await;

    let foo_offset = grammar.find("rule foo").unwrap() + "rule ".len();
    let rope = ropey::Rope::from_str(grammar);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, foo_offset as u32);

    let edit = rename_at(&mut service, uri.clone(), pos, "let")
        .await
        .expect("rename should succeed (keyword target should auto-escape)");
    let edits = &edit.changes.unwrap()[&uri];
    for e in edits {
        assert_eq!(e.new_text, "r#let", "keyword target needs r# escaping");
    }
}

// ---------------------------------------------------------------------------
// Helper-file rules (bare-name access from importer)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn completion_offers_helper_rules_by_bare_name() {
    let dir = tempfile::tempdir().unwrap();

    let helpers_text = "rule shared_rule { \"x\" }\n";
    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, helpers_text).unwrap();

    let grammar_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"test\" }}\nrule program {{ shared_rule }}\n",
        helpers_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();
    let mut service = init(&[(grammar_uri.clone(), &grammar_text)]).await;

    // Cursor at the start of the rule body, before any identifier.
    let s_offset = grammar_text.find("{ shared_rule }").unwrap() + "{ ".len();
    let rope = ropey::Rope::from_str(&grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, s_offset as u32);

    let items = completions_at(&mut service, grammar_uri, pos).await;
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"shared_rule"),
        "bare-name completion should surface helper rules. got: {labels:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cross_file_features_survive_unresolved_name_mid_edit() {
    // After the file has been analyzed once successfully, introducing an
    // unresolved name (which fails the Loader's resolve stage) shouldn't
    // wipe out the cross-file `import_modules` info. Hover on a helper rule
    // by bare name should still work.
    let dir = tempfile::tempdir().unwrap();

    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, "rule shared_rule { \"x\" }\n").unwrap();

    let original = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"test\" }}\nrule program {{ shared_rule }}\n",
        helpers_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &original).unwrap();

    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();
    let mut service = init(&[(grammar_uri.clone(), &original)]).await;

    // Force the first analysis to populate `last_good_analysis`.
    let body_offset = original.find("{ shared_rule }").unwrap() + "{ ".len();
    let rope = ropey::Rope::from_str(&original);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, body_offset as u32);
    assert!(
        hover_at(&mut service, grammar_uri.clone(), pos)
            .await
            .is_some(),
        "baseline hover should work"
    );

    // Edit the buffer to introduce an unresolved name AFTER the helper
    // reference - resolve will fail but the helper reference is still valid.
    let broken = original.replace(
        "rule program { shared_rule }",
        "rule program { shared_rule }\nrule other { undefined_name }",
    );
    lsp_notify::<DidChangeTextDocument>(
        &mut service,
        DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: grammar_uri.clone(),
                version: 1,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: broken.clone(),
            }],
        },
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Hover should still resolve `shared_rule` to its helper definition,
    // even though the document as a whole no longer passes the full pipeline.
    let rope2 = ropey::Rope::from_str(&broken);
    let pos2 = ts_grammar_ls::text::offset_to_position(&rope2, body_offset as u32);
    assert_eq!(
        hover_at(&mut service, grammar_uri, pos2).await,
        make_hover("```\nrule shared_rule\n```"),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_helper_rule_via_bare_name() {
    let dir = tempfile::tempdir().unwrap();

    let helpers_text = "rule shared_rule { \"x\" }\n";
    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, helpers_text).unwrap();

    let grammar_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"test\" }}\nrule program {{ shared_rule }}\n",
        helpers_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &grammar_text)]).await;

    let body_offset = grammar_text.find("{ shared_rule }").unwrap() + "{ ".len();
    let rope = ropey::Rope::from_str(&grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, body_offset as u32);

    assert_eq!(
        hover_at(&mut service, grammar_uri, pos).await,
        make_hover("```\nrule shared_rule\n```"),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_helper_rule_via_bare_name() {
    // A rule declared in an imported helper is reachable by bare name from
    // the importing grammar. Goto-def should jump into the helper.
    let dir = tempfile::tempdir().unwrap();

    let helpers_text = "rule shared_rule { \"x\" }\n";
    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, helpers_text).unwrap();

    let grammar_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"test\" }}\nrule program {{ shared_rule }}\n",
        helpers_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let helpers_uri = Url::from_file_path(&helpers_path).unwrap();
    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &grammar_text)]).await;

    // Cursor on bare `shared_rule` in grammar.
    let body_offset = grammar_text.find("{ shared_rule }").unwrap() + "{ ".len();
    let rope = ropey::Rope::from_str(&grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, body_offset as u32);

    assert_eq!(
        goto_def_at(&mut service, grammar_uri, pos).await,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: helpers_uri,
            range: Range::new(Position::new(0, 5), Position::new(0, 16)),
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_helper_rule_via_bare_name() {
    // Two grammars import the same helper, both reference its rule by bare
    // name. References from one grammar's call site should find the helper's
    // def + both grammars' call sites.
    let dir = tempfile::tempdir().unwrap();

    let helpers_text = "rule shared_rule { \"x\" }\n";
    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, helpers_text).unwrap();

    let grammar_a_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"a\" }}\nrule program {{ shared_rule }}\n",
        helpers_path.display()
    );
    let grammar_a_path = dir.path().join("grammar_a.tsg");
    std::fs::write(&grammar_a_path, &grammar_a_text).unwrap();

    let grammar_b_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"b\" }}\nrule program {{ shared_rule }}\n",
        helpers_path.display()
    );
    let grammar_b_path = dir.path().join("grammar_b.tsg");
    std::fs::write(&grammar_b_path, &grammar_b_text).unwrap();

    let helpers_uri = Url::from_file_path(&helpers_path).unwrap();
    let grammar_a_uri = Url::from_file_path(&grammar_a_path).unwrap();
    let grammar_b_uri = Url::from_file_path(&grammar_b_path).unwrap();

    let mut service = init(&[
        (grammar_a_uri.clone(), &grammar_a_text),
        (grammar_b_uri.clone(), &grammar_b_text),
    ])
    .await;

    let _ = hover_at(&mut service, grammar_a_uri.clone(), Position::new(2, 17)).await;
    let _ = hover_at(&mut service, grammar_b_uri.clone(), Position::new(2, 17)).await;

    let offset = grammar_a_text.find("{ shared_rule }").unwrap() + "{ ".len();
    let rope = ropey::Rope::from_str(&grammar_a_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let mut result = references_at(&mut service, grammar_a_uri.clone(), pos, true)
        .await
        .expect("references should succeed");
    result.sort_by(|x, y| x.uri.as_str().cmp(y.uri.as_str()));

    let uris: Vec<&Url> = result.iter().map(|l| &l.uri).collect();
    assert!(
        uris.contains(&&helpers_uri)
            && uris.contains(&&grammar_a_uri)
            && uris.contains(&&grammar_b_uri),
        "refs should span helper + both importers, got {uris:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rename_helper_rule_via_bare_name() {
    let dir = tempfile::tempdir().unwrap();

    let helpers_text = "rule shared_rule { \"x\" }\n";
    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, helpers_text).unwrap();

    let grammar_a_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"a\" }}\nrule program {{ shared_rule }}\n",
        helpers_path.display()
    );
    let grammar_a_path = dir.path().join("grammar_a.tsg");
    std::fs::write(&grammar_a_path, &grammar_a_text).unwrap();

    let grammar_b_text = format!(
        "let h = import(\"{}\")\ngrammar {{ language: \"b\" }}\nrule program {{ shared_rule }}\n",
        helpers_path.display()
    );
    let grammar_b_path = dir.path().join("grammar_b.tsg");
    std::fs::write(&grammar_b_path, &grammar_b_text).unwrap();

    let helpers_uri = Url::from_file_path(&helpers_path).unwrap();
    let grammar_a_uri = Url::from_file_path(&grammar_a_path).unwrap();
    let grammar_b_uri = Url::from_file_path(&grammar_b_path).unwrap();

    let mut service = init(&[
        (grammar_a_uri.clone(), &grammar_a_text),
        (grammar_b_uri.clone(), &grammar_b_text),
    ])
    .await;

    let _ = hover_at(&mut service, grammar_a_uri.clone(), Position::new(2, 17)).await;
    let _ = hover_at(&mut service, grammar_b_uri.clone(), Position::new(2, 17)).await;

    let offset = grammar_a_text.find("{ shared_rule }").unwrap() + "{ ".len();
    let rope = ropey::Rope::from_str(&grammar_a_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let edit = rename_at(&mut service, grammar_a_uri.clone(), pos, "renamed")
        .await
        .expect("rename should succeed");
    let changes = edit.changes.expect("changes present");
    let mut keys: Vec<&Url> = changes.keys().collect();
    keys.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(
        keys,
        vec![&grammar_a_uri, &grammar_b_uri, &helpers_uri],
        "rename should span helper + both importers"
    );
}

// ---------------------------------------------------------------------------
// `expect` forward-decls
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn goto_def_external_via_inherit() {
    // An external token declared in an inherited base grammar (via `expect`
    // plus the grammar's `externals:` list) resolves through `base::_ws`.
    let dir = tempfile::tempdir().unwrap();

    let base_text =
        "expect _ws\ngrammar { language: \"base\", externals: [_ws] }\nrule program { _ws }\n";
    let base_path = dir.path().join("base.tsg");
    std::fs::write(&base_path, base_text).unwrap();

    let derived_text = format!(
        "let base = inherit(\"{}\")\ngrammar {{ language: \"derived\", inherits: base, externals: [base::_ws] }}\noverride rule program {{ base::_ws }}\n",
        base_path.display()
    );
    let derived_path = dir.path().join("derived.tsg");
    std::fs::write(&derived_path, &derived_text).unwrap();

    let base_uri = Url::from_file_path(&base_path).unwrap();
    let derived_uri = Url::from_file_path(&derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &derived_text)]).await;

    let body_offset = derived_text.rfind("base::_ws").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, body_offset as u32);

    assert_eq!(
        goto_def_at(&mut service, derived_uri, pos).await,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: base_uri,
            range: Range::new(Position::new(0, 7), Position::new(0, 10)),
        }))
    );
}

// ---------------------------------------------------------------------------
// Cross-file rename tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn rename_base_rule_from_derived_file() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor on "_statement" in `base::_statement`.
    let base_stmt_offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, base_stmt_offset as u32);

    // prepare_rename should succeed for cross-file rename.
    let prep = prepare_rename_at(&mut service, derived_uri.clone(), pos).await;
    assert!(
        prep.is_some(),
        "prepare_rename should accept base rule access"
    );

    // Rename "_statement" to "stmt".
    let edit = rename_at(&mut service, derived_uri.clone(), pos, "stmt")
        .await
        .unwrap();
    let changes = edit.changes.unwrap();

    // Base file: definition of `_statement` + reference in `repeat(_statement)`.
    let base_edits = &changes[&base_uri];
    assert_eq!(base_edits.len(), 2);
    assert!(base_edits.iter().all(|e| e.new_text == "stmt"));

    // Derived file: the `base::_statement` reference.
    let derived_edits = &changes[&derived_uri];
    assert_eq!(derived_edits.len(), 1);
    assert_eq!(derived_edits[0].new_text, "stmt");
}

#[tokio::test(flavor = "current_thread")]
async fn rename_imported_function_from_importing_file() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();
    let helper_uri = Url::from_file_path(&fix.helper_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    // Cursor on "commaSep" in `helpers::commaSep("x")`.
    let comma_sep_offset = fix.grammar_text.find("helpers::commaSep").unwrap() + "helpers::".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, comma_sep_offset as u32);

    // prepare_rename should succeed for cross-file rename.
    let prep = prepare_rename_at(&mut service, grammar_uri.clone(), pos).await;
    assert!(
        prep.is_some(),
        "prepare_rename should accept import member access"
    );

    // Rename "commaSep" to "separated".
    let edit = rename_at(&mut service, grammar_uri.clone(), pos, "separated")
        .await
        .unwrap();
    let changes = edit.changes.unwrap();

    // Helper file: definition of `commaSep`.
    let helper_edits = &changes[&helper_uri];
    assert_eq!(helper_edits.len(), 1);
    assert_eq!(helper_edits[0].new_text, "separated");

    // Grammar file: the `helpers::commaSep` reference.
    let grammar_edits = &changes[&grammar_uri];
    assert_eq!(grammar_edits.len(), 1);
    assert_eq!(grammar_edits[0].new_text, "separated");
}

// ---------------------------------------------------------------------------
// Benchmarks (run with `cargo test --release -- --ignored --nocapture bench_`)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
#[ignore = "benchmark"]
async fn bench_hover_cpp_end_to_end() {
    let path = "/home/lillis/projects/grammars/tree-sitter-cpp/grammar.tsg";
    let Ok(text) = std::fs::read_to_string(path) else {
        eprintln!("skipping: cpp grammar not found at {path}");
        return;
    };
    let uri = Url::from_file_path(path).unwrap();
    let mut service = init(&[(uri.clone(), &text)]).await;

    // Cursor on the first occurrence of an identifier.
    let rope = ropey::Rope::from_str(&text);
    let some_offset = text.find("rule").unwrap() as u32;
    let pos = ts_grammar_ls::text::offset_to_position(&rope, some_offset);

    // Warm up.
    for _ in 0..20 {
        let _ = hover_at(&mut service, uri.clone(), pos).await;
    }

    let n = 200u32;

    // Cache hit: same version, repeated hovers.
    let start = std::time::Instant::now();
    for _ in 0..n {
        std::hint::black_box(hover_at(&mut service, uri.clone(), pos).await);
    }
    let hit_time = start.elapsed() / n;

    // did_change only: measures keystroke-handling overhead alone.
    let start = std::time::Instant::now();
    for i in 0..n {
        lsp_notify::<DidChangeTextDocument>(
            &mut service,
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: 1000 + i as i32,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.clone(),
                }],
            },
        )
        .await;
    }
    let change_only_time = start.elapsed() / n;

    // did_change + hover: each hover forces a fresh analyze (cache miss).
    let start = std::time::Instant::now();
    for i in 0..n {
        lsp_notify::<DidChangeTextDocument>(
            &mut service,
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: 2000 + i as i32,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.clone(),
                }],
            },
        )
        .await;
        std::hint::black_box(hover_at(&mut service, uri.clone(), pos).await);
    }
    let miss_time = start.elapsed() / n;

    eprintln!(
        "=== End-to-end LSP hover on cpp grammar ({} lines) ===",
        text.lines().count()
    );
    eprintln!("Cache hit (no change):                {hit_time:>10?}");
    eprintln!("did_change only (no hover):           {change_only_time:>10?}");
    eprintln!("did_change + hover (full re-analyze): {miss_time:>10?}");
    eprintln!(
        "Implied analyze + hover handler cost: {:>10?}",
        miss_time.saturating_sub(change_only_time)
    );
}

// ---------------------------------------------------------------------------
// Cfg attribute support (#[cfg(NAME)]) - dim/hint diagnostics, hover,
// completion. Matches the rust-analyzer treatment for conditionally compiled
// code: disabled regions render dimmed with a hover message.
// ---------------------------------------------------------------------------

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn cfg_disabled_rule_produces_hint_diagnostic() {
    // `GFM` flag is declared but not enabled, so the `strikethrough` rule
    // gets gated off. The LSP should surface a HINT+UNNECESSARY diagnostic
    // over the `#[cfg(GFM)] rule strikethrough { "~~" }` range so editors
    // render it dimmed.
    let grammar = r#"grammar { language: "test", flags: { disabled: ["GFM"] } }
rule program { "x" }
#[cfg(GFM)]
rule strikethrough { "~~" }
"#;
    let service = init(&[(test_uri(), grammar)]).await;
    let doc = service.inner().document_map.get(&test_uri()).unwrap();

    // No hard errors - the cfg-gated rule isn't referenced.
    let hints: Vec<&Diagnostic> = doc
        .dsl_diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::HINT))
        .collect();
    assert_eq!(hints.len(), 1);
    let h = hints[0];
    assert_eq!(h.severity, Some(DiagnosticSeverity::HINT));
    assert_eq!(h.tags.as_deref(), Some(&[DiagnosticTag::UNNECESSARY][..]));
    assert_eq!(
        h.message,
        "disabled by cfg flag `GFM` (currently off)".to_string()
    );
    // Range covers the full `#[cfg(GFM)] rule strikethrough { "~~" }` block.
    assert_eq!(h.range.start, Position::new(2, 0));
    assert_eq!(h.range.end, Position::new(3, 27));
}

#[tokio::test(flavor = "current_thread")]
async fn unreachable_rule_produces_hint_diagnostic() {
    // `unused_rule` is declared but never referenced from `program` (the
    // start rule). `InputGrammar::normalize()` drops it; the LSP surfaces a
    // HINT+UNNECESSARY diagnostic on its declaration so editors render it
    // dimmed.
    let grammar = r#"grammar { language: "test" }
rule program { "x" }
rule unused_rule { "y" }
"#;
    let service = init(&[(test_uri(), grammar)]).await;
    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    let hints: Vec<Diagnostic> = doc
        .dsl_diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::HINT))
        .cloned()
        .collect();
    assert_eq!(
        hints,
        vec![Diagnostic {
            range: Range::new(Position::new(2, 0), Position::new(2, 24)),
            severity: Some(DiagnosticSeverity::HINT),
            source: Some("ts_grammar_ls".into()),
            message: "unused rule `unused_rule`".into(),
            tags: Some(vec![DiagnosticTag::UNNECESSARY]),
            ..Default::default()
        }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reachable_rule_produces_no_unused_hint() {
    // `helper` is reachable from `program` via `seq` - no unused diagnostic.
    let grammar = r#"grammar { language: "test" }
rule program { seq("x", helper) }
rule helper { "y" }
"#;
    let service = init(&[(test_uri(), grammar)]).await;
    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    let unused_hints: Vec<&Diagnostic> = doc
        .dsl_diagnostics
        .iter()
        .filter(|d| {
            d.severity == Some(DiagnosticSeverity::HINT) && d.message.starts_with("unused rule")
        })
        .collect();
    assert!(
        unused_hints.is_empty(),
        "unexpected hints: {unused_hints:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_cfg_flag_name_shows_disabled_state() {
    let grammar = r#"grammar { language: "test", flags: { disabled: ["GFM"] } }
rule program { "x" }
#[cfg(GFM)]
rule strikethrough { "~~" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on `GFM` inside `#[cfg(GFM)]` (line 2, col 6).
    let result = hover_at(&mut service, test_uri(), Position::new(2, 6)).await;
    assert_eq!(result, make_hover("```\ncfg flag `GFM` (disabled)\n```"));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_cfg_flag_name_shows_enabled_state() {
    let grammar = r#"grammar { language: "test", flags: { enabled: ["GFM"] } }
#[cfg(GFM)]
rule program { "x" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on `GFM` inside `#[cfg(GFM)]` (line 1, col 6).
    let result = hover_at(&mut service, test_uri(), Position::new(1, 6)).await;
    assert_eq!(result, make_hover("```\ncfg flag `GFM` (enabled)\n```"));
}

#[tokio::test(flavor = "current_thread")]
async fn completion_inside_cfg_lists_declared_flags() {
    // Grammar must parse cleanly so the loader records the flag registry.
    // Cursor lands at the start of `GFM` (just after the opening paren).
    let grammar = r#"grammar { language: "test", flags: { enabled: ["GFM"], disabled: ["TABLES"] } }
#[cfg(GFM)]
rule program { "x" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor inside `#[cfg(|GFM)]` (line 1, col 6).
    let items = completions_at(&mut service, test_uri(), Position::new(1, 6)).await;
    assert_eq!(
        items,
        vec![
            CompletionItem {
                label: "GFM".into(),
                kind: Some(CompletionItemKind::CONSTANT),
                detail: Some("cfg flag (enabled)".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "TABLES".into(),
                kind: Some(CompletionItemKind::CONSTANT),
                detail: Some("cfg flag (disabled)".into()),
                ..Default::default()
            },
        ]
    );
}

use std::fs::DirEntry;
use std::path::Path;

use ts_grammar_ls::config::FormattingConfig;
use ts_grammar_ls::formatter;

fn corpus_dir() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/corpus/formatting"
    ))
}

#[ignore = "formatter under construction; un-ignore once full corpus passes"]
#[test]
fn formatting_corpus() {
    let config = FormattingConfig::default();
    let mut failures = Vec::new();

    let mut entries: Vec<DirEntry> = std::fs::read_dir(corpus_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(DirEntry::file_name);

    assert!(!entries.is_empty(), "no corpus entries found");

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let dir = entry.path();

        let before_path = dir.join("before.tsg");
        let after_path = dir.join("after.tsg");

        let before = match std::fs::read_to_string(&before_path) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{name}: failed to read before.tsg: {e}"));
                continue;
            }
        };
        let expected = match std::fs::read_to_string(&after_path) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{name}: failed to read after.tsg: {e}"));
                continue;
            }
        };

        // Test: before -> after
        match formatter::format(&before, &entry.path(), &config) {
            None => {
                failures.push(format!("{name}: failed to parse before.tsg"));
            }
            Some(formatted) if formatted != expected => {
                failures.push(format!(
                    "{name}: formatted output differs from after.tsg\n--- expected ---\n{expected}\n--- got ---\n{formatted}"
                ));
            }
            Some(_) => {}
        }

        // Test: after -> after (idempotency)
        match formatter::format(&expected, &after_path, &config) {
            None => {
                failures.push(format!("{name}: failed to parse after.tsg (idempotency)"));
            }
            Some(reformatted) if reformatted != expected => {
                failures.push(format!(
                    "{name}: formatting is not idempotent\n--- expected ---\n{expected}\n--- got ---\n{reformatted}"
                ));
            }
            Some(_) => {}
        }
    }

    assert!(
        failures.is_empty(),
        "{} corpus test(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

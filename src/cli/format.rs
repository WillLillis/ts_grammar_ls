use std::path::{Path, PathBuf};

use crate::config;
use crate::formatter;

/// Exit code when the run succeeded (including when no `.tsg` files were found).
pub const EXIT_OK: i32 = 0;
/// Exit code when `--check` found files that would be reformatted.
pub const EXIT_DIFF: i32 = 1;
/// Exit code when one or more files could not be read, written, or parsed.
pub const EXIT_ERROR: i32 = 2;

/// Run the format CLI command. Returns the exit code.
#[must_use]
pub fn run(paths: &[PathBuf], check: bool, config_path: Option<&Path>) -> i32 {
    let config = config::load_config(config_path, None);
    let files = collect_tsg_files(paths);

    if files.is_empty() {
        eprintln!("No .tsg files found");
        return EXIT_OK;
    }

    let mut has_diff = false;
    let mut has_error = false;

    for file in &files {
        let source = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {e}", file.display());
                has_error = true;
                continue;
            }
        };

        let Some(formatted) = formatter::format(&source, file, &config.formatting) else {
            eprintln!("{}: failed to parse, skipping", file.display());
            has_error = true;
            continue;
        };

        if source == formatted {
            continue;
        }

        if check {
            eprintln!("{}: would reformat", file.display());
            has_diff = true;
        } else if let Err(e) = std::fs::write(file, &formatted) {
            eprintln!("{}: {e}", file.display());
            has_error = true;
        } else {
            eprintln!("{}: formatted", file.display());
        }
    }

    if has_error {
        EXIT_ERROR
    } else if has_diff {
        EXIT_DIFF
    } else {
        EXIT_OK
    }
}

fn collect_tsg_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            collect_tsg_files_recursive(path, &mut files);
        } else if path.extension().is_some_and(|e| e == "tsg") {
            files.push(path.clone());
        } else {
            eprintln!("{}: not a .tsg file, skipping", path.display());
        }
    }
    files
}

fn collect_tsg_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_tsg_files_recursive(&path, files);
        } else if path.extension().is_some_and(|e| e == "tsg") {
            files.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formatted .tsg snippet. Running the formatter on this produces
    /// byte-identical output.
    const CLEAN: &str = "grammar {\n    language: \"test\",\n}\n\nrule program { \"x\" }\n";

    /// Same grammar as CLEAN but with extra whitespace/newlines that the
    /// formatter will normalize.
    const DIRTY: &str = "grammar{ language:\"test\" }\nrule program{\"x\"}\n";

    #[test]
    fn format_writes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grammar.tsg");
        std::fs::write(&path, DIRTY).unwrap();

        let exit = run(std::slice::from_ref(&path), false, None);

        assert_eq!(exit, EXIT_OK);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CLEAN);
    }

    #[test]
    fn check_clean_file_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grammar.tsg");
        std::fs::write(&path, CLEAN).unwrap();

        let exit = run(std::slice::from_ref(&path), true, None);

        assert_eq!(exit, EXIT_OK);
        // File is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CLEAN);
    }

    #[test]
    fn check_dirty_file_exits_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grammar.tsg");
        std::fs::write(&path, DIRTY).unwrap();

        let exit = run(std::slice::from_ref(&path), true, None);

        assert_eq!(exit, EXIT_DIFF);
        // File is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DIRTY);
    }

    #[test]
    fn unparseable_file_exits_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grammar.tsg");
        let bad = "grammar { this is { not valid syntax";
        std::fs::write(&path, bad).unwrap();

        let exit = run(std::slice::from_ref(&path), false, None);

        assert_eq!(exit, EXIT_ERROR);
        // File is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
    }

    #[test]
    fn directory_collects_tsg_files_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();

        let root_tsg = dir.path().join("root.tsg");
        let nested_tsg = nested.join("nested.tsg");
        let other = dir.path().join("readme.md");

        std::fs::write(&root_tsg, DIRTY).unwrap();
        std::fs::write(&nested_tsg, DIRTY).unwrap();
        std::fs::write(&other, "not a grammar").unwrap();

        let exit = run(&[dir.path().to_path_buf()], false, None);

        assert_eq!(exit, EXIT_OK);
        assert_eq!(std::fs::read_to_string(&root_tsg).unwrap(), CLEAN);
        assert_eq!(std::fs::read_to_string(&nested_tsg).unwrap(), CLEAN);
        // Non-.tsg file is untouched.
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "not a grammar");
    }

    #[test]
    fn empty_paths_exits_ok() {
        // No files means nothing to do - success, not an error.
        let exit = run(&[], false, None);
        assert_eq!(exit, EXIT_OK);
    }

    #[test]
    fn non_tsg_extension_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grammar.txt");
        std::fs::write(&path, DIRTY).unwrap();

        let exit = run(std::slice::from_ref(&path), false, None);

        // No .tsg files found (the .txt was skipped at collection time).
        assert_eq!(exit, EXIT_OK);
        // File is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DIRTY);
    }

    #[test]
    fn check_dirty_in_directory_exits_one() {
        let dir = tempfile::tempdir().unwrap();
        let clean_path = dir.path().join("clean.tsg");
        let dirty_path = dir.path().join("dirty.tsg");
        std::fs::write(&clean_path, CLEAN).unwrap();
        std::fs::write(&dirty_path, DIRTY).unwrap();

        let exit = run(&[dir.path().to_path_buf()], true, None);

        assert_eq!(exit, EXIT_DIFF);
        // Neither file is modified in check mode.
        assert_eq!(std::fs::read_to_string(&clean_path).unwrap(), CLEAN);
        assert_eq!(std::fs::read_to_string(&dirty_path).unwrap(), DIRTY);
    }
}

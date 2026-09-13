//! Run the formatter against one file or a tree of `.tsg` files. Reports
//! parse failures, idempotency violations, and lines over the width budget.
//!
//! Usage:
//!   cargo run --example check_format -- <path>
//!   cargo run --example check_format -- <path> --diff       # single file
//!   cargo run --example check_format -- <dir> --recursive

use std::path::{Path, PathBuf};

use ts_grammar_ls::config::FormattingConfig;
use ts_grammar_ls::formatter;

#[derive(Default)]
struct Tally {
    files: usize,
    parse_fail: Vec<PathBuf>,
    not_idempotent: Vec<PathBuf>,
    overlong: Vec<(PathBuf, usize)>, // (path, num_lines_over)
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: check_format <path> [--diff] [--recursive]");
        std::process::exit(2);
    }
    let recursive = args.iter().any(|a| a == "--recursive");
    let show_diff = args.iter().any(|a| a == "--diff");
    args.retain(|a| !a.starts_with("--"));
    let root = PathBuf::from(args.remove(0));

    let cfg = FormattingConfig::default();
    let mut tally = Tally::default();

    if recursive {
        let mut stack = vec![root];
        while let Some(p) = stack.pop() {
            if p.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&p) {
                    for e in entries.flatten() {
                        stack.push(e.path());
                    }
                }
            } else if p.extension().and_then(|e| e.to_str()) == Some("tsg") {
                check_one(&p, &cfg, &mut tally, false);
            }
        }
    } else {
        check_one(&root, &cfg, &mut tally, show_diff);
    }

    println!("\n=== summary ===");
    println!("files checked:     {}", tally.files);
    println!("parse-fail (1st):  {}", tally.parse_fail.len());
    println!("not idempotent:    {}", tally.not_idempotent.len());
    println!("with overlong:     {}", tally.overlong.len());
    for p in &tally.parse_fail {
        println!("  parse-fail: {}", p.display());
    }
    for p in &tally.not_idempotent {
        println!("  not idempotent: {}", p.display());
    }
}

fn check_one(path: &Path, cfg: &FormattingConfig, tally: &mut Tally, show_diff: bool) {
    let Ok(source) = std::fs::read_to_string(path) else {
        return;
    };
    tally.files += 1;
    let label = path.display();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        formatter::format(&source, path, cfg)
    }));
    let Some(formatted) = (match result {
        Ok(opt) => opt,
        Err(_) => {
            println!("[PANIC]        {label}");
            tally.parse_fail.push(path.to_owned());
            return;
        }
    }) else {
        println!("[FAIL parse]   {label}");
        tally.parse_fail.push(path.to_owned());
        return;
    };

    let Some(twice) = formatter::format(&formatted, path, cfg) else {
        println!("[FAIL reparse] {label}");
        std::fs::write("/tmp/last_formatted.tsg", &formatted).ok();
        tally.not_idempotent.push(path.to_owned());
        return;
    };
    if twice != formatted {
        println!("[FAIL idempot] {label}");
        tally.not_idempotent.push(path.to_owned());
        return;
    }

    let max = cfg.max_line_width;
    let over: Vec<(usize, usize)> = formatted
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let n = l.chars().count();
            (n > max).then_some((i + 1, n))
        })
        .collect();
    if over.is_empty() {
        println!(
            "[OK]           {label}  ({} lines)",
            formatted.lines().count()
        );
    } else {
        println!(
            "[OK*]          {label}  ({} lines, {} over budget)",
            formatted.lines().count(),
            over.len()
        );
        tally.overlong.push((path.to_owned(), over.len()));
    }

    if show_diff {
        show_unified_diff(&source, &formatted);
    }
}

fn show_unified_diff(a: &str, b: &str) {
    let a_lines: Vec<&str> = a.lines().collect();
    let b_lines: Vec<&str> = b.lines().collect();
    let max = a_lines.len().max(b_lines.len());
    for i in 0..max {
        let a = a_lines.get(i).copied().unwrap_or("");
        let b = b_lines.get(i).copied().unwrap_or("");
        if a != b {
            if !a.is_empty() {
                println!("-{a}");
            }
            if !b.is_empty() {
                println!("+{b}");
            }
        }
    }
}

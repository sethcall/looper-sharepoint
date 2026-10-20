//! Every command line in the docs and in the help's examples still parses: `specs/onboarding-design.md` §9. A
//! renamed flag or a changed argument fails here, not for the person (or assistant) following the docs.
//!
//! Each line is run with `--help` added: clap rejects an unknown flag or an extra argument before it gets to `--help`,
//! and nothing is ever executed.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_looper-sharepoint");

fn help(args: &[&str]) -> String {
    let out = Command::new(BIN).args(args).arg("--help").output().unwrap();
    assert!(out.status.success(), "`looper-sharepoint {} --help` failed:\n{}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Shell-like words: quotes group, `<placeholder words>` stay one word, ` #` or two spaces (the help's description
/// column) end the command.
fn words(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote, mut angle) = (vec![], String::new(), None, false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => quote = Some(c),
            (None, '<') => {
                angle = true;
                cur.push(c);
            }
            (None, '>') => {
                angle = false;
                cur.push(c);
            }
            (None, '#') if !angle && cur.is_empty() => break,
            (None, ' ') if !angle && chars.peek() == Some(&' ') => break,
            (None, ' ') if !angle => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The `looper-sharepoint …` lines of `text` (in code blocks, or indented in help), without the program name.
fn command_lines(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .map(str::trim)
        .map(|l| l.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.').trim_start())
        .filter_map(|l| l.strip_prefix("looper-sharepoint ").or_else(|| l.strip_prefix("./target/release/looper-sharepoint ")))
        .map(words)
        .filter(|w| !w.is_empty())
        .collect()
}

fn check(source: &str, lines: Vec<Vec<String>>) -> usize {
    for w in &lines {
        let args: Vec<&str> = w.iter().map(String::as_str).collect();
        let out = Command::new(BIN).args(&args).arg("--help").output().unwrap();
        assert!(
            out.status.success(),
            "{source}: `looper-sharepoint {}` doesn't parse:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    lines.len()
}

#[test]
fn docs_and_help_examples_parse() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut n = 0;
    for doc in ["README.md", "docs/QUICKSTART.md", "docs/user-guide.md"] {
        let text = std::fs::read_to_string(root.join(doc)).unwrap_or_else(|e| panic!("{doc}: {e}"));
        // Only code blocks: prose mentions commands with placeholders mid-sentence.
        let mut code = String::new();
        let mut inside = false;
        for l in text.lines() {
            if l.trim_start().starts_with("```") {
                inside = !inside;
            } else if inside {
                code.push_str(l);
                code.push('\n');
            }
        }
        n += check(doc, command_lines(&code));
    }
    // The top-level help (its setup steps) and every command's examples.
    let top = help(&[]);
    n += check("--help", command_lines(&top));
    let commands: Vec<String> = top
        .split("Commands:")
        .nth(1)
        .unwrap()
        .lines()
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .filter(|c| c != "help")
        .collect();
    for c in &commands {
        let h = help(&[c]);
        n += check(&format!("{c} --help"), command_lines(h.split("Examples:").nth(1).unwrap_or("")));
        if let Some(subs) = h.split("Commands:").nth(1) {
            for s in subs.lines().skip(1).take_while(|l| !l.trim().is_empty()).filter_map(|l| l.split_whitespace().next()).filter(|s| *s != "help") {
                let h = help(&[c, s]);
                n += check(&format!("{c} {s} --help"), command_lines(h.split("Examples:").nth(1).unwrap_or("")));
            }
        }
    }
    assert!(n > 40, "only {n} command lines found: is the extraction broken?");
}

#[test]
fn help_starts_with_the_setup_steps() {
    let top = help(&[]);
    let steps = top.find("Set up (once):").expect("setup steps in --help");
    assert!(steps < top.find("Commands:").unwrap(), "the steps come before the command list");
    for c in ["setup <file or link>", "login", "add <SharePoint link> --to <folder>", "start-at-login on"] {
        assert!(top.contains(&format!("looper-sharepoint {c}")), "--help names `{c}`");
    }
    // No arguments at all shows the same help.
    let out = Command::new(BIN).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("Set up (once):") || String::from_utf8_lossy(&out.stdout).contains("Set up (once):"));
}

#[test]
fn words_split_like_a_shell() {
    assert_eq!(words(r#"add "a b" --to ~/x   # note"#), ["add", "a b", "--to", "~/x"]);
    assert_eq!(words("setup <file or link> --yes"), ["setup", "<file or link>", "--yes"]);
    assert_eq!(words(r"add a\ b"), ["add", "a b"]);
    assert_eq!(words("login                   sign in"), ["login"]);
}

//! `cargo xtask coverage`: measure production line coverage against a floor.
//!
//! Runs `cargo llvm-cov --workspace --all-features --lcov`, then keeps only
//! production lines: test/bench/example targets and inline `#[cfg(test)]`
//! modules are excluded, so the number reflects the library code, not the test
//! code that exercises it.
//!
//! ```text
//! cargo xtask coverage                 # run the suite, fail below 95%
//! cargo xtask coverage --min 90        # lower the floor
//! cargo xtask coverage --lcov cov.info # reuse an existing lcov file
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_MIN: f64 = 95.0;
const DEFAULT_LCOV: &str = "target/cov/coverage.info";

pub fn run(root: &Path, args: &[String]) -> i32 {
    let mut min = DEFAULT_MIN;
    let mut lcov: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--min" => match args.get(i + 1).and_then(|s| s.parse::<f64>().ok()) {
                Some(value) if (0.0..=100.0).contains(&value) => {
                    min = value;
                    i += 1;
                }
                _ => {
                    eprintln!("coverage: --min needs a number between 0 and 100");
                    return 2;
                }
            },
            "--lcov" => match args.get(i + 1) {
                Some(path) => {
                    lcov = Some(PathBuf::from(path));
                    i += 1;
                }
                None => {
                    eprintln!("coverage: --lcov needs a path");
                    return 2;
                }
            },
            other => {
                eprintln!("coverage: unknown argument {other}");
                return 2;
            }
        }
        i += 1;
    }

    let path = lcov.clone().unwrap_or_else(|| root.join(DEFAULT_LCOV));
    if lcov.is_none() {
        if let Err(error) = run_llvm_cov(&path) {
            eprintln!("coverage: {error}");
            return 1;
        }
    }
    let report = match report_for(root, &path) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("coverage: {error}");
            return 1;
        }
    };
    report.print(min);
    if report.percent() + f64::EPSILON < min {
        eprintln!(
            "coverage: {:.2}% is below the {min:.2}% floor ({} uncovered lines)",
            report.percent(),
            report.lines - report.covered
        );
        1
    } else {
        0
    }
}

fn run_llvm_cov(output: &Path) -> Result<(), String> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let status = Command::new("cargo")
        .args([
            "llvm-cov",
            "--workspace",
            "--all-features",
            "--lcov",
            "--output-path",
        ])
        .arg(output)
        .status()
        .map_err(|e| {
            format!(
                "cannot run cargo-llvm-cov ({e}); install it with `cargo install cargo-llvm-cov --locked`"
            )
        })?;
    if !status.success() {
        return Err("cargo llvm-cov failed".to_string());
    }
    Ok(())
}

#[derive(Default)]
struct Totals {
    lines: usize,
    covered: usize,
    by_crate: BTreeMap<String, Totals>,
}

impl Totals {
    fn percent(&self) -> f64 {
        100.0 * self.covered as f64 / self.lines.max(1) as f64
    }

    fn print(&self, min: f64) {
        println!(
            "{:<34} {:>8} {:>8} {:>8}",
            "crate / area", "covered", "lines", "percent"
        );
        for (name, totals) in &self.by_crate {
            println!(
                "{:<34} {:>8} {:>8} {:>7.2}%",
                name,
                totals.covered,
                totals.lines,
                totals.percent()
            );
        }
        println!(
            "{:<34} {:>8} {:>8} {:>7.2}%  (floor {min:.2}%)",
            "TOTAL production",
            self.covered,
            self.lines,
            self.percent()
        );
    }
}

fn report_for(root: &Path, lcov: &Path) -> Result<Totals, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve workspace root: {error}"))?;
    let text = std::fs::read_to_string(lcov)
        .map_err(|e| format!("cannot read {}: {e}", lcov.display()))?;
    let mut report = Totals::default();
    let mut file: Option<(PathBuf, BTreeMap<u32, bool>)> = None;
    let mut records: BTreeMap<PathBuf, BTreeMap<u32, bool>> = BTreeMap::new();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            if file.is_some() {
                return Err("LCOV record is missing end_of_record".into());
            }
            if path.trim().is_empty() {
                return Err("LCOV record has an empty source path".into());
            }
            file = Some((PathBuf::from(path), BTreeMap::new()));
        } else if let Some(da) = line.strip_prefix("DA:") {
            let Some((_, lines)) = file.as_mut() else {
                return Err("LCOV DA appears outside a record".into());
            };
            let mut parts = da.split(',');
            let number = parts
                .next()
                .ok_or("LCOV DA has no line number")?
                .parse::<u32>()
                .map_err(|_| "LCOV DA has an invalid line number")?;
            let count = parts
                .next()
                .ok_or("LCOV DA has no execution count")?
                .parse::<f64>()
                .map_err(|_| "LCOV DA has an invalid execution count")?;
            if !count.is_finite() || count < 0.0 {
                return Err("LCOV DA has an invalid execution count".into());
            }
            lines
                .entry(number)
                .and_modify(|covered| *covered |= count > 0.0)
                .or_insert(count > 0.0);
        } else if line == "end_of_record" {
            let Some((path, lines)) = file.take() else {
                return Err("LCOV end_of_record without SF".into());
            };
            let absolute = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            let source = absolute.canonicalize().unwrap_or(absolute);
            let merged = records.entry(source).or_default();
            for (number, covered) in lines {
                merged
                    .entry(number)
                    .and_modify(|old| *old |= covered)
                    .or_insert(covered);
            }
        }
    }
    if file.is_some() {
        return Err("LCOV record is missing end_of_record".into());
    }
    if records.is_empty() {
        return Err("LCOV contains no complete records".into());
    }
    for (path, lines) in records {
        add_file(&root, &mut report, &path, &lines)?;
    }
    if report.lines == 0 {
        return Err("LCOV contains no production lines".into());
    }
    Ok(report)
}

fn add_file(
    root: &Path,
    report: &mut Totals,
    path: &Path,
    lines: &BTreeMap<u32, bool>,
) -> Result<(), String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let Ok(relative) = absolute.strip_prefix(root) else {
        return Ok(());
    };
    let Some(relative) = relative.to_str() else {
        return Ok(());
    };
    let relative = relative.replace('\\', "/");
    let parts: Vec<&str> = relative.split('/').collect();
    if parts
        .iter()
        .any(|p| matches!(*p, "tests" | "benches" | "examples"))
    {
        return Ok(());
    }
    if parts.first() != Some(&"crates") || parts.len() < 2 {
        return Ok(()); // xtask and any out-of-workspace file are tooling
    }
    let source = std::fs::read_to_string(&absolute)
        .map_err(|error| format!("cannot read source {}: {error}", absolute.display()))?;
    let source_lines = source.lines().count();
    if lines
        .keys()
        .any(|line| *line == 0 || *line as usize > source_lines)
    {
        return Err(format!("LCOV line outside source {}", absolute.display()));
    }
    let excluded = excluded_test_lines(&absolute);
    let mut totals = Totals::default();
    for (number, covered) in lines {
        if excluded.contains(number) {
            continue;
        }
        totals.lines += 1;
        if *covered {
            totals.covered += 1;
        }
    }
    if totals.lines == 0 {
        return Ok(()); // generated tables have no executable lines
    }
    report.lines += totals.lines;
    report.covered += totals.covered;
    let name = format!("{}/{}", parts[0], parts[1]);
    let entry = report.by_crate.entry(name).or_default();
    entry.lines += totals.lines;
    entry.covered += totals.covered;
    Ok(())
}

/// Line numbers inside `#[cfg(test)]` items, so test code does not inflate the
/// production figure. Handles modules, functions and `use`/`static` items; the
/// brace matcher ignores comments and strings well enough for this purpose.
fn excluded_test_lines(path: &Path) -> std::collections::BTreeSet<u32> {
    let mut excluded = std::collections::BTreeSet::new();
    let Ok(source) = std::fs::read_to_string(path) else {
        return excluded;
    };
    let syntax = rust_syntax(&source);
    let code_lines: Vec<&str> = syntax.lines().collect();
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if code_lines[i].trim() != "#[cfg(test)]" {
            i += 1;
            continue;
        }
        let start = i;
        let mut item = i + 1;
        while item < lines.len() {
            let text = code_lines[item].trim();
            if text.starts_with("#[") || text.starts_with("//") || text.is_empty() {
                item += 1;
            } else {
                break;
            }
        }
        let mut depth = 0i32;
        let mut opened = false;
        let mut end = item;
        let mut current = item;
        while current < lines.len() {
            for ch in code_lines[current].chars() {
                match ch {
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            end = current;
            if opened && depth <= 0 {
                break;
            }
            if !opened && code_lines[current].contains(';') {
                break;
            }
            current += 1;
        }
        for number in start..=end {
            excluded.insert(number as u32 + 1);
        }
        i = end + 1;
    }
    excluded
}

/// Blanks literals and comments while retaining line numbers and item delimiters.
fn rust_syntax(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut syntax = bytes.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if bytes[i] == b'r' && {
            let mut quote = i + 1;
            while quote < bytes.len() && bytes[quote] == b'#' {
                quote += 1;
            }
            bytes.get(quote) == Some(&b'"')
        } {
            let mut quote = i + 1;
            while bytes[quote] == b'#' {
                quote += 1;
            }
            let hashes = quote - i - 1;
            i = quote + 1;
            while i < bytes.len() {
                if bytes[i] == b'"'
                    && bytes
                        .get(i + 1..i + 1 + hashes)
                        .is_some_and(|end| end.iter().all(|byte| *byte == b'#'))
                {
                    i += 1 + hashes;
                    break;
                }
                i += 1;
            }
        } else if bytes[i] == b'"'
            || (bytes[i] == b'\'' && {
                let next = source.get(i + 1..).and_then(|text| text.chars().next());
                next == Some('\\')
                    || next.is_some_and(|ch| bytes.get(i + 1 + ch.len_utf8()) == Some(&b'\''))
            })
        {
            let delimiter = bytes[i];
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == delimiter {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
        } else {
            i += 1;
            continue;
        }
        for byte in &mut syntax[start..i] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    // Every non-ASCII byte belongs to an unchanged identifier or a blanked literal.
    String::from_utf8(syntax).expect("blanking literals preserves UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcov(entries: &[(&str, &[u32], &[u32])]) -> String {
        let mut text = String::new();
        for (path, covered, uncovered) in entries {
            text.push_str(&format!("SF:{path}\n"));
            for line in *covered {
                text.push_str(&format!("DA:{line},1\n"));
            }
            for line in *uncovered {
                text.push_str(&format!("DA:{line},0\n"));
            }
            text.push_str("end_of_record\n");
        }
        text
    }

    #[test]
    fn test_modules_are_excluded_from_the_line_count() {
        let dir = std::env::temp_dir().join(format!("radicom-cov-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crates/demo/src")).unwrap();
        let file = dir.join("crates/demo/src/lib.rs");
        std::fs::write(
            &file,
            "pub fn a() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() {}\n\
             }\n\
             pub fn b() {}\n",
        )
        .unwrap();
        let excluded = excluded_test_lines(&file);
        assert!(excluded.contains(&2), "attribute line is test code");
        assert!(excluded.contains(&4), "the test function is test code");
        assert!(!excluded.contains(&1) && !excluded.contains(&8));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn braces_in_comments_strings_and_characters_do_not_hide_production_lines() {
        let dir = std::env::temp_dir().join(format!("radicom-cov-braces-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lib.rs");
        std::fs::write(&path, "#[cfg(test)]\nmod tests {\nlet s = \"{\"; // {\nlet c = '{'; /* { */\nlet r = r#\"{\"#;\n}\npub fn production() {}\n").unwrap();
        let excluded = excluded_test_lines(&path);
        assert!(excluded.contains(&6));
        assert!(!excluded.contains(&7));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_test_attribute_in_a_comment_or_raw_string_does_not_hide_production() {
        let dir = std::env::temp_dir().join(format!("radicom-cov-marker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lib.rs");
        for source in [
            "/*\n#[cfg(test)]\n*/\npub fn uncovered() {}\npub fn covered() {}\n",
            "const DESCRIPTION: &str = r#\"\n#[cfg(test)]\n\"#;\npub fn uncovered() {}\npub fn covered() {}\n",
        ] {
            std::fs::write(&path, source).unwrap();
            let excluded = excluded_test_lines(&path);
            assert!(!excluded.contains(&4), "production line hidden by {source:?}");
            assert!(!excluded.contains(&5), "production line hidden by {source:?}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn report_excludes_tests_benches_and_xtask() {
        let dir = std::env::temp_dir().join(format!("radicom-cov-report-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crates/demo/src")).unwrap();
        std::fs::create_dir_all(dir.join("crates/demo/tests")).unwrap();
        std::fs::write(
            dir.join("crates/demo/src/lib.rs"),
            "pub fn a() {}\npub fn b() {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("crates/demo/tests/it.rs"), "fn t() {}\n").unwrap();
        let lcov_path = dir.join("coverage.info");
        std::fs::write(
            &lcov_path,
            lcov(&[
                ("crates/demo/src/lib.rs", &[1], &[2]),
                ("crates/demo/tests/it.rs", &[1], &[]),
                ("xtask/src/main.rs", &[], &[1]),
            ]),
        )
        .unwrap();

        let report = report_for(&dir, &lcov_path).unwrap();
        assert_eq!(report.lines, 2, "only production lines are counted");
        assert_eq!(report.covered, 1);
        assert_eq!(report.percent(), 50.0);
        assert_eq!(report.by_crate.len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn invalid_or_empty_reports_cannot_pass_the_gate() {
        let dir = std::env::temp_dir().join(format!("radicom-cov-invalid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("coverage.info");
        for text in [
            "",
            "SF:crates/demo/src/lib.rs\nDA:1,garbage\nend_of_record\n",
            "SF:crates/demo/src/lib.rs\nDA:1,1\n",
            "SF:xtask/src/main.rs\nDA:1,1\nend_of_record\n",
        ] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(
                run(
                    &dir,
                    &[
                        "--lcov".into(),
                        path.display().to_string(),
                        "--min".into(),
                        "0".into()
                    ]
                ),
                1,
                "{text:?}"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_lines_and_duplicate_records_cannot_inflate_coverage() {
        let dir = std::env::temp_dir().join(format!("radicom-cov-source-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crates/demo/src")).unwrap();
        let source = dir.join("crates/demo/src/lib.rs");
        std::fs::write(&source, "pub fn a() {}\npub fn b() {}\n").unwrap();
        let path = dir.join("coverage.info");
        std::fs::write(
            &path,
            lcov(&[
                ("crates/demo/src/lib.rs", &[1], &[2]),
                ("crates/demo/src/lib.rs", &[2], &[1]),
            ]),
        )
        .unwrap();
        let report = report_for(&dir, &path).unwrap();
        assert_eq!((report.covered, report.lines), (2, 2));
        let alias = source.display().to_string();
        std::fs::write(
            &path,
            lcov(&[("crates/demo/src/lib.rs", &[1], &[2]), (&alias, &[1], &[])]),
        )
        .unwrap();
        let report = report_for(&dir, &path).unwrap();
        assert_eq!(
            (report.covered, report.lines),
            (1, 2),
            "relative and absolute sources have one identity"
        );
        for line in [0, 3] {
            std::fs::write(&path, lcov(&[("crates/demo/src/lib.rs", &[line], &[])])).unwrap();
            assert!(report_for(&dir, &path).is_err());
        }
        std::fs::remove_file(source).unwrap();
        std::fs::write(&path, lcov(&[("crates/demo/src/lib.rs", &[1], &[])])).unwrap();
        assert!(report_for(&dir, &path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

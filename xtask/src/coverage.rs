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

    let path = lcov.unwrap_or_else(|| root.join(DEFAULT_LCOV));
    if !path.exists() {
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
        if self.lines == 0 {
            100.0
        } else {
            100.0 * self.covered as f64 / self.lines as f64
        }
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
    let text = std::fs::read_to_string(lcov)
        .map_err(|e| format!("cannot read {}: {e}", lcov.display()))?;
    let mut report = Totals::default();
    let mut file: Option<(PathBuf, BTreeMap<u32, bool>)> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            file = Some((PathBuf::from(path), BTreeMap::new()));
        } else if let Some(da) = line.strip_prefix("DA:") {
            if let Some((_, lines)) = file.as_mut() {
                let mut parts = da.split(',');
                let (Some(number), Some(count)) = (parts.next(), parts.next()) else {
                    continue;
                };
                let (Ok(number), Ok(count)) = (number.parse::<u32>(), count.parse::<f64>()) else {
                    continue;
                };
                lines.insert(number, count > 0.0);
            }
        } else if line == "end_of_record" {
            if let Some((path, lines)) = file.take() {
                add_file(root, &mut report, &path, &lines);
            }
        }
    }
    Ok(report)
}

fn add_file(root: &Path, report: &mut Totals, path: &Path, lines: &BTreeMap<u32, bool>) {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let Ok(relative) = absolute.strip_prefix(root) else {
        return;
    };
    let Some(relative) = relative.to_str() else {
        return;
    };
    let relative = relative.replace('\\', "/");
    let parts: Vec<&str> = relative.split('/').collect();
    if parts
        .iter()
        .any(|p| matches!(*p, "tests" | "benches" | "examples"))
    {
        return;
    }
    if parts.first() != Some(&"crates") || parts.len() < 2 {
        return; // xtask and any out-of-workspace file are tooling
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
        return; // generated tables have no executable lines
    }
    report.lines += totals.lines;
    report.covered += totals.covered;
    let name = format!("{}/{}", parts[0], parts[1]);
    let entry = report.by_crate.entry(name).or_default();
    entry.lines += totals.lines;
    entry.covered += totals.covered;
}

/// Line numbers inside `#[cfg(test)]` items, so test code does not inflate the
/// production figure. Handles modules, functions and `use`/`static` items; the
/// brace matcher ignores comments and strings well enough for this purpose.
fn excluded_test_lines(path: &Path) -> std::collections::BTreeSet<u32> {
    let mut excluded = std::collections::BTreeSet::new();
    let Ok(source) = std::fs::read_to_string(path) else {
        return excluded;
    };
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "#[cfg(test)]" {
            i += 1;
            continue;
        }
        let start = i;
        let mut item = i + 1;
        while item < lines.len() {
            let text = lines[item].trim();
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
            for ch in lines[current].chars() {
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
            if !opened && lines[current].contains(';') {
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
    fn an_empty_floor_passes_an_empty_report() {
        let totals = Totals::default();
        assert_eq!(totals.percent(), 100.0);
    }
}

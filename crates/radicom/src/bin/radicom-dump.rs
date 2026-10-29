//! `radicom-dump` — open a DICOM file and print its dataset (INF-07).
//!
//! The Rust equivalent of fo-dicom's `FO-DICOM.Dump` GUI, as a CLI. Uses the
//! INF-04 dump formatter in the `radicom` library. No dependency beyond std:
//! arguments are parsed by hand.
//!
//! Usage:
//!   radicom-dump `<file>` [--max-value N] [--max-binary N] [--no-private]

use radicom::dump::{dump_dataset, pixel_summary, DumpOptions};
use radicom::io::file::DicomFile;

fn main() {
    std::process::exit(run(std::env::args().skip(1)));
}

/// Runs the CLI with `args` (the process arguments after `argv[0]`) and returns
/// the process exit code. Kept separate from [`main`] so the argument handling
/// and the file path are testable without spawning a process.
fn run(args: impl IntoIterator<Item = String>) -> i32 {
    let args: Vec<String> = args.into_iter().collect();
    let mut path: Option<String> = None;
    let mut options = DumpOptions::default();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--max-value" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
                    options.max_value_chars = v;
                    i += 1;
                }
            }
            "--max-binary" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
                    options.max_binary_bytes = v;
                    i += 1;
                }
            }
            "--no-private" => options.show_private = false,
            "-h" | "--help" => {
                print_usage();
                return 0;
            }
            other => path = Some(other.to_string()),
        }
        i += 1;
    }

    let Some(path) = path else {
        print_usage();
        return 2;
    };

    let file = match DicomFile::open(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("radicom-dump: cannot open {path}: {error}");
            return 1;
        }
    };

    // file-format diagnostics (INF-07)
    println!("File: {path}");
    println!("Transfer syntax: {}", file.dataset.transfer_syntax().uid());
    // pixel-data summary, if present
    if let Some(summary) = pixel_summary(&file.dataset) {
        println!("Image: {}", summary.one_line());
    }
    println!("---");
    print!("{}", dump_dataset(&file.dataset, &options));
    0
}

fn print_usage() {
    eprintln!("usage: radicom-dump <file.dcm> [--max-value N] [--max-binary N] [--no-private]");
    eprintln!("  Dump a DICOM file's dataset (tag, VR, keyword, value) to the console.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use radicom::model::dataset::Dataset;
    use radicom::model::tag::Tag;
    use radicom::model::vr::Vr;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn sample_file(name: &str) -> std::path::PathBuf {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.2");
        ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5.6.7");
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "DOE^JANE");
        ds.insert_value(Tag::ROWS, 2u16);
        ds.insert_value(Tag::COLUMNS, 2u16);
        let path =
            std::env::temp_dir().join(format!("radicom-dump-{name}-{}.dcm", std::process::id()));
        DicomFile::from_dataset(ds)
            .save(&path)
            .expect("save sample");
        path
    }

    #[test]
    fn a_missing_path_prints_usage_and_exits_with_2() {
        assert_eq!(run(args(&[])), 2);
    }

    #[test]
    fn help_exits_with_0() {
        assert_eq!(run(args(&["--help"])), 0);
        assert_eq!(run(args(&["-h"])), 0);
    }

    #[test]
    fn an_unopenable_path_exits_with_1() {
        let missing = std::env::temp_dir().join("radicom-dump-does-not-exist.dcm");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(run(vec![missing.to_string_lossy().into_owned()]), 1);
    }

    #[test]
    fn a_valid_file_dumps_with_options_applied() {
        let path = sample_file("valid");
        let code = run(args(&[
            "--max-value",
            "10",
            "--max-binary",
            "4",
            "--no-private",
            &path.to_string_lossy(),
        ]));
        assert_eq!(code, 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalid_option_values_are_ignored() {
        let path = sample_file("bad-options");
        let code = run(vec![
            "--max-value".to_string(),
            "not-a-number".to_string(),
            "--max-binary".to_string(),
            "also-bad".to_string(),
            path.to_string_lossy().into_owned(),
        ]);
        assert_eq!(code, 0);
        let _ = std::fs::remove_file(path);
    }
}

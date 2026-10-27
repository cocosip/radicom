//! `radicom-dump` — open a DICOM file and print its dataset (INF-07).
//!
//! The Rust equivalent of fo-dicom's `FO-DICOM.Dump` GUI, as a CLI. Uses the
//! INF-04 dump formatter in the `radicom` library. No dependency beyond std:
//! arguments are parsed by hand.
//!
//! Usage:
//!   radicom-dump <file> [--max-value N] [--max-binary N] [--no-private]

use radicom::dump::{dump_dataset, pixel_summary, DumpOptions};
use radicom::io::file::DicomFile;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
                return;
            }
            other => path = Some(other.to_string()),
        }
        i += 1;
    }

    let Some(path) = path else {
        print_usage();
        std::process::exit(2);
    };

    let file = match DicomFile::open(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("radicom-dump: cannot open {path}: {error}");
            std::process::exit(1);
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
}

fn print_usage() {
    eprintln!("usage: radicom-dump <file.dcm> [--max-value N] [--max-binary N] [--no-private]");
    eprintln!("  Dump a DICOM file's dataset (tag, VR, keyword, value) to the console.");
}

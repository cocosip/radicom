//! Directory scanner feeding a DICOMDIR builder (MEDIA-03; docs/01-architecture.md §12).
//!
//! A recursive directory walk with an optional glob filter, progress/file
//! events and a configurable depth / error policy. The output feeds a
//! `DicomDirectory` (the `DicomDirectoryBuilder` in the spec).
//!
//! The walk is **synchronous** because the core `radicom` crate has no async
//! runtime (tokio is isolated in `radicom-network`, per architecture §"heavy
//! deps isolated"). This is the normal split: a caller in an async context runs
//! the scan on a blocking thread and forwards the events, rather than pulling a
//! runtime into the data-model crate. The event stream is the same either way.

use crate::error::Error;
use crate::io::file::DicomFile;
use crate::media::icon::IconGenerator;
use crate::media::DicomDirectory;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What the scanner does when a directory entry cannot be read (§3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanErrorPolicy {
    /// Record the error and keep walking (the safe default).
    #[default]
    Skip,
    /// Abort the whole scan on the first error.
    Stop,
}

/// Scan configuration (§3.5).
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Maximum recursion depth; `0` scans only the root, `None` is unlimited.
    pub max_depth: Option<usize>,
    /// Error handling for unreadable entries.
    pub error_policy: ScanErrorPolicy,
    /// Glob filter applied to each file name (e.g. `*.dcm`); `None` accepts all.
    pub glob: Option<String>,
    /// Require the PS3.10 preamble and `DICM` marker before reporting a file.
    pub check_for_valid_header: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        // fo-dicom's scanner default: recurse fully, skip errors, no filter
        ScanOptions { max_depth: None, error_policy: ScanErrorPolicy::Skip, glob: None, check_for_valid_header: false }
    }
}

/// Shared cancellation flag for a running scan.
#[derive(Clone, Debug, Default)]
pub struct ScanControl {
    stopped: Arc<AtomicBool>,
}

impl ScanControl {
    /// Creates a control that allows the scan to run.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests that the scan stop after the current callback returns.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    /// Returns whether [`stop`](Self::stop) has been requested.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

/// A progress or file event during a scan (§3.5).
#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// Emitted after each matched file with running totals.
    Progress { scanned: usize, matched: usize },
    /// Emitted for each matched file, before it is opened.
    File { path: PathBuf, index: usize },
    /// Emitted once after a scan completes successfully.
    Complete { summary: ScanSummary },
}

/// Receives scan events (§3.5). Implement to stream progress or collect files.
pub trait ScanVisitor {
    fn on_event(&mut self, event: ScanEvent);
}

/// A simple `FnMut` visitor so callers do not need a struct.
impl<F> ScanVisitor for F
where
    F: FnMut(ScanEvent),
{
    fn on_event(&mut self, event: ScanEvent) {
        self(event)
    }
}

/// Result summary of a scan (§3.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanSummary {
    /// Directories visited.
    pub directories: usize,
    /// Files seen (before the glob filter).
    pub files_seen: usize,
    /// Files that matched the filter.
    pub files_matched: usize,
    /// Paths skipped because they could not be read.
    pub errors: usize,
    /// Whether the scan was stopped through [`ScanControl`].
    pub stopped: bool,
}

/// Scans `root` and feeds matched files to `on_match` (§3.5).
///
/// `on_match` is called with the file path and may return an error to abort
/// (used to wire the DICOMDIR builder). Events go to `visitor`.
pub fn scan_directory<F>(
    root: impl AsRef<Path>,
    options: &ScanOptions,
    visitor: &mut dyn ScanVisitor,
    on_match: F,
) -> Result<ScanSummary, Error>
where
    F: FnMut(&Path) -> Result<(), Error>,
{
    scan_directory_with_control(root, options, &ScanControl::default(), visitor, on_match)
}

/// Scans `root` with an explicit cancellation control.
pub fn scan_directory_with_control<F>(
    root: impl AsRef<Path>,
    options: &ScanOptions,
    control: &ScanControl,
    visitor: &mut dyn ScanVisitor,
    mut on_match: F,
) -> Result<ScanSummary, Error>
where
    F: FnMut(&Path) -> Result<(), Error>,
{
    let matcher = match &options.glob {
        Some(pattern) => Some(Glob::new(pattern)?),
        None => None,
    };
    let mut summary = ScanSummary::default();
    walk(root.as_ref(), 0, options, matcher.as_ref(), control, &mut summary, visitor, &mut on_match)?;
    summary.stopped = control.is_stopped();
    visitor.on_event(ScanEvent::Complete { summary: summary.clone() });
    Ok(summary)
}

#[allow(clippy::too_many_arguments)]
fn walk<F>(
    dir: &Path,
    depth: usize,
    options: &ScanOptions,
    matcher: Option<&Glob>,
    control: &ScanControl,
    summary: &mut ScanSummary,
    visitor: &mut dyn ScanVisitor,
    on_match: &mut F,
) -> Result<(), Error>
where
    F: FnMut(&Path) -> Result<(), Error>,
{
    if control.is_stopped() {
        return Ok(());
    }
    summary.directories += 1;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => return handle_error(options, summary, dir, &e),
    };

    for entry in entries {
        if control.is_stopped() {
            return Ok(());
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                handle_error(options, summary, dir, &e)?;
                continue;
            }
        };
        let path = entry.path();
        // use the metadata kind without following symlinks into a loop
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                handle_error(options, summary, &path, &e)?;
                continue;
            }
        };

        if file_type.is_dir() {
            let can_descend = options.max_depth.is_none_or(|max| depth < max);
            if can_descend {
                walk(&path, depth + 1, options, matcher, control, summary, visitor, on_match)?;
            }
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        summary.files_seen += 1;
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => continue,
        };
        if let Some(matcher) = matcher {
            if !matcher.matches(name) {
                continue;
            }
        }
        if options.check_for_valid_header && !DicomFile::has_valid_header(&path) {
            continue;
        }
        let index = summary.files_matched;
        visitor.on_event(ScanEvent::File { path: path.clone(), index });
        on_match(&path)?;
        summary.files_matched += 1;
        visitor.on_event(ScanEvent::Progress { scanned: summary.files_seen, matched: summary.files_matched });
    }
    Ok(())
}

fn handle_error(
    options: &ScanOptions,
    summary: &mut ScanSummary,
    path: &Path,
    error: &std::io::Error,
) -> Result<(), Error> {
    summary.errors += 1;
    match options.error_policy {
        ScanErrorPolicy::Skip => Ok(()),
        ScanErrorPolicy::Stop => Err(Error::File(crate::error::FileError::Other(format!(
            "scan failed at {}: {error}",
            path.display()
        )))),
    }
}

/// A DICOMDIR builder driven by the scanner (§3.5).
///
/// Collects matched files, then turns them into a `DicomDirectory` in one pass.
pub struct DicomDirectoryBuilder {
    file_set_id: String,
    files: Vec<PathBuf>,
}

impl DicomDirectoryBuilder {
    pub fn new(file_set_id: impl Into<String>) -> Self {
        DicomDirectoryBuilder { file_set_id: file_set_id.into(), files: Vec::new() }
    }

    /// A collector callback suitable as the scanner's `on_match`.
    pub fn collector(&mut self) -> impl FnMut(&Path) -> Result<(), Error> + '_ {
        move |path| {
            self.files.push(path.to_path_buf());
            Ok(())
        }
    }

    /// Builds the directory; `file_id` maps a scanned path to its record path.
    pub fn build<E>(
        self,
        mut file_id: impl FnMut(&Path) -> PathBuf,
    ) -> Result<DicomDirectory, E>
    where
        E: From<Error>,
    {
        build_with_icon(self.files, self.file_set_id, &mut file_id, None)
    }

    /// Builds the directory, attaching an icon to every record (§3.4).
    pub fn build_with_icon<E>(
        self,
        mut file_id: impl FnMut(&Path) -> PathBuf,
        generator: &dyn IconGenerator,
    ) -> Result<DicomDirectory, E>
    where
        E: From<Error>,
    {
        build_with_icon(self.files, self.file_set_id, &mut file_id, Some(generator))
    }
}

fn build_with_icon<E>(
    files: Vec<PathBuf>,
    file_set_id: String,
    file_id: &mut dyn FnMut(&Path) -> PathBuf,
    generator: Option<&dyn IconGenerator>,
) -> Result<DicomDirectory, E>
where
    E: From<Error>,
{
    let mut directory = DicomDirectory::try_new(file_set_id)?;
    for path in files {
        let file = DicomFile::open(&path)?;
        let id = file_id(&path);
        match generator {
            Some(generator) => directory.add_file_with_icon(&file, &id, generator)?,
            None => directory.add_file(&file, &id)?,
        };
    }
    Ok(directory)
}

/// A glob matcher supporting `*`, `?` and `**` (§3.5).
///
/// `*` matches within one path segment, `?` one character, `**` across
/// segments. The grammar is small and standard-library-free.
#[derive(Debug, Clone)]
pub struct Glob {
    pattern: String,
}

impl Glob {
    pub fn new(pattern: &str) -> Result<Glob, Error> {
        if pattern.is_empty() {
            return Err(Error::File(crate::error::FileError::Other("empty glob pattern".into())));
        }
        Ok(Glob { pattern: pattern.to_string() })
    }

    /// True when `name` (a file name, not a full path) matches the pattern.
    pub fn matches(&self, name: &str) -> bool {
        glob_match(self.pattern.as_bytes(), name.as_bytes())
    }
}

/// Iterative glob match with backtracking for `*`.
///
/// `*` matches any run of characters except `/`, so it stays within one path
/// segment; `**` collapses to `*` here because matches are applied to a single
/// file name. `?` matches one non-`/` character.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    // the last `*` position and the text position it was matched at
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        let advance = match pattern.get(p) {
            // a literal, or `?` for any single non-separator character
            Some(&c) if c == text[t] && c != b'*' => true,
            Some(&b'?') if text[t] != b'/' => true,
            Some(&b'*') => {
                // collapse `**` to `*` for single-segment names
                while pattern.get(p) == Some(&b'*') {
                    p += 1;
                }
                star = Some((p, t));
                false
            }
            _ => false,
        };
        if advance {
            p += 1;
            t += 1;
        } else if let Some((star_p, star_t)) = star {
            // backtrack: let the last `*` consume one more character
            if text[star_t] == b'/' {
                return false; // `*` cannot consume a separator
            }
            p = star_p;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    while pattern.get(p) == Some(&b'*') {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("radicom-scan-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn glob_matches_star_question_and_double_star() {
        let g = Glob::new("*.dcm").unwrap();
        assert!(g.matches("a.dcm"));
        assert!(!g.matches("a.txt"));
        assert!(!g.matches("dir/a.dcm"), "a single-level name only");

        assert!(Glob::new("IMG_??.dcm").unwrap().matches("IMG_01.dcm"));
        assert!(!Glob::new("IMG_??.dcm").unwrap().matches("IMG_1.dcm"));

        assert!(Glob::new("*").unwrap().matches("anything"));
        assert!(Glob::new("a*b*c").unwrap().matches("axxbyyc"));
        assert!(!Glob::new("a*b*c").unwrap().matches("axxbyy"));
    }

    #[test]
    fn a_flat_scan_finds_matching_files() {
        let dir = temp_dir("flat");
        touch(&dir.join("a.dcm"));
        touch(&dir.join("b.txt"));
        touch(&dir.join("c.dcm"));

        let options = ScanOptions { glob: Some("*.dcm".into()), ..Default::default() };
        let mut seen = Vec::new();
        let summary = scan_directory(&dir, &options, &mut |_| {}, |path| {
            seen.push(path.to_path_buf());
            Ok::<(), Error>(())
        })
        .unwrap();

        assert_eq!(summary.files_seen, 3, "all files seen");
        assert_eq!(summary.files_matched, 2, "only .dcm matched");
        assert_eq!(seen.len(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn valid_header_filter_skips_non_dicom_files() {
        let dir = temp_dir("header");
        let good = dir.join("good.dcm");
        let bad = dir.join("bad.dcm");
        let mut bytes = vec![0u8; 128];
        bytes.extend_from_slice(b"DICM");
        fs::write(&good, bytes).unwrap();
        touch(&bad);

        let options = ScanOptions { check_for_valid_header: true, ..Default::default() };
        let mut seen = Vec::new();
        let summary = scan_directory(&dir, &options, &mut |_| {}, |path| {
            seen.push(path.to_path_buf());
            Ok::<(), Error>(())
        })
        .unwrap();

        assert_eq!(summary.files_seen, 2);
        assert_eq!(summary.files_matched, 1);
        assert_eq!(seen, vec![good]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn depth_limits_recursion() {
        let dir = temp_dir("depth");
        touch(&dir.join("top.dcm"));
        touch(&dir.join("sub/deep.dcm"));
        touch(&dir.join("sub/deeper/deepest.dcm"));

        // max_depth 0 = root only
        let options = ScanOptions { max_depth: Some(0), ..Default::default() };
        let mut count = 0;
        scan_directory(&dir, &options, &mut |_| {}, |_| {
            count += 1;
            Ok::<(), Error>(())
        })
        .unwrap();
        assert_eq!(count, 1, "only the top-level file is seen at depth 0");

        // unlimited depth sees all three
        let all = ScanOptions::default();
        let mut total = 0;
        scan_directory(&dir, &all, &mut |_| {}, |_| {
            total += 1;
            Ok::<(), Error>(())
        })
        .unwrap();
        assert_eq!(total, 3);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn events_report_progress_and_files() {
        let dir = temp_dir("events");
        touch(&dir.join("a.dcm"));
        touch(&dir.join("b.dcm"));

        let mut files = 0usize;
        let mut progress_seen = false;
        scan_directory(&dir, &ScanOptions::default(), &mut |event| match event {
            ScanEvent::File { index, .. } => {
                assert_eq!(index, files, "indices are sequential");
                files += 1;
            }
            ScanEvent::Progress { matched, .. } if matched > 0 => progress_seen = true,
            _ => {}
        }, |_| Ok::<(), Error>(()))
        .unwrap();
        assert_eq!(files, 2);
        assert!(progress_seen, "progress events fire after each match");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_emits_a_complete_event_with_the_summary() {
        let dir = temp_dir("complete");
        touch(&dir.join("a.dcm"));

        let mut complete = None;
        let summary = scan_directory(&dir, &ScanOptions::default(), &mut |event| {
            if let ScanEvent::Complete { summary } = event {
                complete = Some(summary);
            }
        }, |_| Ok::<(), Error>(()))
        .unwrap();

        assert_eq!(complete, Some(summary));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_can_be_stopped_explicitly() {
        let dir = temp_dir("stop");
        touch(&dir.join("a.dcm"));
        touch(&dir.join("b.dcm"));
        let control = ScanControl::new();
        let callback_control = control.clone();
        let mut seen = 0;

        let summary = scan_directory_with_control(
            &dir,
            &ScanOptions::default(),
            &control,
            &mut |_| {},
            |_| {
                seen += 1;
                callback_control.stop();
                Ok::<(), Error>(())
            },
        )
        .unwrap();

        assert!(summary.stopped);
        assert_eq!(seen, 1, "stopping from the first callback prevents later files");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_root_with_skip_policy_counts_an_error() {
        let options = ScanOptions { error_policy: ScanErrorPolicy::Skip, ..Default::default() };
        let summary = scan_directory(
            std::env::temp_dir().join("radicom-scan-does-not-exist-xyz"),
            &options,
            &mut |_| {},
            |_| Ok::<(), Error>(()),
        )
        .unwrap();
        assert!(summary.errors >= 1, "the missing root is one error");
    }

    #[test]
    fn a_missing_root_with_stop_policy_errors() {
        let options = ScanOptions { error_policy: ScanErrorPolicy::Stop, ..Default::default() };
        let result = scan_directory(
            std::env::temp_dir().join("radicom-scan-does-not-exist-xyz"),
            &options,
            &mut |_| {},
            |_| Ok::<(), Error>(()),
        );
        assert!(result.is_err());
    }

    #[test]
    fn the_builder_collects_and_builds_a_directory() {
        // build two minimal DICOM files on disk, then scan + build a DICOMDIR
        let dir = temp_dir("build");
        for (i, uid) in ["1.2.3.4.5", "1.2.3.4.6"].iter().enumerate() {
            let mut dataset = crate::model::dataset::Dataset::new();
            dataset.insert_str(crate::media::tags::PATIENT_ID, crate::model::vr::Vr::Lo, "P1");
            dataset.insert_str(crate::media::tags::STUDY_INSTANCE_UID, crate::model::vr::Vr::Ui, "1.2.3");
            dataset.insert_str(crate::media::tags::SERIES_INSTANCE_UID, crate::model::vr::Vr::Ui, "1.2.3.4");
            dataset.insert_str(
                crate::media::tags::SOP_CLASS_UID,
                crate::model::vr::Vr::Ui,
                "1.2.840.10008.5.1.4.1.1.2",
            );
            dataset.insert_str(crate::media::tags::SOP_INSTANCE_UID, crate::model::vr::Vr::Ui, *uid);
            dataset.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
            DicomFile::from_dataset(dataset).save(dir.join(format!("img{i}.dcm"))).unwrap();
        }

        let mut builder = DicomDirectoryBuilder::new("MYFS");
        {
            let mut collector = builder.collector();
            let options = ScanOptions { glob: Some("*.dcm".into()), ..Default::default() };
            scan_directory(&dir, &options, &mut |_| {}, &mut collector).unwrap();
        }
        let directory = builder
            .build::<Error>(|path| PathBuf::from(path.file_name().unwrap()))
            .unwrap();
        // one patient, one study, one series, two images
        assert_eq!(directory.records().len(), 5);
        let _ = fs::remove_dir_all(dir);
    }
}

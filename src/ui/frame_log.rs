//! Session logs for post-hoc UI review: frame capture (`frames.txt`), a
//! per-frame timing CSV (`perf.log`), and composer visual-state debug output
//! (`composer.log`).
//!
//! Extracted from `mod.rs`'s `run_tui` loop so the hot loop stays focused on
//! "drain events → poll input → draw". Each logger owns its own file handle
//! and its own dedup/throttle state.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use crate::ui::app::App;
use crate::ui::render;

/// Cap on distinct frames captured to `frames.txt` (bounds the log size).
const MAX_CAPTURE_FRAMES: u64 = 2000;

/// Snapshot the frame-capture log at most this often. The offscreen re-render
/// is only for post-hoc review, so it must not tax the hot loop — mouse-wheel
/// scrolling should stay responsive regardless of output size.
const CAPTURE_INTERVAL: Duration = Duration::from_millis(100);

/// A deduplicated flipbook of text snapshots of the rendered screen, so a TUI
/// session can be reviewed after the fact (colors dropped, layout kept).
pub struct FrameCapture {
    file: BufWriter<File>,
    last_snapshot: String,
    frame_id: u64,
    last_capture: Instant,
}

impl FrameCapture {
    pub fn new(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            file: BufWriter::new(File::create(path)?),
            last_snapshot: String::new(),
            frame_id: 0,
            last_capture: Instant::now() - CAPTURE_INTERVAL,
        })
    }

    /// Render and append a frame if the loop is dirty, under the cap, and enough
    /// time has passed since the last capture. Returns the time spent rendering
    /// the snapshot (`Duration::ZERO` when skipped) so the perf log can record
    /// capture cost even when the dedup discards an identical frame.
    pub fn capture(&mut self, dirty: bool, app: &mut App, width: u16, height: u16) -> Duration {
        if !dirty
            || self.frame_id >= MAX_CAPTURE_FRAMES
            || self.last_capture.elapsed() < CAPTURE_INTERVAL
        {
            return Duration::ZERO;
        }
        self.last_capture = Instant::now();
        let cap_start = Instant::now();
        let snap = render::snapshot_text(app, width, height);
        let elapsed = cap_start.elapsed();
        if snap != self.last_snapshot {
            self.last_snapshot = snap.clone();
            self.frame_id += 1;
            let _ = writeln!(self.file, "──── frame {} ────", self.frame_id);
            let _ = writeln!(self.file, "{snap}");
        }
        elapsed
    }

    /// Number of distinct frames captured so far (used to label log rows).
    pub fn frame_id(&self) -> u64 {
        self.frame_id
    }

    pub fn flush(&mut self) {
        let _ = self.file.flush();
    }
}

/// One row of the per-frame perf CSV.
pub struct PerfRow<'a> {
    pub frame_id: u64,
    pub draw_ms: u128,
    pub capture_ms: u128,
    pub loop_ms: u128,
    pub dirty: bool,
    pub scroll_offset: usize,
    pub follow_bottom: bool,
    pub output_lines: usize,
    pub crossterm_events: u32,
    pub slept: bool,
    pub event_types: &'a str,
}

/// Per-frame timing log (CSV) for diagnosing scroll jank.
pub struct PerfLog {
    file: BufWriter<File>,
}

impl PerfLog {
    pub fn new(path: &Path) -> std::io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        let _ = writeln!(
            file,
            "frame_id,draw_ms,capture_ms,loop_ms,dirty,scroll_offset,follow_bottom,output_lines,crossterm_events,slept,event_types"
        );
        Ok(Self { file })
    }

    pub fn record(&mut self, row: &PerfRow<'_>) {
        let _ = writeln!(
            self.file,
            "{},{},{},{},{},{},{},{},{},{},{}",
            row.frame_id,
            row.draw_ms,
            row.capture_ms,
            row.loop_ms,
            row.dirty,
            row.scroll_offset,
            row.follow_bottom,
            row.output_lines,
            row.crossterm_events,
            row.slept,
            row.event_types,
        );
    }

    pub fn flush(&mut self) {
        let _ = self.file.flush();
    }
}

/// Debug log of the composer's visual state (rows, scroll, cursor), written only
/// when the composer is non-empty and its state changes.
pub struct ComposerLog {
    file: BufWriter<File>,
    prev_state: Option<(usize, usize, usize, usize)>,
}

impl ComposerLog {
    pub fn new(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            file: BufWriter::new(File::create(path)?),
            prev_state: None,
        })
    }

    /// Log the composer state if it changed since the last record. `size` is the
    /// terminal size used to derive the inner box dimensions.
    pub fn record(&mut self, app: &App, frame_id: u64, size: (u16, u16)) {
        let cursor = app.composer.cursor();
        let line0_len = app.composer.lines().first().map_or(0, |l| l.len());
        let n_lines = app.composer.lines().len();
        let state = (cursor.0, cursor.1, n_lines, line0_len);
        if app.composer.is_empty() || Some(state) == self.prev_state {
            return;
        }
        self.prev_state = Some(state);
        let inner_w = size.0.saturating_sub(2) as usize;
        let inner_h = size.1.saturating_sub(2) as usize;
        let lines = app.composer.lines();
        // Count visual rows.
        let vis_rows: usize = lines
            .iter()
            .map(|line| {
                let full_w = 2 + UnicodeWidthStr::width(line.as_str());
                full_w.div_ceil(inner_w.max(1)).max(1)
            })
            .sum();
        let scroll = vis_rows.saturating_sub(inner_h);
        let t0: String = lines
            .get(0)
            .unwrap_or(&String::new())
            .chars()
            .take(30)
            .collect();
        let _ = writeln!(
            self.file,
            "frame={} inner={}x{} vis_rows={} scroll={} cursor={:?} lines={} [0]={:?}",
            frame_id, inner_w, inner_h, vis_rows, scroll, cursor, n_lines, t0,
        );
    }

    pub fn flush(&mut self) {
        let _ = self.file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::App;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn frame_capture_dedups_identical_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frames.txt");
        let mut cap = FrameCapture::new(&path).unwrap();
        let mut app = App::new();

        // First dirty frame renders and is captured.
        let spent = cap.capture(true, &mut app, 80, 24);
        assert!(spent > Duration::ZERO, "first capture must render");
        assert_eq!(cap.frame_id(), 1);

        // Identical frame (nothing changed) is skipped by the dedup.
        cap.last_capture = Instant::now() - CAPTURE_INTERVAL; // bypass throttle
        let spent = cap.capture(true, &mut app, 80, 24);
        let _ = spent; // render cost is real even when the dedup drops the frame
        assert_eq!(cap.frame_id(), 1, "identical frame must not be captured");

        cap.flush();
        let log = read(&path);
        assert!(log.contains("──── frame 1 ────"), "{log}");
    }

    #[test]
    fn frame_capture_skips_clean_loops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frames.txt");
        let mut cap = FrameCapture::new(&path).unwrap();
        let mut app = App::new();

        let spent = cap.capture(false, &mut app, 80, 24);
        assert_eq!(spent, Duration::ZERO, "clean loop must not render");
        assert_eq!(cap.frame_id(), 0);
        cap.flush();
        assert_eq!(read(&path), "");
    }

    #[test]
    fn perf_log_writes_header_then_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("perf.log");
        let mut log = PerfLog::new(&path).unwrap();

        log.record(&PerfRow {
            frame_id: 7,
            draw_ms: 3,
            capture_ms: 1,
            loop_ms: 9,
            dirty: true,
            scroll_offset: 42,
            follow_bottom: false,
            output_lines: 120,
            crossterm_events: 4,
            slept: false,
            event_types: "k",
        });
        log.flush();

        let binding = read(&path);
        let lines: Vec<&str> = binding.lines().collect();
        assert_eq!(lines.len(), 2, "header + one row: {lines:?}");
        assert!(lines[0].starts_with("frame_id,draw_ms"), "{}", lines[0]);
        assert_eq!(
            lines[1], "7,3,1,9,true,42,false,120,4,false,k",
            "CSV row must be field-ordered"
        );
    }

    #[test]
    fn composer_log_records_only_visible_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("composer.log");
        let mut log = ComposerLog::new(&path).unwrap();
        let mut app = App::new();
        let size = (80, 24);

        // Empty composer → nothing to log.
        log.record(&app, 1, size);
        assert_eq!(read(&path), "");

        // Typed text → a state change → one record.
        app.composer.insert_str("hello");
        log.record(&app, 2, size);
        log.flush(); // records sit in a BufWriter; flush before reading
        let first = read(&path);
        assert!(first.contains("frame=2"), "{first}");
        assert!(first.contains("hello"), "{first}");

        // Same state again → no new record.
        log.record(&app, 3, size);
        log.flush();
        assert_eq!(read(&path), first, "unchanged state must not be re-logged");
    }
}

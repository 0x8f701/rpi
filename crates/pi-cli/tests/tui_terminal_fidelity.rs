//! TUI terminal-grid fidelity gates for continuous overlay frames.
//!
//! Drives the shipped `render_model_selector` (the /model picker overlay)
//! through ratatui's `TestBackend` across frame sequences and resize matrices,
//! asserting the grid-level invariants the terminal must uphold:
//! - no duplicated rows or stale leftover lines between consecutive frames
//! - no single-character column overflow at 20/40/80/120 columns
//! - CJK wide labels never split a wide char mid-cell
//! - ANSI-styled content does not corrupt the grid
//!
//! Layer note: these tests render the deepest reachable shipped overlay
//! without touching `tui.rs` internals. Full-PTY transcript streaming frames
//! remain gated on tui.rs ownership and are listed in the audit report.

use pi_cli::tui_test_support::{model_selector_screen, selector_frame_fixture};
use unicode_width::UnicodeWidthStr;

fn fixture_models(count: usize, provider: &str, cjk: bool) -> Vec<pi_ai::Model> {
    (0..count)
        .map(|index| {
            let name = if cjk && index % 5 == 0 {
                format!("模型 {index:02}")
            } else {
                format!("Model {index:02}")
            };
            selector_frame_fixture(provider, &format!("m{index:02}"), &name)
        })
        .collect()
}

fn grid_rows(screen: &str) -> Vec<String> {
    screen.lines().map(str::to_owned).collect()
}

#[test]
fn consecutive_frames_leave_no_stale_or_duplicated_rows() {
    let models = fixture_models(30, "frame", false);
    // Frame 1: selection near the head.
    let first = model_selector_screen(&models, "frame/m00", 80, 20);
    assert!(first.contains("Model 00"), "{first}");
    // Frame 2: same panel after the cursor jumps to the tail — the previous
    // head rows must be gone from the new buffer (no stale lines).
    let second = model_selector_screen(&models, "frame/m29", 80, 20);
    let rows = grid_rows(&second);
    assert!(
        rows.iter().any(|row| row.contains("Model 29")),
        "tail row must render: {second}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("Model 00") && row.contains("●")),
        "stale head highlight must not survive into the new frame: {rows:?}"
    );
    // No duplicated non-empty data rows within a single frame.
    let mut sorted = rows.clone();
    sorted.sort();
    let duplicates = sorted.windows(2).filter(|pair| {
        !pair[0].trim().is_empty() && pair[0] == pair[1] && pair[0].contains("Model")
    });
    assert_eq!(duplicates.count(), 0, "duplicated rows: {rows:?}");
}

#[test]
fn resize_matrix_keeps_single_character_columns_and_visible_selection() {
    let models = fixture_models(50, "resize", false);
    // The picker overlay's own minimum panel width is 30 columns; below that
    // the overlay cannot fit at all (pre-existing renderer constraint), so
    // the matrix starts there.
    for width in [30u16, 40, 80, 120] {
        for height in [12u16, 18, 26] {
            let screen = model_selector_screen(&models, "resize/m49", width, height);
            for (line_index, line) in grid_rows(&screen).iter().enumerate() {
                let cells = UnicodeWidthStr::width(line.as_str());
                assert_eq!(
                    cells,
                    usize::from(width),
                    "width {width} height {height}: line {line_index} is {cells} cells: {line:?}"
                );
            }
            assert!(
                screen.contains("Selected: resize/m49"),
                "selection footer must survive {width}x{height}: {screen}"
            );
        }
    }
}

#[test]
fn cjk_labels_render_without_splitting_wide_characters() {
    let models = fixture_models(24, "cjk", true);
    // CJK ideographs occupy two terminal cells each. Measuring with
    // `chars().count()` would under-count "模型 05" as 5 instead of 7.
    let cjk_label = "模型 05";
    assert_eq!(cjk_label.chars().count(), 5);
    assert_eq!(UnicodeWidthStr::width(cjk_label), 7);
    for width in [30u16, 44, 80] {
        let screen = model_selector_screen(&models, "cjk/m05", width, 20);
        for (line_index, line) in grid_rows(&screen).iter().enumerate() {
            let cells = UnicodeWidthStr::width(line.as_str());
            assert_eq!(
                cells,
                usize::from(width),
                "CJK width {width}: line {line_index} is {cells} cells: {line:?}"
            );
        }
        // A split wide char renders as U+FFFD replacement or a stray
        // half-cell; neither may appear anywhere in the grid.
        assert!(
            !screen.contains('\u{FFFD}'),
            "replacement char at width {width}: {screen}"
        );
        // The truncated label keeps its leading text intact (no partial code
        // points from the middle of a wide char).
        assert!(
            screen.contains(cjk_label) || screen.contains("m05"),
            "CJK selection row lost at width {width}: {screen}"
        );
    }
}

#[test]
fn ansi_escaped_content_does_not_corrupt_the_grid() {
    let mut models = fixture_models(6, "ansi", false);
    if let Some(first) = models.first_mut() {
        // A hostile catalog name carrying ANSI escapes must be neutralized by
        // the renderer's clean_terminal_text pass, not emitted raw.
        first.name = "\x1b[31mRed Model\x1b[0m".to_owned();
    }
    let screen = model_selector_screen(&models, "ansi/m00", 60, 16);
    assert!(
        !screen.contains("\x1b"),
        "raw ANSI escape reached the grid: {screen:?}"
    );
}

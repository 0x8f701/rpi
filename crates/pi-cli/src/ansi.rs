//! One-pass ANSI parser for untrusted terminal text.
//!
//! Tool/bash output often carries SGR styling (`\x1b[96m…\x1b[0m`) that must
//! render as styled spans instead of leaking literal `[96m` fragments into
//! the TUI or Web surfaces. This module parses the style-bearing subset of
//! ECMA-48 — SGR (`CSI … m`) — and strips everything else that would execute
//! in a terminal or corrupt layout: non-SGR CSI (cursor movement, erase,
//! mode setting), OSC (OSC8 hyperlinks, titles, clipboard) terminated by BEL
//! or ST, bare ESC, and C0/C1 controls.
//!
//! The scan is a single bounded forward pass: no backtracking, no regex, no
//! allocation beyond the returned runs. Every run carries an ABSOLUTE
//! (fully-resolved) style, so a `reset` or a later color change can never
//! leak style into unrelated text — the caller's base/role color applies
//! wherever the run has no ANSI color. [`ansi_plain_text`] is the plain-text
//! projection of the same parse, which keeps [`crate::tui::clean_terminal_text`]
//! byte-identical to the styled render path (the two never drift).
//!
//! Supported SGR attributes: reset (0); bold/dim/italic/underline (1/2/3/4)
//! with their off codes (22/23/24); standard and bright foreground/background
//! (30-37/90-97, 40-47/100-107); 256-color (`38;5;n`/`48;5;n`); and truecolor
//! (`38;2;r;g;b`/`48;2;r;g;b`). Unsupported SGR parameters (blink, reverse,
//! hidden, …) are consumed and ignored. Tabs expand to four spaces; newlines
//! are preserved inside run text so the caller decides row layout.
//!
//! Callers redact FAIL-CLOSED: parse the raw input, redact the full plain
//! text, and — if redaction changed anything (a credential shape, possibly
//! split across an SGR boundary in the raw input) — render the whole output
//! as one base-style plain run of the redacted text instead of styled runs
//! (TUI: `ansi_styled_lines` in tui.rs; Web: `AnsiText`).

use ratatui::style::{Color, Modifier};

/// A contiguous run of plain text with a fully-resolved style.
///
/// `fg`/`bg` are `None` when no SGR color is active at this position (the
/// caller's base/role color applies), and `modifier` is the accumulated
/// attribute set at this position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnsiRun {
    pub text: String,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub modifier: Modifier,
}

/// Parse `text` into styled runs in a single forward pass. Newlines survive
/// inside run text; tabs expand to four spaces; every other C0/C1 control is
/// dropped; ESC sequences are consumed without reaching the output.
pub(crate) fn parse_ansi_runs(text: &str) -> Vec<AnsiRun> {
    let mut runs: Vec<AnsiRun> = Vec::new();
    let mut fg: Option<Color> = None;
    let mut bg: Option<Color> = None;
    let mut modifier = Modifier::empty();
    let mut current = String::new();

    let flush = |runs: &mut Vec<AnsiRun>,
                 current: &mut String,
                 fg: Option<Color>,
                 bg: Option<Color>,
                 modifier: Modifier| {
        if current.is_empty() {
            return;
        }
        runs.push(AnsiRun {
            text: std::mem::take(current),
            fg,
            bg,
            modifier,
        });
    };

    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            match characters.peek() {
                Some('[') => {
                    characters.next();
                    // CSI: parameter bytes (0x30-0x3f), then a final byte
                    // (0x40-0x7e). A byte outside this grammar marks the
                    // sequence malformed: fall back to the historical
                    // clean_terminal_text rule (consume up to the first
                    // `@`..=`~` byte) so plain-text parity holds for hostile
                    // input.
                    let mut params = String::new();
                    let mut final_byte = None;
                    while let Some(&value) = characters.peek() {
                        if ('\u{30}'..='\u{3f}').contains(&value) {
                            params.push(value);
                            characters.next();
                        } else if ('\u{20}'..='\u{2f}').contains(&value) {
                            // Intermediate byte: consumed, not part of SGR.
                            characters.next();
                        } else if ('\u{40}'..='\u{7e}').contains(&value) {
                            final_byte = Some(value);
                            characters.next();
                            break;
                        } else {
                            while let Some(value) = characters.next() {
                                if ('\u{40}'..='\u{7e}').contains(&value) {
                                    break;
                                }
                            }
                            break;
                        }
                    }
                    if final_byte == Some('m') {
                        flush(&mut runs, &mut current, fg, bg, modifier);
                        apply_sgr(&mut fg, &mut bg, &mut modifier, &params);
                    }
                }
                Some(']') => {
                    characters.next();
                    // OSC: consume through BEL or ST (ESC \); an unterminated
                    // OSC consumes the rest of the input.
                    while let Some(value) = characters.next() {
                        if value == '\u{7}' {
                            break;
                        }
                        if value == '\u{1b}' && characters.peek() == Some(&'\\') {
                            characters.next();
                            break;
                        }
                    }
                }
                // A bare ESC not opening a sequence is dropped without
                // swallowing the next character, so trailing plain text
                // survives (ESC <plain> -> <plain>).
                _ => {}
            }
        } else if character == '\t' {
            current.push_str("    ");
        } else if character == '\n' || !character.is_control() {
            current.push(character);
        }
    }
    flush(&mut runs, &mut current, fg, bg, modifier);
    runs
}

/// Plain-text projection of [`parse_ansi_runs`]: exactly the visible text
/// with every ESC/control sequence removed and tabs expanded. This is the
/// canonical `clean_terminal_text` behavior, shared so the plain and styled
/// display paths can never diverge.
pub(crate) fn ansi_plain_text(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    for run in parse_ansi_runs(text) {
        plain.push_str(&run.text);
    }
    plain
}

/// Apply an SGR parameter string to the accumulated style state. Parameters
/// are split on `;` (and `:` for the newer colon form); unsupported codes are
/// ignored; malformed extended-color sequences are consumed without effect.
fn apply_sgr(
    fg: &mut Option<Color>,
    bg: &mut Option<Color>,
    modifier: &mut Modifier,
    params: &str,
) {
    let mut params = params.split(|character| character == ';' || character == ':');
    while let Some(param) = params.next() {
        if param.is_empty() {
            continue;
        }
        let Ok(value) = param.parse::<u16>() else {
            continue;
        };
        match value {
            0 => {
                *fg = None;
                *bg = None;
                *modifier = Modifier::empty();
            }
            1 => *modifier |= Modifier::BOLD,
            2 => *modifier |= Modifier::DIM,
            3 => *modifier |= Modifier::ITALIC,
            4 => *modifier |= Modifier::UNDERLINED,
            22 => *modifier &= !(Modifier::BOLD | Modifier::DIM),
            23 => *modifier &= !Modifier::ITALIC,
            24 => *modifier &= !Modifier::UNDERLINED,
            30..=37 => *fg = Some(standard_color(value - 30)),
            40..=47 => *bg = Some(standard_color(value - 40)),
            90..=97 => *fg = Some(bright_color(value - 90)),
            100..=107 => *bg = Some(bright_color(value - 100)),
            39 => *fg = None,
            49 => *bg = None,
            38 => apply_extended_color(fg, &mut params),
            48 => apply_extended_color(bg, &mut params),
            _ => {}
        }
    }
}

/// `38;5;n` / `38;2;r;g;b` (and the `48;…` background forms). Consumes the
/// submode and payload from the shared parameter iterator.
fn apply_extended_color(slot: &mut Option<Color>, params: &mut dyn Iterator<Item = &str>) {
    match params.next() {
        Some("5") => {
            if let Some(Ok(index)) = params.next().map(|raw| raw.parse::<u16>()) {
                *slot = Some(Color::Indexed(index.min(255) as u8));
            }
        }
        Some("2") => {
            let read = |raw: Option<&str>| {
                raw.and_then(|raw| raw.parse::<u16>().ok())
                    .map(|value| value.min(255) as u8)
            };
            let red = read(params.next());
            let green = read(params.next());
            let blue = read(params.next());
            if let (Some(red), Some(green), Some(blue)) = (red, green, blue) {
                *slot = Some(Color::Rgb(red, green, blue));
            }
        }
        _ => {}
    }
}

/// SGR 30-37 / 40-47 → ratatui standard palette.
fn standard_color(index: u16) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        _ => Color::Reset,
    }
}

/// SGR 90-97 / 100-107 → ratatui bright palette.
fn bright_color(index: u16) -> Color {
    match index {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        7 => Color::White,
        _ => Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::clean_terminal_text;

    fn run_style(run: &AnsiRun) -> (Option<Color>, Option<Color>, Modifier) {
        (run.fg, run.bg, run.modifier)
    }

    #[test]
    fn bright_fg_samples_parse_to_light_cyan_and_light_blue() {
        let runs = parse_ansi_runs("\u{1b}[96mnew zk sign circuit\u{1b}[0m");
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, "new zk sign circuit");
        assert_eq!(run_style(&runs[0]), (Some(Color::LightCyan), None, Modifier::empty()));

        let runs = parse_ansi_runs("\u{1b}[94mbuild inner circuit\u{1b}[0m");
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, "build inner circuit");
        assert_eq!(run_style(&runs[0]), (Some(Color::LightBlue), None, Modifier::empty()));
    }

    #[test]
    fn indexed_fg_bg_sample_keeps_spacing() {
        let runs = parse_ansi_runs("\u{1b}[38;5;230m\u{1b}[48;5;34m 11ms \u{1b}[0m");
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, " 11ms ");
        assert_eq!(
            run_style(&runs[0]),
            (Some(Color::Indexed(230)), Some(Color::Indexed(34)), Modifier::empty())
        );
    }

    #[test]
    fn truecolor_maps_to_rgb() {
        let runs = parse_ansi_runs("\u{1b}[38;2;255;0;128mA\u{1b}[48;2;10;20;30mB\u{1b}[0m");
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(runs[0].text, "A");
        assert_eq!(runs[0].fg, Some(Color::Rgb(255, 0, 128)));
        assert_eq!(runs[1].text, "B");
        assert_eq!(runs[1].bg, Some(Color::Rgb(10, 20, 30)));
        // A truecolor sequence with a truncated payload is ignored whole.
        let runs = parse_ansi_runs("\u{1b}[38;2;1;2mX");
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, "X");
        assert_eq!(runs[0].fg, None);
    }

    #[test]
    fn modifiers_apply_and_reset_isolates() {
        let runs = parse_ansi_runs("\u{1b}[1;3;4;96mbold italic underline cyan\u{1b}[0mplain\u{1b}[31mred");
        assert_eq!(runs.len(), 3, "{runs:?}");
        assert_eq!(runs[0].text, "bold italic underline cyan");
        assert_eq!(
            run_style(&runs[0]),
            (
                Some(Color::LightCyan),
                None,
                Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED
            )
        );
        // Reset (0) clears color AND modifiers: the middle run is fully plain.
        assert_eq!(runs[1].text, "plain");
        assert_eq!(run_style(&runs[1]), (None, None, Modifier::empty()));
        // A later color change never leaks into the reset run.
        assert_eq!(runs[2].text, "red");
        assert_eq!(run_style(&runs[2]), (Some(Color::Red), None, Modifier::empty()));

        // 22 clears bold+dim; style carries across a newline until reset.
        let runs = parse_ansi_runs("\u{1b}[1;96mbold cyan\ntext\u{1b}[22mnow plain");
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(runs[0].text, "bold cyan\ntext");
        assert_eq!(runs[0].fg, Some(Color::LightCyan));
        assert_eq!(runs[0].modifier, Modifier::BOLD);
        assert_eq!(runs[1].text, "now plain");
        assert_eq!(runs[1].fg, Some(Color::LightCyan));
        assert_eq!(runs[1].modifier, Modifier::empty());
    }

    #[test]
    fn standard_and_bright_backgrounds_map() {
        let runs = parse_ansi_runs("\u{1b}[31;44mred on blue\u{1b}[0m\u{1b}[90;107mgray on white\u{1b}[0m");
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(
            run_style(&runs[0]),
            (Some(Color::Red), Some(Color::Blue), Modifier::empty())
        );
        // 90 = bright fg (DarkGray); 107 = bright bg (White).
        assert_eq!(
            run_style(&runs[1]),
            (Some(Color::DarkGray), Some(Color::White), Modifier::empty())
        );
        // 100-107 are bright BACKGROUND codes: 100 sets bright-black bg.
        let runs = parse_ansi_runs("\u{1b}[100mX\u{1b}[0m");
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, "X");
        assert_eq!(run_style(&runs[0]), (None, Some(Color::DarkGray), Modifier::empty()));
    }

    #[test]
    fn osc8_and_cursor_csi_are_stripped() {
        // OSC 8 hyperlink (BEL and ST terminated) and cursor positioning CSI
        // never reach the runs; surrounding text survives.
        let runs = parse_ansi_runs("go\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{7}end\u{1b}[2;5Hpost");
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, "golinkendpost");
        assert_eq!(runs[0].fg, None);

        let runs = parse_ansi_runs("go\u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\end");
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, "golinkend");

        // Cursor-hide / move sequences are non-SGR CSI and are stripped.
        let runs = parse_ansi_runs("a\u{1b}[?25lb\u{1b}[1;2Hc");
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, "abc");
    }

    #[test]
    fn malformed_and_incomplete_sequences_are_stripped() {
        // Incomplete CSI at end of input: stripped, nothing hangs.
        assert_eq!(ansi_plain_text("tail\u{1b}[96"), "tail");
        assert_eq!(ansi_plain_text("tail\u{1b}["), "tail");
        assert_eq!(ansi_plain_text("tail\u{1b}"), "tail");
        // Bare ESC does not swallow the following plain character.
        assert_eq!(ansi_plain_text("x\u{1b}y"), "xy");
        // Incomplete extended-color payloads leave the text plain.
        assert_eq!(ansi_plain_text("\u{1b}[38;5;mX"), "X");
        // A non-SGR CSI consumes its final byte too: `X` here terminates the
        // sequence, so nothing of it survives.
        assert_eq!(ansi_plain_text("\u{1b}[38;2;1;2X"), "");
        assert_eq!(ansi_plain_text("\u{1b}[38mX"), "X");
        // Unterminated OSC consumes to end of input.
        assert_eq!(ansi_plain_text("a\u{1b}]8;;https://x"), "a");
    }

    #[test]
    fn tabs_expand_and_c0_controls_are_dropped() {
        let runs = parse_ansi_runs("a\tb\n\u{4f60}\u{597d}\u{1b}[1mc");
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(runs[0].text, "a    b\n\u{4f60}\u{597d}");
        assert_eq!(runs[0].modifier, Modifier::empty());
        assert_eq!(runs[1].text, "c");
        assert_eq!(runs[1].modifier, Modifier::BOLD);
        // BEL, backspace, and other C0 controls disappear (not replaced).
        let runs = parse_ansi_runs("x\u{7}y\u{8}z\u{0}w");
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, "xyzw");
    }

    #[test]
    fn plain_text_projection_matches_clean_terminal_text() {
        let samples = [
            "\u{1b}[96mnew zk sign circuit\u{1b}[0m",
            "\u{1b}[94mbuild inner circuit\u{1b}[0m",
            "\u{1b}[38;5;230m\u{1b}[48;5;34m 11ms \u{1b}[0m",
            "pre\u{1b}[31mRED\u{1b}[0m\u{1b}[2;5Hpost",
            "go\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{7}end",
            "go\u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\end",
            "a\tb\n\u{4f60}\u{597d}\u{1b}[1mc",
            "x\u{1b}y\u{7}z\u{8}w",
            "plain text, no sequences",
            "\u{1b}[96",
            "\u{1b}[38;5;",
            "\u{1b}",
            "",
            "\u{1b}[1;31;44mX\u{1b}[0mY\n",
        ];
        for sample in samples {
            let plain = ansi_plain_text(sample);
            assert_eq!(plain, clean_terminal_text(sample), "sample {sample:?}");
            // Structural invariant: the plain projection is exactly the
            // concatenation of the run texts.
            let joined: String = parse_ansi_runs(sample).into_iter().map(|run| run.text).collect();
            assert_eq!(joined, plain, "run concatenation for {sample:?}");
        }
    }
}

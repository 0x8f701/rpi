//! Output helpers: ANSI styling, stderr warnings, and reasoning-level parsing.

use pi_agent::ThinkingLevel;

/// Bold ANSI prefix.
pub const BOLD: &str = "\x1b[1m";
/// Dim ANSI prefix.
pub const DIM: &str = "\x1b[2m";
/// Yellow ANSI prefix (warnings).
pub const YELLOW: &str = "\x1b[33m";
/// Red ANSI prefix (errors).
pub const RED: &str = "\x1b[31m";
/// Reset ANSI sequence.
pub const RESET: &str = "\x1b[0m";

/// Print a yellow warning line to stderr.
pub fn warn_line(msg: &str) {
    eprintln!("{YELLOW}{msg}{RESET}");
}

/// Print an error to stderr (no ANSI in case the stream is not a TTY).
pub fn error_line(msg: &str) {
    eprintln!("pi: {msg}");
}

/// Parse a reasoning level string into a [`ThinkingLevel`].
///
/// Accepts the six pi levels (off|minimal|low|medium|high|xhigh) case-
/// insensitively. An empty string is treated as unset and resolves to the
/// pi default (`medium`), which the session clamps to the model's
/// capabilities — matching the Go CLI's `DefaultThinkingLevel`.
pub fn parse_thinking_level(level: &str) -> ThinkingLevel {
    match level.trim().to_ascii_lowercase().as_str() {
        "" | "medium" => ThinkingLevel::Medium,
        "off" => ThinkingLevel::Off,
        "minimal" => ThinkingLevel::Minimal,
        "low" => ThinkingLevel::Low,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::Xhigh,
        other => {
            warn_line(&format!(
                "ignoring unknown thinking level {other:?}; defaulting to medium"
            ));
            ThinkingLevel::Medium
        }
    }
}

/// The canonical lowercase name for a [`ThinkingLevel`], matching pi's
/// `VALID_THINKING_LEVELS` serialization.
#[must_use]
pub fn thinking_level_str(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
    }
}

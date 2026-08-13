// One-pass ANSI SGR parser for untrusted terminal text (web mirror of
// crates/pi-cli/src/ansi.rs — same grammar, same palette, same plain-text
// projection). Tool/bash output often carries SGR styling (`\x1b[96m…\x1b[0m`)
// that must render as styled runs instead of leaking literal `[96m` fragments
// into the transcript.
//
// The scan is a single bounded forward pass with no backtracking: the only
// regexes are fixed patterns (an SGR parameter split and a digits-only token
// check — no user input in either pattern, nothing to backtrack over). Every
// run carries an ABSOLUTE style, so a reset or a later color change can never
// leak into unrelated text.
// Non-SGR CSI (cursor movement, erase, mode setting), OSC (OSC8 hyperlinks,
// titles, clipboard) terminated by BEL or ST, bare ESC, and C0/C1 controls
// are consumed and never reach the output. Tabs expand to four spaces,
// matching the TUI.
//
// Security model: this module never builds HTML and never sees raw HTML.
// Callers follow a FAIL-CLOSED redaction contract: parse the raw input first,
// redact the FULL plain text, and — if redaction changed anything (a
// credential shape, possibly split across an SGR boundary in the raw input)
// — render the whole output as one base-style plain run of the redacted
// text instead of styled runs (see src/AnsiText.tsx; TUI:
// `ansi_styled_lines`). `redactSecrets(ansiToPlainText(text))` is exactly
// what the clipboard copy writes: text only, no escape sequences.

/** Fully-resolved style for one text run. Palette colors (0-255) map to the
 *  xterm palette; truecolor carries explicit rgb strings. */
export interface AnsiRun {
  text: string;
  /** xterm palette index 0-255 (16-color + 256-color cube/grays). */
  fg?: number;
  bg?: number;
  /** Truecolor foreground 'rgb(r, g, b)' from `38;2;…`; takes precedence
   *  over `fg`. */
  fgRgb?: string;
  /** Truecolor background 'rgb(r, g, b)' from `48;2;…`; takes precedence
   *  over `bg`. */
  bgRgb?: string;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
}

// Standard xterm 16-color palette — identical to the TUI's ratatui mapping
// (black/red/green/yellow/blue/magenta/cyan/white + bright variants).
const BASE_PALETTE: ReadonlyArray<readonly [number, number, number]> = [
  [0x00, 0x00, 0x00], [0x80, 0x00, 0x00], [0x00, 0x80, 0x00], [0x80, 0x80, 0x00],
  [0x00, 0x00, 0x80], [0x80, 0x00, 0x80], [0x00, 0x80, 0x80], [0xc0, 0xc0, 0xc0],
  [0x80, 0x80, 0x80], [0xff, 0x00, 0x00], [0x00, 0xff, 0x00], [0xff, 0xff, 0x00],
  [0x00, 0x00, 0xff], [0xff, 0x00, 0xff], [0x00, 0xff, 0xff], [0xff, 0xff, 0xff],
];

const CUBE_LEVELS = [0, 95, 135, 175, 215, 255] as const;

/** xterm 256-color palette → 'rgb(r, g, b)': 0-15 base, 16-231 cube
 *  (6×6×6), 232-255 grayscale ramp. Out-of-range indices clamp to the
 *  palette bounds. The result is constructed solely from the fixed tables
 *  above — never from raw input — so it is safe to hand to React style. */
export function ansiRgb(index: number): string {
  const clamped = Math.max(0, Math.min(255, index | 0));
  let r: number;
  let g: number;
  let b: number;
  if (clamped < 16) {
    const base = BASE_PALETTE[clamped];
    r = base[0];
    g = base[1];
    b = base[2];
  } else if (clamped < 232) {
    const value = clamped - 16;
    r = CUBE_LEVELS[Math.floor(value / 36)];
    g = CUBE_LEVELS[Math.floor((value % 36) / 6)];
    b = CUBE_LEVELS[value % 6];
  } else {
    r = 8 + (clamped - 232) * 10;
    g = r;
    b = r;
  }
  return `rgb(${r}, ${g}, ${b})`;
}

interface SgrState {
  fg?: number;
  bg?: number;
  fgRgb?: string;
  bgRgb?: string;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
}

function emptyStyle(): SgrState {
  return { bold: false, dim: false, italic: false, underline: false };
}

/** Apply one SGR parameter string (the bytes between `ESC[` and `m`) to the
 *  accumulated state. Parameters split on `;` (and `:` for the newer colon
 *  form); unsupported codes are ignored; malformed extended-color payloads
 *  are consumed without effect. Numeric tokens must be plain ASCII digits
 *  (mirroring Rust's `u16::parse` — hex/exponent forms are rejected), and
 *  extended-color channels clamp to 0-255 like the TUI's `min(255)` cast. */
const DIGIT_TOKEN = /^[0-9]+$/;

function applySgr(params: string, state: SgrState): void {
  const tokens = params.split(/[;:]/);
  const channel = (at: number): number | null => {
    const raw = tokens[at];
    if (raw === undefined || !DIGIT_TOKEN.test(raw)) return null;
    const value = Number(raw);
    return value <= 65535 ? value : null;
  };
  for (let t = 0; t < tokens.length; t += 1) {
    const raw = tokens[t];
    if (raw === '' || !DIGIT_TOKEN.test(raw)) continue;
    const value = Number(raw);
    if (value > 65535) continue;
    switch (value) {
      case 0:
        Object.assign(state, emptyStyle());
        state.fg = undefined;
        state.bg = undefined;
        state.fgRgb = undefined;
        state.bgRgb = undefined;
        break;
      case 1:
        state.bold = true;
        break;
      case 2:
        state.dim = true;
        break;
      case 3:
        state.italic = true;
        break;
      case 4:
        state.underline = true;
        break;
      case 22:
        state.bold = false;
        state.dim = false;
        break;
      case 23:
        state.italic = false;
        break;
      case 24:
        state.underline = false;
        break;
      case 39:
        state.fg = undefined;
        state.fgRgb = undefined;
        break;
      case 49:
        state.bg = undefined;
        state.bgRgb = undefined;
        break;
      default:
        if (value >= 30 && value <= 37) {
          state.fg = value - 30;
          state.fgRgb = undefined;
        } else if (value >= 40 && value <= 47) {
          state.bg = value - 40;
          state.bgRgb = undefined;
        } else if (value >= 90 && value <= 97) {
          state.fg = value - 90 + 8;
          state.fgRgb = undefined;
        } else if (value >= 100 && value <= 107) {
          state.bg = value - 100 + 8;
          state.bgRgb = undefined;
        } else if (value === 38 || value === 48) {
          const slot = value === 38 ? 'fg' : 'bg';
          const rgbSlot = value === 38 ? 'fgRgb' : 'bgRgb';
          const mode = tokens[t + 1];
          if (mode === '5') {
            const index = channel(t + 2);
            if (index !== null) {
              state[slot] = Math.min(255, index);
              state[rgbSlot] = undefined;
              t += 2;
            }
          } else if (mode === '2') {
            const red = channel(t + 2);
            const green = channel(t + 3);
            const blue = channel(t + 4);
            if (red !== null && green !== null && blue !== null) {
              state[slot] = undefined;
              state[rgbSlot] = `rgb(${Math.min(255, red)}, ${Math.min(255, green)}, ${Math.min(255, blue)})`;
              t += 4;
            }
          }
        }
        break;
    }
  }
}

/** Parse `text` into styled runs in a single forward pass. Newlines survive
 *  inside run text (the renderer preserves `<pre>` whitespace); tabs expand
 *  to four spaces; every other C0/C1 control is dropped; ESC sequences are
 *  consumed without reaching the output. */
export function parseAnsi(text: string): AnsiRun[] {
  const runs: AnsiRun[] = [];
  const state: SgrState = emptyStyle();
  let current = '';
  const flush = (): void => {
    if (current !== '') {
      runs.push({
        text: current,
        fg: state.fg,
        bg: state.bg,
        fgRgb: state.fgRgb,
        bgRgb: state.bgRgb,
        bold: state.bold,
        dim: state.dim,
        italic: state.italic,
        underline: state.underline,
      });
      current = '';
    }
  };

  let i = 0;
  while (i < text.length) {
    const code = text.codePointAt(i) as number;
    if (code === 0x1b) {
      const next = text.codePointAt(i + 1);
      if (next === 0x5b) {
        // CSI: parameter bytes (0x30-0x3f) then a final byte (0x40-0x7e). A
        // byte outside this grammar marks the sequence malformed: fall back
        // to the historical clean_terminal_text rule (consume up to the
        // first `@`..=`~` byte) so plain-text parity holds.
        let j = i + 2;
        let params = '';
        let finalByte = 0;
        while (j < text.length) {
          const c = text.codePointAt(j) as number;
          if (c >= 0x30 && c <= 0x3f) {
            params += text[j];
            j += 1;
          } else if (c >= 0x20 && c <= 0x2f) {
            // Intermediate byte: consumed, not part of SGR.
            j += 1;
          } else if (c >= 0x40 && c <= 0x7e) {
            finalByte = c;
            j += 1;
            break;
          } else {
            while (j < text.length) {
              const c2 = text.codePointAt(j) as number;
              j += 1;
              if (c2 >= 0x40 && c2 <= 0x7e) break;
            }
            break;
          }
        }
        if (finalByte === 0x6d) {
          flush();
          applySgr(params, state);
        }
        i = j;
      } else if (next === 0x5d) {
        // OSC: consume through BEL or ST (ESC \); an unterminated OSC
        // consumes the rest of the input.
        let j = i + 2;
        while (j < text.length) {
          const c = text.codePointAt(j);
          if (c === 0x07) {
            j += 1;
            break;
          }
          if (c === 0x1b && text.codePointAt(j + 1) === 0x5c) {
            j += 2;
            break;
          }
          j += 1;
        }
        i = j;
      } else {
        // Bare ESC: dropped without swallowing the next character.
        i += 1;
      }
      continue;
    }
    if (code === 0x09) {
      current += '    ';
      i += 1;
      continue;
    }
    if (code === 0x0a) {
      current += '\n';
      i += 1;
      continue;
    }
    if (code < 0x20 || (code >= 0x7f && code <= 0x9f)) {
      i += 1;
      continue;
    }
    current += String.fromCodePoint(code);
    i += code > 0xffff ? 2 : 1;
  }
  flush();
  return runs;
}

/** Plain-text projection of [`parseAnsi`]: exactly the visible text with
 *  every ESC/control sequence removed and tabs expanded. `ansiToPlainText(
 *  redactSecrets(text))` is what the clipboard copy writes — text only, no
 *  escape sequences. */
export function ansiToPlainText(text: string): string {
  let plain = '';
  for (const run of parseAnsi(text)) {
    plain += run.text;
  }
  return plain;
}

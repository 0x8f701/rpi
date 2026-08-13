// Safe React renderer for untrusted terminal text (see src/ansi.ts).
//
// The one-pass ANSI SGR parser splits the input into text runs with absolute
// styles; each run paints as a plain <span> with stable `ansi-*` classes for
// the base-16 palette and modifiers, plus inline sanitized rgb values for
// 256-color/truecolor. No `dangerouslySetInnerHTML`, no raw HTML anywhere:
// run text lands in the DOM as a React text node, so hostile content (tags,
// script payloads, control sequences) stays inert, and cursor movement/OSC
// hyperlinks/other terminal control bytes never execute in the UI.
//
// Redaction contract (fail-closed): the raw input is parsed first and the
// FULL plain text is then redacted. If redaction changed anything, a
// credential shape was present — it may have been split across an SGR
// boundary in the raw input — so the whole text collapses to ONE base-style
// plain run of the redacted text (no ANSI styles can carry a secret). If
// redaction changed nothing, the styled runs render normally. The clipboard
// copy path writes the same redacted plain text (`redactSecrets(
// ansiToPlainText(text))`).

import { type CSSProperties } from 'react';
import { ansiRgb, ansiToPlainText, parseAnsi } from './ansi';
import { redactSecrets } from './redact';

/** Render untrusted terminal/tool output as safe styled runs. */
export function AnsiText({ text }: { text: string }) {
  const runs = parseAnsi(text);
  const plain = ansiToPlainText(text);
  const redacted = redactSecrets(plain);
  if (redacted !== plain) {
    // Fail closed: a credential shape is present in the plain text (possibly
    // split across an SGR boundary in the raw input). Render the redacted
    // text as one plain run — no ANSI styling on this output.
    return <>{redacted}</>;
  }
  return (
    <>
      {runs.map((run, index) => {
        const classes: string[] = [];
        if (run.fg !== undefined && run.fg < 16) classes.push(`ansi-fg-${run.fg}`);
        if (run.bg !== undefined && run.bg < 16) classes.push(`ansi-bg-${run.bg}`);
        if (run.bold) classes.push('ansi-bold');
        if (run.dim) classes.push('ansi-dim');
        if (run.italic) classes.push('ansi-italic');
        if (run.underline) classes.push('ansi-underline');
        const style: CSSProperties = {};
        if (run.fgRgb !== undefined) style.color = run.fgRgb;
        else if (run.fg !== undefined && run.fg >= 16) style.color = ansiRgb(run.fg);
        if (run.bgRgb !== undefined) style.backgroundColor = run.bgRgb;
        else if (run.bg !== undefined && run.bg >= 16) style.backgroundColor = ansiRgb(run.bg);
        const className = classes.length > 0 ? classes.join(' ') : undefined;
        const styleProp = Object.keys(style).length > 0 ? style : undefined;
        return (
          <span key={index} className={className} style={styleProp}>
            {run.text}
          </span>
        );
      })}
    </>
  );
}

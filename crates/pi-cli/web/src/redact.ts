// XSS safety — TypeScript ports of the Rust export pipeline.
//
// escape_text    (crates/pi-coding/src/export/mod.rs): escape & < > " '
// redact_secrets (crates/pi-coding/src/redact.rs): credential shapes
//
// EVERY model-derived string passes through redactSecrets(); strings that
// cross into innerHTML additionally pass through escapeHtml(). Streaming
// deltas are appended via textContent (inherently safe). Model text is
// NEVER injected as raw HTML.

// Credential-shaped patterns, in application order (PEM block first so its
// multi-line body is consumed before narrower token patterns can match
// inside it). Mirrors the SECRET_PATTERNS table in crates/pi-coding/src/redact.rs.
const REDACT_PATTERNS: Array<[RegExp, string]> = [
  [/-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----/g, '[REDACTED]'],
  [/\bsk-[A-Za-z0-9_-]{16,}/g, '[REDACTED]'],
  [/\bgh[pousr]_[A-Za-z0-9]{20,}/g, '[REDACTED]'],
  [/\bgithub_pat_[A-Za-z0-9_]{20,}/g, '[REDACTED]'],
  [/\bAKIA[0-9A-Z]{16}/g, '[REDACTED]'],
  [/Authorization:\s*Bearer\s+[A-Za-z0-9_.\-]+/g, '[REDACTED]'],
  [/\bbearer\s+[A-Za-z0-9._~+/=-]{16,}/gi, '[REDACTED]'],
  [/(token|access_token)=[A-Za-z0-9_.\-]+/gi, '[REDACTED]'],
  [/\b(AWS_(?:ACCESS_KEY_ID|SECRET_ACCESS_KEY|SESSION_TOKEN))\s*[:=]\s*(?:"[^"]*"|'[^']*'|\S+)/gi, '$1=[REDACTED]'],
  [/(api[_-]?key|token|secret|password|authorization)\s*[:=]\s*(?:bearer\s+)?\S+/gi, '$1=[REDACTED]'],
  [/\bbearer\s+[a-z0-9._\-]+/gi, 'Bearer [REDACTED]'],
  [/\b(?:sk|pk|rk|ghp|gho|ghu|ghs|ghr|xox[baprs])[-_][A-Za-z0-9\-_]{8,}\b/gi, '[REDACTED]'],
];

export function redactSecrets(input: unknown): string {
  let out = String(input);
  for (const [pattern, replacement] of REDACT_PATTERNS) {
    out = out.replace(pattern, replacement);
  }
  return out;
}

export function escapeHtml(input: unknown): string {
  return redactSecrets(input)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

/** Safe text for textContent (redact only; escaping is unnecessary there). */
export function safeText(input: unknown): string {
  return redactSecrets(input == null ? '' : String(input));
}

/** Safe HTML for innerHTML (redact + escape). */
export function safeHtml(input: unknown): string {
  return escapeHtml(input == null ? '' : String(input));
}

/** JSON for tool-call argument cards: redact + escape, pretty-printed. */
export function safeJson(value: unknown): string {
  let text: string;
  try {
    text = JSON.stringify(value, null, 2);
  } catch {
    text = '{}';
  }
  return escapeHtml(text);
}

/** Link target policy: http/https/mailto or same-origin relative paths only. */
export function safeUrl(raw: unknown): string {
  const url = String(raw == null ? '' : raw).trim();
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(url)) {
    return /^(https?|mailto):/i.test(url) ? url : '';
  }
  if (url === '' || url === '#') return url;
  if (url.charAt(0) === '/' || url.indexOf('./') === 0 || url.indexOf('../') === 0) {
    return url;
  }
  return '';
}

/** Image data URIs only for whitelisted MIME types and base64 payloads. */
export function safeImage(mime: unknown, data: unknown): string {
  if (!/^image\/(png|jpeg|gif|webp)$/.test(String(mime || ''))) return '';
  if (!/^[A-Za-z0-9+/=\s]+$/.test(String(data || ''))) return '';
  return `data:${mime};base64,${data}`;
}

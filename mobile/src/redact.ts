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

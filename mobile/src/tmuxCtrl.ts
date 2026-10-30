export function applyStickyCtrl(text: string): { text: string; consumed: boolean } {
  if (text.length !== 1) return { text, consumed: false };
  let code = text.charCodeAt(0);
  if (code >= 97 && code <= 122) code -= 32;
  if (code < 64 || code > 95) return { text, consumed: false };
  return { text: String.fromCharCode(code - 64), consumed: true };
}

export function decodeTmuxB64(data: string): string {
  const binary = globalThis.atob(data);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return new TextDecoder().decode(bytes);
}

export function encodeTmuxB64(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let binary = '';
  for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
  return globalThis.btoa(binary);
}

export function mapTmuxInputB64(data: string, ctrlHeld: boolean): { data: string; consumed: boolean } {
  if (!ctrlHeld) return { data, consumed: false };
  const mapped = applyStickyCtrl(decodeTmuxB64(data));
  if (!mapped.consumed) return { data, consumed: false };
  return { data: encodeTmuxB64(mapped.text), consumed: true };
}

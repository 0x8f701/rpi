export function userContentPreview(content: unknown): string {
  if (typeof content === 'string' && content.trim() !== '') {
    return content;
  }
  if (!Array.isArray(content)) {
    return '';
  }
  const texts: string[] = [];
  let hasImage = false;
  for (const block of content) {
    if (!block || typeof block !== 'object') continue;
    const row = block as { type?: unknown; text?: unknown };
    if (row.type === 'text' && typeof row.text === 'string' && row.text !== '') {
      texts.push(row.text);
    } else if (row.type === 'image') {
      hasImage = true;
    }
  }
  const joined = texts.join('\n');
  if (joined.trim() !== '') return joined;
  return hasImage ? '(image)' : '';
}

export function steeringPreviewsFromMessages(messages: unknown): string[] {
  if (!Array.isArray(messages)) return [];
  const previews: string[] = [];
  for (const message of messages) {
    if (!message || typeof message !== 'object') continue;
    const row = message as { role?: unknown; content?: unknown };
    if (row.role !== 'user') continue;
    const preview = userContentPreview(row.content);
    if (preview !== '') previews.push(preview);
  }
  return previews;
}

export function collapseSteeringPreview(text: string): string {
  return text.split(/\s+/).filter(Boolean).join(' ');
}

export function dropSteeringAt(previews: string[], index: number): string[] {
  if (index < 0 || index >= previews.length) return previews;
  return previews.filter((_, i) => i !== index);
}

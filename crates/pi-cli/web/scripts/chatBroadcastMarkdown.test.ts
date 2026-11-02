#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const failures: string[] = [];
let assertions = 0;

function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const here = dirname(fileURLToPath(import.meta.url));
const channel = readFileSync(join(here, '../src/chat/BroadcastConversation.tsx'), 'utf8');
const styles = readFileSync(join(here, '../src/chat/styles.css'), 'utf8');

const messageBlockStart = channel.indexOf('{messages.map((message) => (');
const messageBlockEnd = channel.indexOf('{runs.map((run) => {');
const messageBlock = messageBlockStart >= 0 && messageBlockEnd > messageBlockStart
  ? channel.slice(messageBlockStart, messageBlockEnd)
  : '';

check('BroadcastConversation imports MarkdownBody from ../MarkdownBody',
  channel.includes("import { MarkdownBody } from '../MarkdownBody';"));
check('channel message map is present', messageBlock !== '');
check('channel messages render through MarkdownBody',
  messageBlock.includes('<MarkdownBody className="chat-session-markdown" text={message.text} onLayoutChange={pinIfPinned} />'));
check('bare paragraph safeText render is gone',
  !messageBlock.includes('<p>{safeText(message.text)}</p>')
  && !channel.includes('<p>{safeText(message.text)}</p>'));
check('author role line stays',
  messageBlock.includes('className="chat-session-message__role"')
  && messageBlock.includes("message.kind === 'human' ? 'You' : `@${message.author}`"));
check('human and runner share one MarkdownBody path',
  messageBlock.includes("message.kind === 'human'")
  && messageBlock.includes("chat-session-message--user")
  && messageBlock.includes("chat-session-message--assistant")
  && (messageBlock.match(/<MarkdownBody /g) ?? []).length === 1);

check('Channel slash rejection is unchanged',
  channel.includes("slash.name === 'goal'")
  && channel.includes("'Open a runner session and use /goal there.'")
  && channel.includes("`/${slash.name} is not available in chat.`")
  && channel.includes('const slash = parseSlashLine(message);')
  && channel.indexOf('parseSlashLine(message)') < channel.indexOf('await onSend(')
  && !channel.includes('parseSupportedCommand'));
check('Channel omits Sending-to-N-runners status',
  !channel.includes('Sending to')
  && !channel.includes('recipientLine')
  && !channel.includes('chat-broadcast__recipients')
  && !styles.includes('.chat-broadcast__recipients')
  && channel.includes('Send one message to the agent team. @ a runner to assign work.')
  && channel.includes('No runner named @')
  && channel.includes('matches more than one runner'));

check('chat-session-message padding is 8px 12px',
  /\.chat-session-message \{\s*padding:\s*8px 12px;/.test(styles));
check('chat-session-message role margin-bottom is 6px',
  /\.chat-session-message__role \{[^}]*margin-bottom:\s*6px;/.test(styles));
check('Files header refresh is pushed to the right edge in chat context',
  styles.includes('.chat-app .chat-context .file-explorer__refresh-btn {\n  margin-left: auto;\n}'));

const sessionView = readFileSync(join(here, '../src/chat/SessionView.tsx'), 'utf8');
check('SessionView assistant markdown uses chat-session-markdown',
  sessionView.includes('className="chat-session-markdown"'));

const mermaidViewport = styles.match(/\.chat-session-markdown \.md-mermaid-viewport \{([^}]*)\}/)?.[1] ?? '';
const mermaidSvg = styles.match(/\.chat-session-markdown \.md-mermaid-viewport svg,\n\.chat-session-markdown \.md-mermaid-viewport svg\[data-mermaid-wide="true"\] \{([^}]*)\}/)?.[1] ?? '';
const mermaidHost = styles.match(/\.chat-session-markdown \.md-mermaid-host \{([^}]*)\}/)?.[1] ?? '';
check('channel mermaid host is centered',
  /\btext-align:\s*center;/.test(mermaidHost), mermaidHost);
check('channel mermaid viewport is centered without scrollbars',
  /\bjustify-content:\s*center;/.test(mermaidViewport)
  && /\boverflow:\s*hidden;/.test(mermaidViewport)
  && /\bmax-width:\s*100%;/.test(mermaidViewport)
  && /\bmax-height:\s*60vh;/.test(mermaidViewport)
  && !/\boverflow-x:\s*auto;/.test(mermaidViewport), mermaidViewport);
check('channel mermaid svg scales inside the cap including wide diagrams',
  /\bmax-width:\s*100%;/.test(mermaidSvg)
  && /\bmax-height:\s*60vh;/.test(mermaidSvg)
  && /\bmargin-inline:\s*auto;/.test(mermaidSvg)
  && /\bobject-fit:\s*contain;/.test(mermaidSvg)
  && /\bmin-width:\s*0 !important;/.test(mermaidSvg), mermaidSvg);
check('channel mermaid toolbar stays visible for View/Download',
  !/\.chat-session-markdown \.md-mermaid-toolbar \{[^}]*display:\s*none/.test(styles)
  && styles.includes('.chat-session-markdown .md-mermaid-toolbar'));

if (failures.length > 0) {
  console.error(`chatBroadcastMarkdown.test: ${assertions} checks, ${failures.length} FAILURES`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(`chatBroadcastMarkdown.test: ${assertions} checks, 0 failures`);
process.exit(0);

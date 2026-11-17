#!/usr/bin/env node
// Open Files fills the available app body. The outer stored width, its
// resizer, and the overlay reservation are gone. Header and footer stay.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: unknown, detail?: string): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const here = dirname(fileURLToPath(import.meta.url));
const app = readFileSync(join(here, '..', 'src', 'App.tsx'), 'utf8');
const css = readFileSync(join(here, '..', 'src', 'styles.css'), 'utf8');
const explorerStart = css.indexOf('\n.file-explorer {');
const explorerBlock = explorerStart >= 0 ? css.slice(explorerStart, css.indexOf('\n}', explorerStart)) : '';

check('App no longer owns a stored outer Files width', !app.includes('FILE_EXPLORER_WIDTH_DEFAULT') && !app.includes('rpi-file-explorer-width') && !app.includes('file-explorer-resizer'));
check('App no longer flips an overlay class for a narrow drawer', !app.includes('fileExplorerOverlayActive') && !app.includes('app-layout--files-overlay'));
check(
  'open Files stretches from the left edge to the right edge',
  /left:\s*0/.test(explorerBlock)
    && /right:\s*0/.test(explorerBlock)
    && /width:\s*auto/.test(explorerBlock)
    && /max-width:\s*none/.test(explorerBlock),
  explorerBlock,
);
check(
  'open Files stays under the header and above the footer',
  /top:\s*var\(--app-header-height\)/.test(explorerBlock)
    && /bottom:\s*var\(--app-footer-height, 0px\)/.test(explorerBlock),
);
check('the outer width resizer and drawer reservation are gone', !css.includes('.file-explorer-resizer') && !css.includes('--file-explorer-width') && !css.includes('.app-layout--files-overlay'));
check('the inner tree split remains', css.includes('--file-explorer-tree-pct: 29%') && css.includes('flex: 0 0 var(--file-explorer-tree-pct, 29%)'));

console.log(`\nfileExplorerResize.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);

import fs from 'node:fs';

const path = new URL('../dist/index.html', import.meta.url);
const source = fs.readFileSync(path, 'utf8');
const cleaned = source
  .split('\n')
  .map((line) => line.trimEnd())
  .join('\n');
fs.writeFileSync(path, cleaned);

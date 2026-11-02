#!/usr/bin/env node
import { deflateRawSync } from 'node:zlib';
import {
  applyPlantumlServerFromCatalog,
  configurePlantumlServer,
  configuredPlantumlServer,
  encodePlantuml64,
  parsePlantumlServer,
  plantumlHttpErrorDetail,
  plantumlSvgRequestUrl,
} from '../src/plantumlEncode.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const raw = deflateRawSync('Bob -> Alice : hello');
check('plantuml encode64 known vector',
  encodePlantuml64(raw) === 'SyfFKj2rKt3CoKnELR1Io4ZDoSa70000',
  encodePlantuml64(raw));
check('plantuml encode64 zero triplet', encodePlantuml64(Uint8Array.of(0, 0, 0)) === '0000');
check('plantuml server empty is off', parsePlantumlServer('') === '');
check('plantuml server https accepted', parsePlantumlServer('https://plantuml.example/uml') === 'https://plantuml.example/uml');
check('plantuml server rejects ftp', parsePlantumlServer('ftp://x') === null);
check('plantuml server rejects credentials', parsePlantumlServer('https://user:pass@example.com/uml') === null);
check('plantuml svg url has no encoding prefix',
  plantumlSvgRequestUrl('https://plantuml.example/uml', 'SyfFKj2rKt3CoKnELR1Io4ZDoSa70000')
    === 'https://plantuml.example/uml/svg/SyfFKj2rKt3CoKnELR1Io4ZDoSa70000');

configurePlantumlServer('');
check('configure empty is off', configuredPlantumlServer() === '');
applyPlantumlServerFromCatalog({
  values: [{ definition: { key: 'plantumlServer' }, effectiveValue: 'https://plantuml.example/uml' }],
});
check('catalog apply sets server', configuredPlantumlServer() === 'https://plantuml.example/uml');
configurePlantumlServer('javascript:alert(1)');

const syntaxErrorSvg =
  '<svg><text>PlantUML version 1.2026.8</text>' +
  '<text>[From string (line 33) ]</text>' +
  '<text>@startuml</text>' +
  '<text>else (否)</text>' +
  '<text>Syntax Error? (Assumed diagram type: activity)</text></svg>';
const syntaxDetail = plantumlHttpErrorDetail(400, syntaxErrorSvg);
check('syntax error detail carries status', syntaxDetail.startsWith('plantuml HTTP 400: '), syntaxDetail);
check('syntax error detail names the diagram type', syntaxDetail.includes('Assumed diagram type: activity'), syntaxDetail);
check('syntax error detail carries the source line number', syntaxDetail.includes('at line 33'), syntaxDetail);
check('syntax error detail quotes the failing line', syntaxDetail.includes('else (否)'), syntaxDetail);
check('non-svg body falls back to bare status', plantumlHttpErrorDetail(500, 'gateway timeout') === 'plantuml HTTP 500');
check('welcome body reports decode failure',
  plantumlHttpErrorDetail(400, '<svg><text>Welcome to PlantUML!</text></svg>') ===
    'plantuml HTTP 400: the server could not decode the diagram payload');
check('success status ignores body', plantumlHttpErrorDetail(200, '<svg><text>Syntax Error?</text></svg>') === 'plantuml HTTP 200');
const longBody = plantumlHttpErrorDetail(400, syntaxErrorSvg.replace('else (否)', `else (${'-'.repeat(400)} 否)`));
check('long detail is bounded', longBody.length <= 243 && longBody.startsWith('plantuml HTTP 400: '));
check('invalid configure fail-closes to off', configuredPlantumlServer() === '');

if (failures.length > 0) {
  for (const failure of failures) console.error(`not ok - ${failure}`);
  process.exit(1);
}
console.log(`plantumlEncode.test: ${ran} assertions, 0 failure(s)`);

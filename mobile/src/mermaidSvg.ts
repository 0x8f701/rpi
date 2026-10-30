export const MERMAID_DOWNLOAD_FILE_NAME = 'mermaid-diagram.svg';
export const MAX_MERMAID_SOURCE_BYTES = 64 * 1024;

export function mermaidSourceBytes(source: string): number {
  return new TextEncoder().encode(source).length;
}

export function mermaidSourceWithinBound(source: string): boolean {
  return mermaidSourceBytes(source) <= MAX_MERMAID_SOURCE_BYTES;
}

export function sanitizeMermaidSvg(svg: string): string {
  if (typeof svg !== 'string' || !svg.includes('<svg')) return '';
  if (/<script[\s>]/i.test(svg) || /<foreignobject/i.test(svg) || /<iframe/i.test(svg)) return '';
  if (/\son[a-z]+\s*=/i.test(svg)) return '';
  if (/url\(\s*(['"]?)(?!#)/i.test(svg)) return '';
  if (/@import/i.test(svg)) return '';
  // javascript:/vbscript: hrefs run in the viewer document on click and never
  // reach the navigation callback, so they are rejected here.
  if (/javascript\s*:|vbscript\s*:/i.test(svg)) return '';
  return svg;
}

/** Navigation allowlist for the viewer document: its own about:blank document
 *  and the bundled android_asset tree (mermaid.min.js). Anything else — http,
 *  https, data, intent, file outside the asset tree — is blocked in-view and
 *  is never handed to the OS handler. */
export function viewerNavigationAllowed(url: string): boolean {
  const target = url.trim();
  if (target === 'about:blank') return true;
  return /^file:\/\/\/android_asset(\/|$)/.test(target);
}

/** The viewer's ONLY diagram injection path: sanitize the SVG first, then
 *  build the script that loads it into the page. A hostile SVG yields
 *  `{ clean: '', script: '' }`, so nothing unsanitized is ever injected. */
export function viewerDiagramInjection(rawSvg: string): { clean: string; script: string } {
  const clean = sanitizeMermaidSvg(rawSvg);
  if (clean === '') return { clean: '', script: '' };
  return { clean, script: `window.rpiViewer.showDiagram(${JSON.stringify(clean)});true;` };
}

/** Note for the viewer stage (textContent only) when RN rejects a diagram. */
export function viewerNoteScript(note: string): string {
  return `window.rpiViewer.showNote(${JSON.stringify(note)});true;`;
}

export function mermaidViewerHtml(source: string): string {
  const payload = JSON.stringify(source);
  return `<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width,initial-scale=1"/>
<style>
html,body{margin:0;height:100%;background:#101214;color:#e8eaed;}
#stage{padding:12px;overflow:auto;height:100%;box-sizing:border-box;}
svg{max-width:100%;height:auto;}
.note{padding:12px;font:14px sans-serif;color:#9aa0a6;}
</style>
</head>
<body>
<div id="stage"></div>
<script src="mermaid.min.js"></script>
<script>
var source = ${payload};
var stage = document.getElementById('stage');
function post(msg){window.ReactNativeWebView.postMessage(JSON.stringify(msg));}
function fail(reason){
  stage.textContent = String(reason);
  post({type:'error', reason: String(reason)});
}
window.rpiViewer = {
  showDiagram: function(svg){ stage.innerHTML = String(svg); },
  showNote: function(note){ stage.textContent = String(note); }
};
function start(){
  if (typeof mermaid === 'undefined') { fail('Mermaid failed to load'); return; }
  mermaid.initialize({ startOnLoad:false, securityLevel:'strict', theme:'dark' });
  mermaid.render('rpi-mermaid', source).then(function(out){
    var svg = out && out.svg ? String(out.svg) : '';
    if (svg.indexOf('<svg') < 0) { fail('Mermaid produced no SVG'); return; }
    post({type:'svg', svg: svg});
  }).catch(function(err){ fail(err && err.message ? err.message : String(err)); });
}
start();
</script>
</body>
</html>`;
}

export const TMUX_TERMINAL_HTML = `<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1,user-scalable=no"/>
<link rel="stylesheet" href="xterm.css"/>
<style>
html,body,#host{margin:0;height:100%;width:100%;background:#111111;color:#c8c8c8;}
body{font-family:"JetBrainsMono Nerd Font","JetBrainsMono NF","Symbols Nerd Font",monospace;}
#fallback{display:none;flex-direction:column;height:100%;}
#out{flex:1;overflow:auto;white-space:pre;padding:8px;font-size:12px;line-height:1.3;
  font-family:"JetBrainsMono Nerd Font","JetBrainsMono NF","Symbols Nerd Font",monospace;}
#in{border:0;background:#1a1d20;color:#c8c8c8;padding:8px;font-size:14px;
  font-family:"JetBrainsMono Nerd Font","JetBrainsMono NF","Symbols Nerd Font",monospace;}
</style>
</head>
<body>
<div id="host"></div>
<div id="fallback">
  <pre id="out"></pre>
  <input id="in" autocomplete="off" autocapitalize="off" spellcheck="false"/>
</div>
<script src="xterm.js"></script>
<script src="addon-fit.js"></script>
<script>
function post(msg){window.ReactNativeWebView.postMessage(JSON.stringify(msg));}
function strToB64(text){return btoa(unescape(encodeURIComponent(text)));}
function b64ToStr(b64){try{return decodeURIComponent(escape(atob(b64)));}catch(e){return atob(b64);}}
function startFallback(){
  document.getElementById('host').style.display='none';
  var fb=document.getElementById('fallback');
  fb.style.display='flex';
  var out=document.getElementById('out');
  var inp=document.getElementById('in');
  window.__rpiOutput=function(b64){out.textContent+=b64ToStr(b64);out.scrollTop=out.scrollHeight;};
  window.__rpiSoftKey=function(seq){post({type:'input',data:strToB64(seq)});};
  inp.addEventListener('keydown',function(ev){
    if(ev.key==='Enter'){post({type:'input',data:strToB64(inp.value+'\\n')});inp.value='';}
  });
  post({type:'ready',cols:Math.max(8,Math.floor(window.innerWidth/8)),rows:Math.max(4,Math.floor(window.innerHeight/16))});
}
function startXterm(){
  if(typeof Terminal==='undefined'){startFallback();return;}
  var term=new Terminal({
    cursorBlink:true,
    fontFamily:'"JetBrainsMono Nerd Font", "JetBrainsMono NF", "Symbols Nerd Font", monospace',
    fontSize:12,
    theme:{background:'#111111',foreground:'#c8c8c8'}
  });
  var FitCtor=window.FitAddon&&window.FitAddon.FitAddon;
  var fitAddon=FitCtor?new FitCtor():null;
  if(fitAddon) term.loadAddon(fitAddon);
  term.open(document.getElementById('host'));
  if(fitAddon) fitAddon.fit();
  window.__rpiOutput=function(b64){
    var raw=atob(b64);
    var bytes=new Uint8Array(raw.length);
    for(var i=0;i<raw.length;i++) bytes[i]=raw.charCodeAt(i);
    term.write(bytes);
  };
  window.__rpiSoftKey=function(seq){post({type:'input',data:strToB64(seq)});};
  term.onData(function(data){post({type:'input',data:strToB64(data)});});
  term.onResize(function(size){post({type:'resize',cols:size.cols,rows:size.rows});});
  window.addEventListener('resize',function(){if(fitAddon) fitAddon.fit();});
  var cols=Math.max(8,term.cols||80);
  var rows=Math.max(4,term.rows||24);
  post({type:'ready',cols:cols,rows:rows});
}
if(typeof Terminal==='undefined') startFallback();
else startXterm();
</script>
</body>
</html>`;

(function(){
  var root=document.documentElement;
  var btn=document.getElementById('theme-toggle');
  function current(){return root.getAttribute('data-theme')==='light'?'light':'dark'}
  function apply(t){root.setAttribute('data-theme',t);if(btn)btn.textContent=t==='light'?'Dark':'Light'}
  var stored;
  try{stored=localStorage.getItem('pi-export-theme')}catch(e){}
  if(stored==='light'||stored==='dark')apply(stored);else apply(current());
  if(btn)btn.addEventListener('click',function(){
    var next=current()==='light'?'dark':'light';
    apply(next);
    try{localStorage.setItem('pi-export-theme',next)}catch(e){}
  });
})();
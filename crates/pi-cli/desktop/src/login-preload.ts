import { ipcRenderer } from 'electron';

const attempt = process.argv.find((arg) => arg.startsWith('--rpi-login-attempt='))?.slice('--rpi-login-attempt='.length);
if (process.isMainFrame && attempt) {
  const send = (command: Record<string, unknown>) => ipcRenderer.send('rpi-login:command', { ...command, attempt });
  window.addEventListener('DOMContentLoaded', () => {
    const root = document.getElementById('login');
    if (!root) return;
    const status = document.createElement('section');
    const form = document.createElement('form');
    let activeInput: HTMLInputElement | HTMLSelectElement | undefined;
    const clearPrompt = () => {
      if (activeInput) activeInput.value = '';
      activeInput = undefined;
      form.onsubmit = null;
      form.replaceChildren();
    };
    const cancel = document.createElement('button');
    cancel.textContent = 'Cancel';
    cancel.onclick = () => { clearPrompt(); send({ type: 'cancel' }); };
    root.append(status, form, cancel);
    const close = document.createElement('button');
    close.className = 'auth-close';
    close.textContent = '×';
    close.title = 'Close sign in';
    close.setAttribute('aria-label', 'Close sign in');
    close.onclick = cancel.onclick;
    document.body.append(close);
    window.addEventListener('pagehide', clearPrompt);
    ipcRenderer.on('rpi-login:state', (_event, state) => {
      if (state.attempt !== attempt) return;
      const frame = state.frame;
      if (frame.type === 'prompt') {
        clearPrompt();
        const label = document.createElement('label');
        label.textContent = frame.prompt.message;
        const input = frame.prompt.type === 'select' ? document.createElement('select') : document.createElement('input');
        activeInput = input;
        if (input instanceof HTMLSelectElement) {
          for (const option of frame.prompt.options) {
            const item = document.createElement('option');
            item.value = option.id;
            item.textContent = option.label + (option.description ? ` — ${option.description}` : '');
            input.append(item);
          }
        } else {
          input.type = frame.prompt.type === 'secret' ? 'password' : 'text';
          input.autocomplete = 'off';
          input.spellcheck = false;
          input.placeholder = frame.prompt.placeholder ?? '';
        }
        const submit = document.createElement('button');
        submit.type = 'submit';
        submit.textContent = 'Continue';
        label.append(input);
        form.append(label, submit);
        form.onsubmit = (event) => {
          event.preventDefault();
          const value = input.value;
          clearPrompt();
          send({ type: 'response', id: frame.id, value });
        };
        input.focus();
      } else if (frame.type === 'event') {
        status.replaceChildren();
        const event = frame.event;
        const message = document.createElement('p');
        message.textContent = event.message ?? event.instructions ?? (event.type === 'device_code' ? `Enter code: ${event.user_code}` : 'Continue in your browser.');
        status.append(message);
        const urls = event.type === 'auth_url' ? [event.url] : event.type === 'device_code' ? [event.verification_uri] : (event.links ?? []).map((link: { url: string }) => link.url);
        const safeUrls = urls.filter((url: string) => { try { return ['http:', 'https:'].includes(new URL(url).protocol); } catch { return false; } });
        safeUrls.forEach((url: string, index: number) => {
          const open = document.createElement('button');
          open.textContent = `Open in browser: ${url}`;
          open.onclick = () => send({ type: 'open', index, eventId: state.eventId });
          status.append(open);
        });
      } else {
        clearPrompt();
        status.textContent = frame.type === 'complete' ? 'Credentials saved. Waiting for login to exit…' : 'Sign in did not complete.';
      }
    });
    send({ type: 'ready' });
  });
}

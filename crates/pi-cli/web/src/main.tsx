import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { CollabGuestView } from './CollabGuestView';
import { parseCollabLocation, COLLAB_PATH_PREFIX } from './collab';
import './styles.css';

function renderApp(): void {
  const root = document.getElementById('root');
  if (!root) throw new Error('missing #root element');
  // A collab guest route is any document served at /collab/ws/<roomId>.
  // Parse the role capability exactly once. Once decoded into its owner,
  // remove the fragment from the visible URL and current history entry before
  // any render or async connection work. The link then lives only in React's
  // guest prop; there is no module-scope capability reference.
  //
  // A malformed (or missing) capability fragment on a collab route STILL
  // renders the guest view — with a null link, CollabGuestView shows its safe
  // malformed-link error instead of ever mounting the host App — and the
  // fragment is scrubbed even when parsing failed, so capability-shaped bytes
  // never linger in the address bar. Only non-collab documents (the normal
  // /web session UX) fall back to <App /> when parse returns null.
  const isCollabRoute = window.location.pathname.startsWith(COLLAB_PATH_PREFIX);
  const collabLink = parseCollabLocation();
  if (isCollabRoute && window.location.hash) {
    window.history.replaceState(null, '', `${window.location.pathname}${window.location.search}`);
  }
  createRoot(root).render(
    <StrictMode>
      {isCollabRoute ? <CollabGuestView link={collabLink} /> : <App />}
    </StrictMode>
  );
}

renderApp();
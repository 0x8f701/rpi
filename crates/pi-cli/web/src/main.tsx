import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { CollabGuestView } from './CollabGuestView';
import { parseCollabLocation } from './collab';
import './styles.css';

function renderApp(): void {
  const root = document.getElementById('root');
  if (!root) throw new Error('missing #root element');
  // Parse the role capability exactly once. Once decoded into its owner,
  // remove the fragment from the visible URL and current history entry before
  // any render or async connection work. The link then lives only in React's
  // guest prop; there is no module-scope capability reference.
  const collabLink = parseCollabLocation();
  if (collabLink && window.location.hash) {
    window.history.replaceState(null, '', `${window.location.pathname}${window.location.search}`);
  }
  createRoot(root).render(
    <StrictMode>
      {collabLink ? <CollabGuestView link={collabLink} /> : <App />}
    </StrictMode>
  );
}

renderApp();
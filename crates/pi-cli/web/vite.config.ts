import { defineConfig, type ProxyOptions } from 'vite';
import react from '@vitejs/plugin-react';
import { viteSingleFile } from 'vite-plugin-singlefile';

// Dev workflow: `npm run dev` (vite dev server on :5173). It proxies the
// control-plane WebSocket and HTTP RPC so the page connects to the vite
// origin, which forwards to the rpi listener. Point it at your listener with
// RPI_LISTEN (default http://127.0.0.1:8765), e.g.:
//   RPI_LISTEN=http://127.0.0.1:9876 npm run dev
//
// The tokenless listener enforces a same-origin browser check (DNS-rebinding
// defense): a browser at http://localhost:5173 would be rejected against a
// listener at http://127.0.0.1:8765. The dev proxy therefore strips the
// browser `Origin` header before forwarding, so the listener treats proxied
// dev requests as native clients (no `Origin`) — accepted tokenlessly on
// loopback, and accepted with a token on any bind. The embedded `/web` page
// is same-origin to its own listener and is unaffected.
const listen = process.env.RPI_LISTEN || 'http://127.0.0.1:8765';
const wsTarget = listen.replace(/^http/, 'ws');

// Strip the browser Origin header on every proxied request (HTTP fetch and
// WebSocket upgrade) so the tokenless same-origin listener sees a native
// client. `changeOrigin` rewrites the Host header to the target's. The proxy
// server type is derived from Vite's `ProxyOptions['configure']` so the
// callback stays structurally compatible with the Vite proxy types.
type ViteProxyServer = Parameters<NonNullable<ProxyOptions['configure']>>[0];
const stripOrigin = (proxy: ViteProxyServer) => {
  proxy.on('proxyReq', (proxyReq) => {
    proxyReq.removeHeader('origin');
  });
};

export default defineConfig({
  plugins: [react(), viteSingleFile()],
  build: {
    target: 'es2022',
    assetsInlineLimit: 100000000,
    chunkSizeWarningLimit: 2000,
    cssCodeSplit: false,
  },
  server: {
    port: 5173,
    proxy: {
      '/ws': {
        target: wsTarget,
        ws: true,
        changeOrigin: true,
        configure: stripOrigin,
      },
      '/rpc': {
        target: listen,
        changeOrigin: true,
        configure: stripOrigin,
      },
    },
  },
});

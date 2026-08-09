import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { viteSingleFile } from 'vite-plugin-singlefile';

// Dev workflow: `npm run dev` (vite dev server on :5173). It proxies the
// control-plane WebSocket and HTTP RPC so the page connects to the vite
// origin, which forwards to the rpi listener. Point it at your listener with
// RPI_LISTEN (default http://127.0.0.1:8765), e.g.:
//   RPI_LISTEN=http://127.0.0.1:9876 npm run dev
const listen = process.env.RPI_LISTEN || 'http://127.0.0.1:8765';
const wsTarget = listen.replace(/^http/, 'ws');

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
      },
      '/rpc': {
        target: listen,
        changeOrigin: true,
      },
    },
  },
});

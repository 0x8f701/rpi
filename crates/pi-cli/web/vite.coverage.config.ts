import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { viteSingleFile } from 'vite-plugin-singlefile';

// Coverage build — used ONLY by the hard coverage command (E2E.d/web/coverage.sh).
// The normal `npm run build` (vite.config.ts) never loads this file, so the
// production bundle stays uninstrumented and minified.
//
// What differs from the normal build:
//   - build.minify: false        keep readable code so V8 coverage function
//                                names and ranges are useful
//   - build.sourcemap: 'inline'  a data-URI source map embedded in the bundle
//                                so the V8 -> Istanbul conversion can map every
//                                executed range back to the original TS/TSX
//                                source (crates/pi-cli/web/src/**)
//
// The output stays a single self-contained `index.html` (vite-plugin-singlefile)
// because the fixture's `rpi --listen` serves only `RPI_WEB_DEV_DIR/index.html`.
// Output goes to `$RPI_COVERAGE_OUT` (default `dist-coverage/`), never `dist/`.
const outDir = process.env.RPI_COVERAGE_OUT || 'dist-coverage';

export default defineConfig({
  plugins: [react(), viteSingleFile()],
  build: {
    target: 'es2022',
    assetsInlineLimit: 100000000,
    chunkSizeWarningLimit: 2000,
    cssCodeSplit: false,
    minify: false,
    sourcemap: 'inline',
    outDir,
    emptyOutDir: true,
  },
});

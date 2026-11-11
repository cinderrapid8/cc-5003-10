import { defineConfig } from 'vite';

// The built bundle is served either by looper-server (axum ServeDir) or by
// the Tauri webview (frontendDist in tauri.conf.json). Relative base keeps
// asset URLs valid in both.
export default defineConfig({
  base: './',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    port: 5173,
    proxy: {
      '/api': 'http://127.0.0.1:8088',
      '/files': 'http://127.0.0.1:8088',
    },
  },
});

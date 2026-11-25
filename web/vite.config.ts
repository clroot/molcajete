import {defineConfig} from 'vite';
import react from '@vitejs/plugin-react';
import stylex from '@stylexjs/unplugin';

// `bun run dev` serves the UI with hot reload and forwards the API and the
// tunnel to a gateway on 127.0.0.1:8080 (override with MOLCAJETE_GATEWAY).
const gateway = process.env.MOLCAJETE_GATEWAY ?? 'http://127.0.0.1:8080';

export default defineConfig({
  // Relative URLs, so the build also works below a path prefix.
  base: './',
  plugins: [stylex.vite(), react()],
  server: {
    proxy: {
      '/api': {target: gateway, changeOrigin: false},
      '/websocket-tunnel': {target: gateway.replace(/^http/, 'ws'), ws: true},
    },
  },
  build: {outDir: 'dist', emptyOutDir: true, chunkSizeWarningLimit: 1500},
});

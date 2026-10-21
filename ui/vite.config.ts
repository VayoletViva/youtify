import { defineConfig } from 'vite';

// Tauri supplies a reachable LAN address for physical Android devices.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  clearScreen: false,
  server: {
    host: host || '0.0.0.0',
    port: 1420,
    strictPort: true,
    hmr: host ? { protocol: 'ws', host, port: 1421 } : undefined,
    watch: { ignored: ['**/crates/**'] },
  },
  build: { target: 'es2022', sourcemap: false },
});
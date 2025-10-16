import { defineConfig } from 'vite';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const rootDir = dirname(fileURLToPath(import.meta.url));

export default defineConfig({
  resolve: {
    alias: {
      '@twi/shared-state': resolve(rootDir, '../shared/src'),
    },
  },
  server: {
    host: true,
    port: 5174,
  },
});

import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// For a GitHub Pages *project* site the app is served from /<repo>/, so assets
// must be requested from that sub-path. CI passes BASE_PATH=/<repo>/; locally
// (dev / preview) we fall back to the repo name, and "/" works for a user/org
// site or a custom domain.
const base = process.env.BASE_PATH || '/holon/';

export default defineConfig({
  base,
  plugins: [svelte()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2020'
  }
});

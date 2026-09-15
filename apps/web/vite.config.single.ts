import { defineConfig, type Plugin } from 'vite';
import preact from '@preact/preset-vite';
import { viteSingleFile } from 'vite-plugin-singlefile';
import { readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';

/**
 * Inline web workers as blob URLs into the main bundle.
 *
 * `new Worker(new URL('./x.worker.ts', import.meta.url))` makes Vite emit
 * worker files next to the HTML, which breaks a standalone single file. This
 * hook (running before `vite-plugin-singlefile`) swaps those references for
 * `URL.createObjectURL(new Blob([...]))` and drops the worker chunks.
 */
function inlineWorkers(): Plugin {
  let outDir = '';
  return {
    name: 'singlefile:inline-workers',
    enforce: 'post',
    configResolved(config) {
      outDir = resolve(config.root, config.build.outDir);
    },
    // Worker bundles are emitted by separate Rollup builds, so they are only
    // visible once everything is written: patch the finished HTML instead.
    closeBundle() {
      const workers = readdirSync(outDir).filter((f) => /\.worker-[\w-]+\.js$/.test(f));
      if (workers.length === 0) return;

      const htmlPath = resolve(outDir, 'index.html');
      let html = readFileSync(htmlPath, 'utf8');

      for (const file of workers) {
        const base64 = readFileSync(resolve(outDir, file)).toString('base64');
        // Wrapped in `new URL(...)` so a trailing `.href` (added by Vite's
        // worker plugin) keeps working.
        const blob = `new URL(URL.createObjectURL(new Blob([Uint8Array.from(atob("${base64}"),c=>c.charCodeAt(0))],{type:"text/javascript"})))`;
        const name = file.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
        html = html.replace(
          new RegExp(`new URL\\(["'](?:\\./)?${name}["'],\\s*import\\.meta\\.url\\)`, 'g'),
          blob,
        );
        // `?worker&url` preload constants: preloading is disabled in this
        // build, so blank the now-dangling references.
        html = html.replace(new RegExp(`["'](?:\\./)?${name}["']`, 'g'), '""');
        rmSync(resolve(outDir, file));
      }

      // The bundled workers are classic (iife) scripts, so the app's
      // `{ type: 'module' }` worker option must not survive: a module worker
      // from a blob URL fails to start under a file:// (null) origin.
      html = html.replace(/,{type:"module"}\)/g, ')');

      writeFileSync(htmlPath, html);
      console.log(`inlined ${workers.length} worker bundles into index.html`);
    },
  };
}

/**
 * Guest-side single-file build.
 *
 * Bundles the sender app (JS, CSS, both WASM codecs, workers) into a single
 * self-contained `dist-single/index.html` that can be copied into an offline
 * VM and opened directly from disk.
 *
 * Differences from the normal build:
 * - `vite-plugin-singlefile` inlines every emitted asset (WASM as data URLs).
 * - `worker.format: 'iife'` so worker chunks can be inlined too.
 * - `--mode singlefile` makes `preloadRuntimeAssets`/service worker registration
 *   no-ops (there is no sw.js or manifest next to a standalone HTML).
 */
export default defineConfig({
  plugins: [
    preact({ resolveModuleFormat: false }),
    {
      // The manifest/icon live in `public/` and would be missing next to a
      // standalone HTML file, so drop the dead references.
      name: 'singlefile:strip-external-links',
      transformIndexHtml(html: string) {
        return html.replace(/[ \t]*<link[^>]*rel="(?:manifest|icon)"[^>]*>\s*\n/g, '');
      },
    },
    inlineWorkers(),
    viteSingleFile(),
  ],
  resolve: {
    alias: {
      '@': resolve(__dirname, 'src'),
    },
  },
  worker: {
    format: 'iife',
  },
  base: './',
  // Nothing from `public/` belongs next to a standalone HTML file.
  publicDir: false,
  build: {
    outDir: 'dist-single',
    emptyOutDir: true,
    target: 'es2022',
  },
});

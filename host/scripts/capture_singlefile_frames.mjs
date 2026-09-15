//! Drive the single-file guest sender in headless Chromium (via CDP) and
//! capture its rendered QR canvas frames, for the Rust end-to-end test.
//!
//! Usage:
//!   node scripts/capture_singlefile_frames.mjs <app.html> <outDir> [payloadFile]
//!
//! Writes into <outDir>:
//!   frames/frame-NNN.png   canvas snapshots (real QR stream, as displayed)
//!   payload.txt            the text handed to the sender
//!
//! Requires a Chromium-based browser; set CHROME_BIN to override the default.

import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));

const [htmlArg, outArg, payloadArg] = process.argv.slice(2);
if (!htmlArg || !outArg) {
  console.error('usage: node scripts/capture_singlefile_frames.mjs <app.html> <outDir> [payloadFile]');
  process.exit(1);
}
const htmlPath = resolve(htmlArg);
const outDir = resolve(outArg);
// Default payload: deterministic pseudo-random hex so deflate cannot collapse
// it into a single symbol — the test then exercises several QR frames.
function defaultPayload() {
  let state = 0x2f6e2b1;
  const chars = 'RaptorQR-端到端验证-0123456789abcdef';
  let out = '';
  for (let i = 0; i < 4200; i++) {
    state = (state * 1664525 + 1013904223) >>> 0;
    out += chars[(state >>> 16) % chars.length];
  }
  return out + '-payload-marker-end';
}

const payload = payloadArg ? readFileSync(resolve(payloadArg), 'utf8') : defaultPayload();
const payloadName = payloadArg ? basename(resolve(payloadArg)) : 'e2e-payload.txt';

const CHROME_CANDIDATES = [
  process.env.CHROME_BIN,
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
  '/Applications/Chromium.app/Contents/MacOS/Chromium',
].filter(Boolean);

const chromeBin = CHROME_CANDIDATES.find((p) => existsSync(p));
if (!chromeBin) {
  console.error('no Chromium-based browser found; set CHROME_BIN');
  process.exit(1);
}

const PORT = 9333;
const PROFILE = join(outArg, '.chrome-profile');
mkdirSync(outDir, { recursive: true });
rmSync(PROFILE, { recursive: true, force: true });
const framesDir = join(outDir, 'frames');
rmSync(framesDir, { recursive: true, force: true });
mkdirSync(framesDir, { recursive: true });
writeFileSync(join(outDir, 'payload.txt'), payload);

const VISIBLE = process.env.VISIBLE === '1';
const HOLD_MS = Number(process.env.HOLD_MS ?? 0);

const chrome = spawn(
  chromeBin,
  [
    ...(VISIBLE
      ? [
          // Right edge of the screen: clear of the receiver window, which
          // opens centered, so one full QR tile column stays visible.
          '--window-position=1360,0',
          '--window-size=350,1050',
          '--disable-features=Translate',
        ]
      : ['--headless=new', '--disable-gpu']),
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-extensions',
    `--user-data-dir=${PROFILE}`,
    `--remote-debugging-port=${PORT}`,
    'about:blank',
  ],
  { stdio: 'ignore' },
);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitForDevTools() {
  for (let i = 0; i < 100; i++) {
    try {
      const res = await fetch(`http://127.0.0.1:${PORT}/json/version`);
      if (res.ok) return;
    } catch {
      /* not up yet */
    }
    await sleep(100);
  }
  throw new Error('DevTools endpoint did not come up');
}

async function openTarget(url) {
  const res = await fetch(`http://127.0.0.1:${PORT}/json/new?${encodeURIComponent(url)}`, {
    method: 'PUT',
  });
  if (!res.ok) throw new Error(`failed to open target: HTTP ${res.status}`);
  return res.json();
}

function connect(wsUrl) {
  return new Promise((resolvePromise, reject) => {
    const ws = new WebSocket(wsUrl);
    const pending = new Map();
    let nextId = 1;
    ws.addEventListener('open', () =>
      resolvePromise({
        send(method, params = {}) {
          const id = nextId++;
          return new Promise((res, rej) => {
            pending.set(id, { res, rej });
            ws.send(JSON.stringify({ id, method, params }));
          });
        },
        close: () => ws.close(),
      }),
    );
    ws.addEventListener('error', reject);
    ws.addEventListener('message', (event) => {
      const msg = JSON.parse(event.data);
      const entry = pending.get(msg.id);
      if (!entry) return;
      pending.delete(msg.id);
      if (msg.error) entry.rej(new Error(`${msg.error.message}`));
      else entry.res(msg.result);
    });
  });
}

async function main() {
  await waitForDevTools();
  const target = await openTarget('about:blank');
  const cdp = await connect(target.webSocketDebuggerUrl);

  const evaluate = async (expression) => {
    const result = await cdp.send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
    });
    if (result.exceptionDetails) {
      throw new Error(`page error: ${result.exceptionDetails.text} ${JSON.stringify(result.exceptionDetails.exception?.description ?? '')}`);
    }
    return result.result.value;
  };

  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');
  await cdp.send('Page.navigate', { url: `file://${htmlPath}` });

  // The app boots asynchronously (asset preload is skipped in this build).
  await sleep(1500);
  const ready = await evaluate(`
    (async () => {
      for (let i = 0; i < 100; i++) {
        if (document.body.innerText.includes('Start Live QR')) return true;
        await new Promise((r) => setTimeout(r, 100));
      }
      return false;
    })()
  `);
  if (!ready) throw new Error('sender UI did not render');

  // Collect page-side failures so a stalled transfer is diagnosable.
  await evaluate(`
    window.__errors = [];
    window.addEventListener('error', (e) => window.__errors.push('error: ' + e.message));
    window.addEventListener('unhandledrejection', (e) => window.__errors.push('rejection: ' + (e.reason?.message ?? String(e.reason))));
    true
  `);

  // Hand the payload to the file input, then start the live QR stream.
  await evaluate(`
    (async () => {
      const text = ${JSON.stringify(payload)};
      const input = document.querySelector('input[type="file"]');
      if (!input) throw new Error('file input not found');
      const dt = new DataTransfer();
      dt.items.add(new File([new TextEncoder().encode(text)], ${JSON.stringify(payloadName)}, { type: 'text/plain' }));
      input.files = dt.files;
      input.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 300));
      const start = [...document.querySelectorAll('button')].find((b) => b.textContent.includes('Start Live QR'));
      if (!start) throw new Error('start button not found');
      start.click();
      return true;
    })()
  `);

  const canvasReady = await evaluate(`
    (async () => {
      for (let i = 0; i < 200; i++) {
        const c = document.querySelector('canvas.qr-live-canvas');
        if (c && c.width > 0) return { w: c.width, h: c.height };
        await new Promise((r) => setTimeout(r, 100));
      }
      return null;
    })()
  `);
  if (!canvasReady) {
    const errors = await evaluate('JSON.stringify(window.__errors ?? [])');
    const text = await evaluate('document.body.innerText.replace(/\\n+/g, " | ").slice(0, 1200)');
    throw new Error(`live QR canvas never appeared\nerrors: ${errors}\npage: ${text}`);
  }
  console.log(`canvas: ${canvasReady.w}×${canvasReady.h}`);

  if (VISIBLE) {
    // A screen-capture receiver can only see what is actually on top.
    await cdp.send('Page.bringToFront');
    await evaluate('window.focus(); true');
    // Make sure the whole QR canvas is on screen, not clipped by the window.
    await evaluate(
      `(() => {
         const el = document.querySelector('canvas.qr-live-canvas');
         el.scrollIntoView({ block: 'center', inline: 'start' });
         document.scrollingElement.scrollLeft = 0;
         return true;
       })()`,
    );
    await sleep(700);
  }

  // Snapshot the canvas while the stream loops. Duplicates are fine: the Rust
  // decoder dedupes by (block, ESI).
  const frameCount = 40;
  const unique = new Set();
  let saved = 0;
  for (let i = 0; i < frameCount; i++) {
    const snapshot = await evaluate(`
      (() => {
        const c = document.querySelector('canvas.qr-live-canvas');
        const ctx = c.getContext('2d');
        const d = ctx.getImageData(0, 0, c.width, c.height).data;
        let drawn = 0;
        for (let i = 3; i < d.length; i += 4 * 53) if (d[i] !== 0) drawn++;
        return drawn > 20 ? c.toDataURL('image/png') : null;
      })()
    `);
    await sleep(45);
    if (!snapshot || unique.has(snapshot)) continue;
    const dataUrl = snapshot;
    void dataUrl;
    unique.add(dataUrl);
    writeFileSync(
      join(framesDir, `frame-${String(saved).padStart(3, '0')}.png`),
      Buffer.from(dataUrl.split(',')[1], 'base64'),
    );
    saved++;
  }
  console.log(`captured ${saved} distinct canvas frames → ${framesDir}`);

  const status = await evaluate(`document.body.innerText.split('\\n').filter((l) => /packet|frame|fps|QR/i.test(l)).slice(0, 5).join(' | ')`);
  console.log('page status:', status);

  if (HOLD_MS > 0) {
    console.log(`holding browser open for ${HOLD_MS} ms`);
    // Keep the QR window above whatever else the user/terminal raises.
    const until = Date.now() + HOLD_MS;
    while (Date.now() < until) {
      await sleep(3000);
      try {
        await cdp.send('Page.bringToFront');
      } catch {
        break;
      }
    }
  }

  cdp.close();
  chrome.kill('SIGKILL');
  rmSync(PROFILE, { recursive: true, force: true });
}

main()
  .catch((err) => {
    console.error(err.message ?? err);
    chrome.kill('SIGKILL');
    process.exitCode = 1;
  })
  .finally(() => {
    setTimeout(() => process.exit(process.exitCode ?? 0), 200);
  });

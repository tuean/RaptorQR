//! Generate cross-language decode fixtures with the real sender-side RaptorQ
//! WASM codec: produces `<name>.packets.bin` (transport packets) plus
//! `<name>.expected.bin` (original payload) under tests/fixtures/.
//!
//! Run: `node scripts/encode_fixture.mjs` (needs `pnpm install` first).
//!
//! This deliberately reimplements only the thin transport wrapper (8-byte
//! header + CRC32C, mirroring packet.ts) so the Rust receiver is validated
//! against the exact on-the-wire byte format.

import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import init, { encode_packets } from '@raptorqr/raptorq-wasm';
import fastQrInit, { QrRenderer } from '@raptorqr/fast-qr-wasm';

const __dirname = dirname(fileURLToPath(import.meta.url));
const require = createRequire(
  join(__dirname, '..', '..', 'packages', 'raptorqr-core', 'package.json'),
);
const { deflateSync } = require('fflate');

const FIXTURES = join(__dirname, '..', 'tests', 'fixtures');
mkdirSync(FIXTURES, { recursive: true });

const MAGIC = 0x51;
const SYMBOL_INDEX = 31;
const MAX_PAYLOAD = 201; // V10-M default from protocol/constants.ts

// ── Transport wrapper (port of packet.ts) ──────────────────────────────────

function crc32c(data) {
  let crc = 0xffffffff;
  for (const byte of data) {
    crc ^= byte;
    for (let i = 0; i < 8; i++) {
      crc = crc & 1 ? (crc >>> 1) ^ 0x82f63b78 : crc >>> 1;
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function createPacket(payload, { isText, isLast, compressed, dataLength, total }) {
  let word = 0;
  word |= 0; // generationIndex
  word |= (0xfff & total) << 12; // totalGenerations (capped)
  word |= (SYMBOL_INDEX & 0x1f) << 24;
  word |= (isText ? 1 : 0) << 29;
  word |= (isLast ? 1 : 0) << 30;
  word |= (compressed ? 1 : 0) << 31;
  const header = [MAGIC, word & 0xff, (word >>> 8) & 0xff, (word >>> 16) & 0xff, (word >>> 24) & 0xff];
  header.push(dataLength & 0xff, (dataLength >>> 8) & 0xff, (dataLength >>> 16) & 0xff);
  const body = new Uint8Array([...header, ...payload]);
  const crc = crc32c(body);
  const packet = new Uint8Array(body.length + 4);
  packet.set(body, 0);
  // Little-endian trailer, matching packet.ts writeUint32LE.
  packet.set([crc & 0xff, (crc >>> 8) & 0xff, (crc >>> 16) & 0xff, (crc >>> 24) & 0xff], body.length);
  return packet;
}

// ── Preprocess (port of preprocess_payload.ts) ─────────────────────────────

function preprocess(data, { isText, compress, filename, mimeType }) {
  let wrapped;
  if (!isText && filename) {
    const nameBytes = new TextEncoder().encode(filename);
    const mimeBytes = new TextEncoder().encode(mimeType || 'application/octet-stream');
    const nameLen = Math.min(nameBytes.length, 255);
    const mimeLen = Math.min(mimeBytes.length, 255);
    wrapped = new Uint8Array(2 + nameLen + mimeLen + data.length);
    let off = 0;
    wrapped[off++] = nameLen;
    wrapped.set(nameBytes.slice(0, nameLen), off);
    off += nameLen;
    wrapped[off++] = mimeLen;
    wrapped.set(mimeBytes.slice(0, mimeLen), off);
    off += mimeLen;
    wrapped.set(data, off);
  } else {
    wrapped = new Uint8Array(data);
  }
  if (compress && wrapped.length > 64) {
    const compressed = deflateSync(wrapped);
    if (compressed.length < wrapped.length) {
      return { data: compressed, isCompressed: true };
    }
  }
  return { data: wrapped, isCompressed: false };
}

// ── QR frame rendering (fast_qr, same settings as the web sender) ──────────
// web sender: LIVE_TARGET_PX=360, QR_QUIET_ZONE_MODULES=4, default V20 + EC L.
const QR_VERSION = 20;
const QR_ECC_L = 0;
const QR_SCALE = 3;
const QR_QUIET_MODULES = 4;

let qrRenderer = null;
let qrMemory = null;

function renderQrFrame(packet) {
  const side = qrRenderer.render_rgba(packet, QR_VERSION, QR_ECC_L, QR_SCALE);
  const src = new Uint8Array(
    qrMemory.memory.buffer,
    qrRenderer.rgba_ptr(),
    side * side * 4,
  );
  // fast_qr's RGBA output already includes the quiet zone (the web sender draws
  // it 1:1 into the tile), so no extra padding is added here.
  const pad = 0;
  void QR_QUIET_MODULES;
  const total = side + pad * 2;
  const out = new Uint8Array(total * total * 4).fill(0xff); // white background
  for (let y = 0; y < side; y++) {
    out.set(
      src.subarray(y * side * 4, (y + 1) * side * 4),
      ((y + pad) * total + pad) * 4,
    );
  }
  return { w: total, h: total, rgba: out };
}

function writeFrames(name, transport, count) {
  const frames = [];
  for (let i = 0; i < Math.min(count, transport.length); i++) {
    frames.push(renderQrFrame(transport[i]));
  }
  const header = Buffer.alloc(4 + 8 * frames.length);
  header.writeUInt32LE(frames.length, 0);
  frames.forEach((f, i) => {
    header.writeUInt32LE(f.w, 4 + i * 8);
    header.writeUInt32LE(f.h, 8 + i * 8);
  });
  writeFileSync(
    join(FIXTURES, `${name}.frames.bin`),
    Buffer.concat([header, ...frames.map((f) => Buffer.from(f.rgba))]),
  );
  console.log(`  → ${frames.length} QR frames of ${frames[0].w}×${frames[0].h} (V${QR_VERSION}, EC L, scale ${QR_SCALE})`);
}

// ── Fixture writer ─────────────────────────────────────────────────────────

function writeFixture(name, original, { isText, compress, filename, mimeType, renderFrames = 0 }) {
  const pre = preprocess(original, { isText, compress, filename, mimeType });
  const codecPackets = encode_packets(pre.data, MAX_PAYLOAD, 10);
  const transport = codecPackets.map((p, i) =>
    createPacket(new Uint8Array(p), {
      isText,
      isLast: i === codecPackets.length - 1,
      compressed: pre.isCompressed,
      dataLength: pre.data.length,
      total: codecPackets.length,
    }),
  );
  // All packets in one transfer have identical length.
  const packetLen = transport[0].length;
  const blob = Buffer.concat([Buffer.from([packetLen & 0xff, (packetLen >>> 8) & 0xff, (packetLen >>> 16) & 0xff, (packetLen >>> 24) & 0xff]), ...transport.map((p) => Buffer.from(p))]);
  writeFileSync(join(FIXTURES, `${name}.packets.bin`), blob);
  writeFileSync(join(FIXTURES, `${name}.expected.bin`), Buffer.from(original));
  if (renderFrames) writeFrames(name, transport, renderFrames);
  console.log(
    `${name}: ${original.length} B original → ${pre.data.length} B preprocessed → ${codecPackets.length} packets (${packetLen} B each), compressed=${pre.isCompressed}`,
  );
}

function lcg(seed) {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return (s >>> 16) & 0xff;
  };
}

// ── Build fixtures ─────────────────────────────────────────────────────────

const wasmBytes = readFileSync(
  join(__dirname, '..', '..', 'packages', 'raptorqr-raptorq-wasm', 'src', 'wasm', 'raptorqr_raptorq_wasm_bg.wasm'),
);
await init({ module_or_path: wasmBytes });

const fastQrBytes = readFileSync(
  join(__dirname, '..', '..', 'packages', 'raptorqr-fast-qr-wasm', 'src', 'wasm', 'raptorqr_fast_qr_wasm_bg.wasm'),
);
qrMemory = await fastQrInit({ module_or_path: fastQrBytes });
qrRenderer = new QrRenderer();

// 1. Compressible text (tests deflate + text path).
const lorem = 'RaptorQR 高速二维码传输 RaptorQ fountain code '.repeat(4000);
writeFixture('text', new TextEncoder().encode(lorem), {
  isText: true,
  compress: true,
  renderFrames: 5,
});

// 2. Incompressible file (tests filename/MIME preamble + uncompressed path).
const rng = lcg(0x1234abcd);
const fileData = new Uint8Array(250_000);
for (let i = 0; i < fileData.length; i++) fileData[i] = rng();
writeFixture('file', fileData, {
  isText: false,
  compress: true,
  filename: 'secret-report.bin',
  mimeType: 'application/octet-stream',
  renderFrames: 3,
});

// 3. Large payload spanning multiple RaptorQ source blocks (tests RFC 6330
//    block geometry: 12 MB at 197 B/symbol ≈ 63.8k symbols > 56,403).
const bigData = new Uint8Array(12 * 1024 * 1024);
for (let i = 0; i < bigData.length; i += 4096) bigData[i] = (i >> 12) & 0xff;
writeFixture('multiblock', bigData, {
  isText: false,
  compress: false,
  filename: 'big.bin',
  mimeType: 'application/octet-stream',
});

console.log('fixtures written to', FIXTURES);

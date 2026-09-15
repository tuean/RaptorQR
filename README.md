[![npm](https://img.shields.io/npm/v/@raptorqr/core?label=npm)](https://www.npmjs.com/package/@raptorqr/core)
[![license](https://img.shields.io/npm/l/@raptorqr/core)](LICENSE)
[![Live Demo](https://img.shields.io/badge/Live-Demo-2ea44f)](https://qr.linkto.host/)
# RaptorQR

**The world's fastest** file and text transfer between devices, using animated high-throughput QR codes and a camera.

Everything runs locally: no upload server, no Bluetooth, no cable.

RaptorQR grew out of an earlier QR-streaming prototype and is now a full rewrite of the transfer pipeline — FEC, QR rendering, worker scheduling, scanning, sender/receiver UI, CLI packaging and repo layout.

<img width="221" height="480" alt="raptorQR" src="https://github.com/user-attachments/assets/e4a5f6f5-5fe8-4953-931a-a86a509b52e5" />

Live demo: https://qr.linkto.host/

[![Deploy with Vercel](https://vercel.com/button)](https://vercel.com/new/clone?repository-url=https%3A%2F%2Fgithub.com%2Finfrost%2Fraptorqr)

## Table of Contents

* [Performance](#performance)
* [Packages](#packages)
* [Features](#features)
* [FAQ](#faq)
* [Download](#download)
* [Development](#development)
* [CLI](#cli)
* [WASM Artifacts](#wasm-artifacts)
* [Implementation Notes](#implementation-notes)
* [Links & Acknowledgements](#links--acknowledgements)

## Performance

RaptorQR uses the Rust [`cberner/raptorq`](https://github.com/cberner/raptorq) implementation of RaptorQ (RFC 6330), compiled to WASM, as its primary fountain-code codec, [`erwanvivien/fast_qr`](https://github.com/erwanvivien/fast_qr) compiled to WASM for QR rendering, and ZXing WASM for scanning.

That is at least **50x+ higher throughput** than the original JavaScript-only path.

| Scenario                                  |                         Result |
| ----------------------------------------- | -----------------------------: |
| V20 QR, 4-code parallel playback, 30 FPS  | up to 300 decoded QR symbols/s |
| V30 QR, 4-code parallel playback, 30 FPS  |      100+ decoded QR symbols/s |
| 95.2 KB file transfer (V30-L x 4QR@30fps) |       375 ms, about 254.0 KB/s |
| 6.5 MB file transfer (V30-L x 4QR@30fps)  |         36 s, about 183.6 KB/s |

<img width="295" height="203" alt="clipboard_2026-07-08_17-21" src="https://github.com/user-attachments/assets/a5d5fced-8042-447a-ba58-00c42ee107f6" />

File tests were measured with **iPhone 16 / Safari as scanner**; real speed depends on camera, browser, lighting, QR size/version, playback rate and scan settings.

## Packages

| Package | Version | Install | Usage |
| --- | --- | --- | --- |
| `@raptorqr/core` | [![npm](https://img.shields.io/npm/v/@raptorqr/core)](https://www.npmjs.com/package/@raptorqr/core) | `pnpm add @raptorqr/core` | [Packetize, schedule, render, and decode](packages/raptorqr-core/README.md#send-with-raptorq) |
| `@raptorqr/cli` | [![npm](https://img.shields.io/npm/v/@raptorqr/cli)](https://www.npmjs.com/package/@raptorqr/cli) | `pnpm add --global @raptorqr/cli` | [Terminal sender and local web server](packages/raptorqr-cli/README.md#usage) |
| `@raptorqr/fast-qr-wasm` | [![npm](https://img.shields.io/npm/v/@raptorqr/fast-qr-wasm)](https://www.npmjs.com/package/@raptorqr/fast-qr-wasm) | `pnpm add @raptorqr/fast-qr-wasm` | [Render QR codes as RGBA or matrices](packages/raptorqr-fast-qr-wasm/README.md#render-rgba) |
| `@raptorqr/raptorq-wasm` | [![npm](https://img.shields.io/npm/v/@raptorqr/raptorq-wasm)](https://www.npmjs.com/package/@raptorqr/raptorq-wasm) | `pnpm add @raptorqr/raptorq-wasm` | [Low-level RaptorQ encode and decode](packages/raptorqr-raptorq-wasm/README.md#encode-and-decode) |

Most applications only need `@raptorqr/core`, which already uses both WASM packages. The unpublished Preact/Vite web app lives in `apps/web`.

## Features

* Browser sender/receiver for text and files
* Terminal sender via the `raptorqr` CLI
* RaptorQ WASM fountain codec (default); JS RLNC codec kept for compatibility, deprecated
* fast_qr WASM rendering (optional ZXing WASM writer) and ZXing WASM scanning
* Parallel QR playback, live canvas rendering, optional GIF export
* Adjustable QR version, ECC level, playback FPS, scan FPS and repair overhead

## FAQ

### Can I use RaptorQR offline?

Yes. The web app ships `sw.js` and is PWA-ready: after the first load you can reopen the same link without internet access.

### Does RaptorQR upload my files anywhere?

No. Files and text are encoded into QR codes on the sender and decoded from the camera feed on the receiver — everything stays local.

## Download

Prebuilt artifacts are attached to the [latest release](https://github.com/tuean/RaptorQR/releases/latest), or reproducible locally with `pnpm release` (→ `release/`):

| File | For | Notes |
| --- | --- | --- |
| `RaptorQR-Sender.html` | the sender side (VM / other machine) | ~5 MB self-contained page, works offline from `file://` |
| `RaptorQR-Receiver-macos.zip` / `RaptorQR Receiver.app` | the receiver side (host Mac) | universal binary (Apple Silicon + Intel), macOS 13+; unsigned, so first launch needs right-click → Open |
| `使用说明.txt` | both | quick start + troubleshooting (Chinese) |

### Screen-capture receiver (Rust + gpui)

`host/` contains a desktop receiver that replaces the camera with **screen capture**:
it reads the animated QR stream off a region of your screen. This is for the
"isolated VM ↔ host on one desktop" case, where there is no network and no shared clipboard.

* Guest (in the VM): build the sender as one self-contained HTML file and open it.

  ```bash
  pnpm guest:build      # → apps/web/dist-single/index.html
  ```

  Pick a file, press **Start Live QR**, and the stream plays on screen.

* Host: run the receiver. All displays are scanned automatically; use **◀ Display ▶** to preview another screen and drag a rectangle to narrow the scan.

  ```bash
  pnpm host:run         # cargo run --manifest-path host/Cargo.toml
  ```

  Files land in `~/Downloads`; macOS asks for **Screen Recording** permission on first launch.

See [host/README.md](host/README.md) for the protocol mapping, tests (including a headless-browser end-to-end test) and known limitations.

## Development

```bash
pnpm install
pnpm dev:web      # then open the printed Vite URL
pnpm build        # build all packages and the web app
pnpm test
```

Camera access from another device on the LAN requires serving over HTTPS or an allowed dev host, per browser camera policy.

Deployment: the root `vercel.json` lets Vercel build the web app from the monorepo (`pnpm --filter @raptorqr/web build` → `apps/web/dist`) without changing the project root.

Run the CLI from source:

```bash
pnpm --filter @raptorqr/cli cli
node packages/raptorqr-cli/dist/raptorqr.js --help   # smoke-test the bundle
```

## CLI

```bash
raptorqr document.pdf
echo "Hello, world!" | raptorqr
raptorqr --serve --port 8080
```

`raptorqr document.pdf` reads the local file, keeps the filename and MIME metadata, and displays a looping RaptorQ QR stream in the terminal — nothing is uploaded, no URL is created, no output file is written. Scan it with the RaptorQR receiver to reconstruct the file.

`echo "Hello, world!" | raptorqr` does the same for text from stdin.

`raptorqr --serve --port 8080` serves the built web app (run `pnpm build` first, then open `http://localhost:8080`).

Press `q` or `Ctrl-C` to stop the terminal sender.

The bundle is built at `packages/raptorqr-cli/dist/raptorqr.js`, with its WASM sidecars copied alongside.

## WASM Artifacts

Generated artifacts live under:

```text
packages/raptorqr-fast-qr-wasm/src/wasm
packages/raptorqr-raptorq-wasm/src/wasm
```

Build scripts (paste into a Colab notebook; they download and compile the upstream Rust crates to WASM):

```text
packages/raptorqr-fast-qr-wasm/src/build_fast_qr_wasm_colab.py
packages/raptorqr-raptorq-wasm/src/build_raptorq_wasm_colab.py
```

## Implementation Notes

The protocol keeps the existing fixed 8-byte transport header: RaptorQ packets use the reserved symbol-index sentinel, JS RLNC packets the legacy symbol-index range.

`wasm-raptorq` is the default FEC codec; `js-rlnc` is still exported and test-covered but deprecated, and never used as an automatic fallback.

Deeper protocol and package overview: [ARCHITECTURE.md](ARCHITECTURE.md).

## Links & Acknowledgements

### Open-source projects used

* [hermitm0nk/qr-stream](https://github.com/hermitm0nk/qr-stream) — the original inspiration
* [cberner/raptorq](https://github.com/cberner/raptorq) — RaptorQ / RFC 6330 fountain-code implementation behind the WASM FEC codec
* [erwanvivien/fast_qr](https://github.com/erwanvivien/fast_qr) — high-speed QR rendering library compiled to WASM
* [Sec-ant/zxing-wasm](https://github.com/Sec-ant/zxing-wasm) — ZXing-C++ WebAssembly build used for scanning
* [ZXing-C++](https://github.com/zxing-cpp/zxing-cpp) / [ZXing](https://github.com/zxing/zxing) — the underlying barcode processing libraries
* [Preact](https://preactjs.com/), [Vite](https://vite.dev/), [pnpm](https://pnpm.io/) — UI framework, build tool and package manager

### Community

* [LINUX DO](https://linux.do/t/topic/2549646)
* [Appinn](https://meta.appinn.net/t/topic/87996/6)

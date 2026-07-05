# PD-EZ

![status](https://img.shields.io/badge/status-V1_IN_PROGRESS-blue)
**Brand accent:** fountain-pen blue `#2b4acb` / `#7d95f2`

Offline desktop PDF editor. **$4.99 one-time — pay once, own it forever.**
No subscription, no account, no cloud: documents never leave the machine, and
the license is verified by Ed25519 signature locally at startup, with zero
network calls after the one-time activation.

## Architecture

```
src/                Vue 3 + TypeScript + Vite frontend (viewer UI, license gate)
src-tauri/          Tauri v2 shell, pure Rust (no sidecar runtime)
  src/pdf_worker.rs  Dedicated engine thread owning PDFium (not thread-safe) +
                     all open documents; commands talk to it over mpsc
  src/license.rs     Ed25519 lease verification (offline, pubkey baked in)
  src/bin/mint_lease.rs  Dev tool: mint signed leases (feature: dev-tools)
  pdfium/            pdfium.dll (fetched, not committed — BSD-3, non-V8 build)
  keys/license.pub   Embedded verify key (license.key = dev signer, gitignored)
```

Engine: **PDFium** via `pdfium-render`. Write layer (planned per capability
audit): `lopdf`, `qpdf`, `krilla`, Tesseract sidecar for OCR, CMS crates +
Windows CNG for PAdES digital signatures.

## Dev setup

```powershell
pwsh scripts/fetch-pdfium.ps1   # once: fetch pdfium.dll
pnpm install
pnpm tauri dev                  # vite on port 3011 (see E:\Programming\PORT_REGISTRY.md)
```

Debug builds run unlicensed (DEV badge). To test the license path:

```powershell
cargo run --features dev-tools --bin mint_lease -- you@example.com order-42
# then import the .lease via the app's activation screen
```

Tests: `cargo test` (license signature round-trip lives in `license.rs`).

## Release model

Polar.sh (merchant of record) checkout → purchase webhook mints an
Ed25519-signed lease → app caches it → every startup verifies the signature
locally. Import-from-file stays as the airgapped activation route. The
production signing key never ships; only `keys/license.pub` is embedded.

## Related

- Capability audit + tier plan: PD-EZ scope-lock artifact (2026-07-05).
- Monorepo host: `E:\Programming` (this is an embedded studio repo).

## License

Proprietary — © Zuga Technologies. All rights reserved.

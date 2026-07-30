# PD-EZ

![status](https://img.shields.io/badge/status-V1_IN_PROGRESS-blue)
**Brand accent:** fountain-pen blue `#2b4acb` / `#7d95f2`

Offline desktop PDF editor **and Typora-style Markdown editor**.
**Free and open source (Apache-2.0).**
No subscription, no account, no cloud, no telemetry: documents never leave the
machine, and the app makes zero network calls in normal use.

## Architecture

```
src/                Vue 3 + TypeScript + Vite frontend (PDF viewer, Markdown
                     editor, license gate); App.vue routes between the two by
                     the opened file's extension
  components/PdfViewer.vue       PDF viewer/editor UI
  components/MarkdownEditor.vue  Tiptap-based WYSIWYG Markdown editor — no
                                  visible `#`/`**` syntax while typing, reads
                                  and writes plain .md text via tiptap-markdown
src-tauri/          Tauri v2 shell, pure Rust (no sidecar runtime)
  src/pdf_worker.rs  Dedicated engine thread owning PDFium (not thread-safe) +
                     all open documents; commands talk to it over mpsc
  src/license.rs     Ed25519 lease verification (offline, pubkey baked in)
  src/bin/mint_lease.rs  Dev tool: mint signed leases (feature: dev-tools)
  pdfium/            pdfium.dll (fetched, not committed — BSD-3, non-V8 build)
  keys/license.pub   Embedded verify key (license.key = dev signer, gitignored)
shellext/            Windows Explorer integration (pdez_shell.dll): thumbnails
                     + preview pane for both .pdf and .md/.markdown. Both
                     formats share the same two COM classes — Initialize()
                     content-sniffs the stream (`%PDF-` header) and dispatches
                     to the PDFium engine or to src/md_engine.rs, a lightweight
                     GDI-based Markdown text rasterizer (pulldown-cmark parse +
                     direct-to-DIB drawing, no browser engine involved).
```

Engine: **PDFium** via `pdfium-render` for PDF. Write layer (planned per
capability audit): `lopdf`, `qpdf`, `krilla`, Tesseract sidecar for OCR, CMS
crates + Windows CNG for PAdES digital signatures. Markdown editing is
**Tiptap** (ProseMirror) + `tiptap-markdown` in the frontend — the on-disk
format is always plain Markdown text, Tiptap only owns the live editing view.

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

## Licensing code (retained, not enforced)

`src-tauri/src/license.rs` implements offline Ed25519 lease verification, and
it stays in the tree as a working reference for airgapped license checking.
**It does not gate the application.** PD-EZ was briefly planned as a $4.99
one-time product; that was dropped in favor of Apache-2.0. Nothing is locked,
nothing needs activating, and no key is required to build or run.

## Related

- Capability audit + tier plan: PD-EZ scope-lock artifact (2026-07-05).
- Monorepo host: `E:\Programming` (this is an embedded studio repo).

## Redaction: read this before you rely on it

PD-EZ redaction removes the underlying page objects rather than drawing a black
box over them, which is the correct approach and the one most cheap tools get
wrong. It is still **not certified for legal, medical, or classified material.**
No independent audit has been performed. If a failed redaction would cause real
harm in your situation, verify the output yourself before releasing it.

## License

Apache-2.0. See [LICENSE](LICENSE).

PDFium is BSD-3-Clause and is fetched at build time, not vendored here.

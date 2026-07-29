//! Lightweight Markdown rasterizer for Explorer thumbnails/preview. This is a
//! styled *text block* renderer (headings/lists/code/quotes/rules) — not a
//! full CSS layout engine, which would be out of scope for a COM thumbnail
//! provider that has to respond fast. Rich, faithful rendering happens in-app
//! (Tiptap); this just needs to be a glanceable, on-brand peek.
//!
//! Draws straight onto a `CreateDIBSection`-backed memory DC with plain GDI
//! text APIs — no rasterization crate needed beyond `Win32_Graphics_Gdi`,
//! which `engine.rs` already pulls in for the PDFium bitmap path.

use std::ffi::c_void;

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
    SelectObject, SetBkMode, SetTextColor, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CALCRECT,
    DT_NOPREFIX, DT_WORDBREAK, FF_DONTCARE, FW_BOLD, FW_NORMAL, HDC, OUT_DEFAULT_PRECIS,
    TRANSPARENT,
};

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((b as u32) << 16 | (g as u32) << 8 | r as u32)
}

const INK: (u8, u8, u8) = (42, 42, 40); // body text
const ACCENT: (u8, u8, u8) = (0x2b, 0x4a, 0xcb); // headings — PD-EZ accent-ink
const DIM: (u8, u8, u8) = (90, 87, 76); // quotes
const PAPER: (u8, u8, u8) = (0xfb, 0xfa, 0xf7); // background, matches the in-app canvas
const CODE_BG: (u8, u8, u8) = (0xef, 0xec, 0xe2);

#[derive(Clone, Copy)]
enum Kind {
    H1,
    H2,
    H3,
    Paragraph,
    ListItem,
    Quote,
    Code,
    Rule,
}

struct Block {
    kind: Kind,
    text: String,
}

enum Frame {
    Heading(HeadingLevel),
    Paragraph,
    Item,
    Quote,
    Code,
}

struct OpenFrame {
    frame: Frame,
    text: String,
}

fn parse_blocks(markdown: &str) -> Vec<Block> {
    let mut stack: Vec<OpenFrame> = Vec::new();
    let mut blocks: Vec<Block> = Vec::new();

    let push_text = |stack: &mut Vec<OpenFrame>, s: &str| {
        if let Some(top) = stack.last_mut() {
            top.text.push_str(s);
        }
    };

    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                stack.push(OpenFrame { frame: Frame::Heading(level), text: String::new() });
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(f) = stack.pop() {
                    let kind = match f.frame {
                        Frame::Heading(HeadingLevel::H1) => Kind::H1,
                        Frame::Heading(HeadingLevel::H2) => Kind::H2,
                        _ => Kind::H3,
                    };
                    blocks.push(Block { kind, text: f.text.trim().to_string() });
                }
            }
            Event::Start(Tag::Paragraph) => {
                if stack.is_empty() {
                    stack.push(OpenFrame { frame: Frame::Paragraph, text: String::new() });
                }
            }
            Event::End(TagEnd::Paragraph) => {
                if matches!(stack.last(), Some(f) if matches!(f.frame, Frame::Paragraph)) {
                    let f = stack.pop().unwrap();
                    blocks.push(Block { kind: Kind::Paragraph, text: f.text.trim().to_string() });
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                stack.push(OpenFrame { frame: Frame::Quote, text: String::new() });
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                if let Some(f) = stack.pop() {
                    blocks.push(Block { kind: Kind::Quote, text: f.text.trim().to_string() });
                }
            }
            Event::Start(Tag::Item) => {
                stack.push(OpenFrame { frame: Frame::Item, text: String::new() });
            }
            Event::End(TagEnd::Item) => {
                if let Some(f) = stack.pop() {
                    blocks.push(Block { kind: Kind::ListItem, text: f.text.trim().to_string() });
                }
            }
            Event::Start(Tag::CodeBlock(_)) => {
                stack.push(OpenFrame { frame: Frame::Code, text: String::new() });
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(f) = stack.pop() {
                    // Trailing newline from the fence reads as a blank last line.
                    blocks.push(Block { kind: Kind::Code, text: f.text.trim_end().to_string() });
                }
            }
            Event::Rule => blocks.push(Block { kind: Kind::Rule, text: String::new() }),
            Event::Text(s) | Event::Code(s) => push_text(&mut stack, &s),
            Event::SoftBreak => push_text(&mut stack, " "),
            Event::HardBreak => push_text(&mut stack, "\n"),
            _ => {}
        }
    }
    blocks
}

struct Style {
    face: windows::core::PCWSTR,
    px: i32,
    bold: bool,
    color: (u8, u8, u8),
    indent: i32,
    boxed: bool,
}

fn style_for(kind: Kind) -> Style {
    match kind {
        Kind::H1 => Style { face: w!("Segoe UI"), px: 30, bold: true, color: ACCENT, indent: 0, boxed: false },
        Kind::H2 => Style { face: w!("Segoe UI"), px: 23, bold: true, color: ACCENT, indent: 0, boxed: false },
        Kind::H3 => Style { face: w!("Segoe UI"), px: 19, bold: true, color: ACCENT, indent: 0, boxed: false },
        Kind::Paragraph => Style { face: w!("Segoe UI"), px: 15, bold: false, color: INK, indent: 0, boxed: false },
        Kind::ListItem => Style { face: w!("Segoe UI"), px: 15, bold: false, color: INK, indent: 26, boxed: false },
        Kind::Quote => Style { face: w!("Segoe UI"), px: 15, bold: false, color: DIM, indent: 20, boxed: false },
        Kind::Code => Style { face: w!("Consolas"), px: 13, bold: false, color: INK, indent: 0, boxed: true },
        Kind::Rule => Style { face: w!("Segoe UI"), px: 4, bold: false, color: DIM, indent: 0, boxed: false },
    }
}

/// Render `markdown` scaled to exactly `max_w` x `max_h` (a text pane has no
/// intrinsic aspect ratio, so — unlike PDF pages — we fill the whole target
/// rather than fitting inside it). Returns (width, height, BGRA bytes),
/// matching `engine::render_page_fit`'s contract so callers can share
/// `bgra_to_hbitmap`.
pub fn render_markdown_fit(bytes: &[u8], max_w: u32, max_h: u32) -> Result<(u32, u32, Vec<u8>), String> {
    let markdown = String::from_utf8_lossy(bytes);
    let blocks = parse_blocks(&markdown);

    let (w, h) = (max_w.max(32), max_h.max(32));
    let mut header = BITMAPINFOHEADER::default();
    header.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    header.biWidth = w as i32;
    header.biHeight = -(h as i32); // top-down
    header.biPlanes = 1;
    header.biBitCount = 32;
    header.biCompression = BI_RGB.0;
    let bmi = BITMAPINFO { bmiHeader: header, ..Default::default() };

    unsafe {
        let mem_dc = CreateCompatibleDC(HDC::default());
        if mem_dc.is_invalid() {
            return Err("CreateCompatibleDC failed".to_string());
        }
        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|e| format!("CreateDIBSection failed: {e}"))?;
        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            return Err("CreateDIBSection returned no bits".to_string());
        }
        let old_bmp = SelectObject(mem_dc, dib);

        // Paper background.
        let px = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
        for chunk in px.chunks_exact_mut(4) {
            chunk[0] = PAPER.2;
            chunk[1] = PAPER.1;
            chunk[2] = PAPER.0;
            chunk[3] = 255;
        }

        SetBkMode(mem_dc, TRANSPARENT);

        const MARGIN: i32 = 20;
        const GAP: i32 = 10;
        let mut y: i32 = MARGIN;
        let content_w = (w as i32 - MARGIN * 2).max(40);

        for block in &blocks {
            if y >= h as i32 - MARGIN {
                break;
            }
            if matches!(block.kind, Kind::Rule) {
                y += 14;
                continue;
            }
            let style = style_for(block.kind);
            let font = CreateFontW(
                -style.px,
                0,
                0,
                0,
                if style.bold { FW_BOLD.0 as i32 } else { FW_NORMAL.0 as i32 },
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                ANTIALIASED_QUALITY.0 as u32,
                (FF_DONTCARE.0 as u32) | DEFAULT_PITCH.0 as u32,
                style.face,
            );
            let old_font = SelectObject(mem_dc, font);
            SetTextColor(mem_dc, rgb(style.color.0, style.color.1, style.color.2));

            let text = if block.text.is_empty() { " ".to_string() } else { block.text.clone() };
            let mut wtext: Vec<u16> = text.encode_utf16().collect();
            let rect_w = content_w - style.indent;

            // Measure, then draw at the same rect (word-wrapped).
            let mut measure = RECT { left: 0, top: 0, right: rect_w, bottom: 0 };
            DrawTextW(mem_dc, &mut wtext, &mut measure, DT_WORDBREAK | DT_CALCRECT | DT_NOPREFIX);
            let block_h = (measure.bottom - measure.top).max(style.px + 4);

            if style.boxed {
                for row in y.max(0)..(y + block_h + 12).min(h as i32) {
                    for col in MARGIN.max(0)..(MARGIN + content_w).min(w as i32) {
                        let idx = ((row * w as i32 + col) * 4) as usize;
                        if idx + 3 < px.len() {
                            px[idx] = CODE_BG.2;
                            px[idx + 1] = CODE_BG.1;
                            px[idx + 2] = CODE_BG.0;
                        }
                    }
                }
            }

            let pad_top = if style.boxed { 6 } else { 0 };
            let mut draw_rect = RECT {
                left: MARGIN + style.indent,
                top: y + pad_top,
                right: MARGIN + style.indent + rect_w,
                bottom: y + pad_top + block_h,
            };
            DrawTextW(mem_dc, &mut wtext, &mut draw_rect, DT_WORDBREAK | DT_NOPREFIX);

            y += block_h + if style.boxed { pad_top + 10 } else { GAP };

            SelectObject(mem_dc, old_font);
            let _ = DeleteObject(font);
        }

        SelectObject(mem_dc, old_bmp);
        let out = px.to_vec();
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        Ok((w, h, out))
    }
}

/// Best-effort content sniff: PDFs start with `%PDF-`; anything else opened
/// through this DLL is treated as Markdown/plain text.
pub fn looks_like_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF-")
}

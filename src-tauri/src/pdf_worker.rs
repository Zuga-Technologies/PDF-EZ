//! Dedicated PDF engine thread. PDFium is not thread-safe, so a single worker
//! owns the `Pdfium` instance and every open document; commands talk to it
//! over an mpsc channel and block on a per-request reply channel.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};

use base64::Engine as _;
use pdfium_render::prelude::*;
use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct PageDims {
    /// PDF points (1/72 in)
    pub width: f32,
    pub height: f32,
}

#[derive(Serialize, Clone)]
pub struct DocInfo {
    pub id: u32,
    pub page_count: u16,
    pub pages: Vec<PageDims>,
    pub title: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct RenderedPage {
    pub index: u16,
    pub width_px: u32,
    pub height_px: u32,
    /// data:image/png;base64,...
    pub data_url: String,
}

pub enum PdfRequest {
    Open {
        path: PathBuf,
        password: Option<String>,
        reply: Sender<Result<DocInfo, String>>,
    },
    Close {
        id: u32,
        reply: Sender<Result<(), String>>,
    },
    Render {
        id: u32,
        index: u16,
        target_width: u32,
        reply: Sender<Result<RenderedPage, String>>,
    },
    PageText {
        id: u32,
        index: u16,
        reply: Sender<Result<String, String>>,
    },
}

pub struct PdfWorkerHandle {
    pub tx: std::sync::Mutex<Sender<PdfRequest>>,
}

impl PdfWorkerHandle {
    pub fn send(&self, req: PdfRequest) -> Result<(), String> {
        self.tx
            .lock()
            .map_err(|e| e.to_string())?
            .send(req)
            .map_err(|_| "PDF engine thread is not running".to_string())
    }
}

/// Locate pdfium.dll: bundled resource dir in production, the repo copy in dev.
fn pdfium_library_dirs(resource_dir: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(res) = resource_dir {
        dirs.push(res.join("pdfium").join("bin"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    // Dev: src-tauri/pdfium/bin relative to the crate manifest.
    dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("pdfium").join("bin"));
    dirs
}

fn bind_pdfium(resource_dir: Option<PathBuf>) -> Result<Pdfium, String> {
    for dir in pdfium_library_dirs(resource_dir) {
        let lib = Pdfium::pdfium_platform_library_name_at_path(&dir);
        if let Ok(bindings) = Pdfium::bind_to_library(&lib) {
            return Ok(Pdfium::new(bindings));
        }
    }
    Pdfium::bind_to_system_library()
        .map(Pdfium::new)
        .map_err(|e| format!("Could not load pdfium library: {e}"))
}

pub fn spawn(resource_dir: Option<PathBuf>) -> PdfWorkerHandle {
    let (tx, rx) = channel::<PdfRequest>();

    std::thread::Builder::new()
        .name("pdf-engine".into())
        .spawn(move || {
            let pdfium: &'static Pdfium = match bind_pdfium(resource_dir) {
                // Leak once: the engine lives for the whole app lifetime, and a
                // 'static borrow lets us keep PdfDocument handles in the map.
                Ok(p) => Box::leak(Box::new(p)),
                Err(e) => {
                    // Answer every request with the load error instead of hanging callers.
                    while let Ok(req) = rx.recv() {
                        fail(req, &e);
                    }
                    return;
                }
            };

            let mut docs: HashMap<u32, PdfDocument<'static>> = HashMap::new();
            let mut next_id: u32 = 1;

            while let Ok(req) = rx.recv() {
                match req {
                    PdfRequest::Open { path, password, reply } => {
                        let result = open_doc(pdfium, &mut docs, &mut next_id, path, password);
                        let _ = reply.send(result);
                    }
                    PdfRequest::Close { id, reply } => {
                        docs.remove(&id);
                        let _ = reply.send(Ok(()));
                    }
                    PdfRequest::Render { id, index, target_width, reply } => {
                        let result = render_page(&docs, id, index, target_width);
                        let _ = reply.send(result);
                    }
                    PdfRequest::PageText { id, index, reply } => {
                        let result = page_text(&docs, id, index);
                        let _ = reply.send(result);
                    }
                }
            }
        })
        .expect("failed to spawn pdf-engine thread");

    PdfWorkerHandle { tx: std::sync::Mutex::new(tx) }
}

fn fail(req: PdfRequest, err: &str) {
    match req {
        PdfRequest::Open { reply, .. } => {
            let _ = reply.send(Err(err.to_string()));
        }
        PdfRequest::Close { reply, .. } => {
            let _ = reply.send(Err(err.to_string()));
        }
        PdfRequest::Render { reply, .. } => {
            let _ = reply.send(Err(err.to_string()));
        }
        PdfRequest::PageText { reply, .. } => {
            let _ = reply.send(Err(err.to_string()));
        }
    }
}

fn open_doc(
    pdfium: &'static Pdfium,
    docs: &mut HashMap<u32, PdfDocument<'static>>,
    next_id: &mut u32,
    path: PathBuf,
    password: Option<String>,
) -> Result<DocInfo, String> {
    let doc = pdfium
        .load_pdf_from_file(&path, password.as_deref())
        .map_err(|e| friendly_open_error(e))?;

    let pages: Vec<PageDims> = doc
        .pages()
        .iter()
        .map(|p| PageDims { width: p.width().value, height: p.height().value })
        .collect();

    let title = doc
        .metadata()
        .get(PdfDocumentMetadataTagType::Title)
        .map(|t| t.value().to_string())
        .filter(|t| !t.is_empty());

    let id = *next_id;
    *next_id += 1;
    let info = DocInfo { id, page_count: pages.len() as u16, pages, title };
    docs.insert(id, doc);
    Ok(info)
}

fn friendly_open_error(e: PdfiumError) -> String {
    match e {
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
            "PASSWORD_REQUIRED".to_string()
        }
        other => format!("Could not open PDF: {other}"),
    }
}

fn render_page(
    docs: &HashMap<u32, PdfDocument<'static>>,
    id: u32,
    index: u16,
    target_width: u32,
) -> Result<RenderedPage, String> {
    let doc = docs.get(&id).ok_or("Document is not open")?;
    let page = doc
        .pages()
        .get(index.into())
        .map_err(|e| format!("Page {index} not found: {e}"))?;

    let width = target_width.clamp(64, 8192) as i32;
    let config = PdfRenderConfig::new()
        .set_target_width(width)
        .render_form_data(true);

    let bitmap = page
        .render_with_config(&config)
        .map_err(|e| format!("Render failed: {e}"))?;
    let image = bitmap
        .as_image()
        .map_err(|e| format!("Bitmap conversion failed: {e}"))?;

    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| format!("PNG encode failed: {e}"))?;

    let b64 = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
    Ok(RenderedPage {
        index,
        width_px: image.width(),
        height_px: image.height(),
        data_url: format!("data:image/png;base64,{b64}"),
    })
}

fn page_text(
    docs: &HashMap<u32, PdfDocument<'static>>,
    id: u32,
    index: u16,
) -> Result<String, String> {
    let doc = docs.get(&id).ok_or("Document is not open")?;
    let page = doc
        .pages()
        .get(index.into())
        .map_err(|e| format!("Page {index} not found: {e}"))?;
    let text = page.text().map_err(|e| format!("Text extraction failed: {e}"))?;
    Ok(text.all())
}

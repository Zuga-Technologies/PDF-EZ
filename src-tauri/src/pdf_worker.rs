//! Dedicated PDF engine thread. PDFium is not thread-safe, so a single worker
//! owns the `Pdfium` instance and every open document; commands talk to it
//! over an mpsc channel and block on a per-request reply channel.
//!
//! Documents are loaded from an in-memory byte buffer, never a held file
//! handle — the user's file is never locked, and save-in-place is safe.

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
    pub path: String,
}

#[derive(Serialize, Clone)]
pub struct AnnotInfo {
    pub page: u16,
    pub index: u16,
    pub kind: String,
    /// Bounds in PDF points, PDF coordinate space (origin bottom-left).
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
    pub contents: Option<String>,
}

#[derive(serde::Deserialize, Clone, Copy)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
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
        /// JPEG (fast, small — main pages) vs PNG (thumbnails).
        jpeg: bool,
        reply: Sender<Result<RenderedPage, String>>,
    },
    PageText {
        id: u32,
        index: u16,
        reply: Sender<Result<String, String>>,
    },
    /// Rotate by quarter turns: +1 = 90° clockwise, -1 = 90° counter-clockwise.
    RotatePages {
        id: u32,
        indices: Vec<u16>,
        quarter_turns: i8,
        reply: Sender<Result<DocInfo, String>>,
    },
    DeletePages {
        id: u32,
        indices: Vec<u16>,
        reply: Sender<Result<DocInfo, String>>,
    },
    /// `order[dest] = source index` — a full permutation of current pages.
    ReorderPages {
        id: u32,
        order: Vec<u16>,
        reply: Sender<Result<DocInfo, String>>,
    },
    /// `path: None` saves in place over the document's own path.
    Save {
        id: u32,
        path: Option<PathBuf>,
        reply: Sender<Result<DocInfo, String>>,
    },
    ListAnnotations {
        id: u32,
        page: u16,
        reply: Sender<Result<Vec<AnnotInfo>, String>>,
    },
    /// Rect in PDF points, PDF coordinate space.
    AddHighlight {
        id: u32,
        page: u16,
        left: f32,
        bottom: f32,
        right: f32,
        top: f32,
        color: Rgba,
        reply: Sender<Result<Vec<AnnotInfo>, String>>,
    },
    AddNote {
        id: u32,
        page: u16,
        x: f32,
        y: f32,
        contents: String,
        reply: Sender<Result<Vec<AnnotInfo>, String>>,
    },
    AddInk {
        id: u32,
        page: u16,
        /// Strokes of (x, y) PDF-point coordinates.
        strokes: Vec<Vec<(f32, f32)>>,
        color: Rgba,
        width: f32,
        reply: Sender<Result<Vec<AnnotInfo>, String>>,
    },
    DeleteAnnotation {
        id: u32,
        page: u16,
        index: u16,
        reply: Sender<Result<Vec<AnnotInfo>, String>>,
    },
}

struct OpenDoc {
    doc: PdfDocument<'static>,
    path: PathBuf,
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

            let mut docs: HashMap<u32, OpenDoc> = HashMap::new();
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
                    PdfRequest::Render { id, index, target_width, jpeg, reply } => {
                        // Skip stale renders whose caller already gave up —
                        // keeps a queue pileup from freezing the engine thread.
                        if reply.send(render_page(&docs, id, index, target_width, jpeg)).is_err() {
                            continue;
                        }
                    }
                    PdfRequest::PageText { id, index, reply } => {
                        let result = page_text(&docs, id, index);
                        let _ = reply.send(result);
                    }
                    PdfRequest::RotatePages { id, indices, quarter_turns, reply } => {
                        let result = rotate_pages(&mut docs, id, &indices, quarter_turns);
                        let _ = reply.send(result);
                    }
                    PdfRequest::DeletePages { id, indices, reply } => {
                        let result = delete_pages(&mut docs, id, indices);
                        let _ = reply.send(result);
                    }
                    PdfRequest::ReorderPages { id, order, reply } => {
                        let result = reorder_pages(pdfium, &mut docs, id, &order);
                        let _ = reply.send(result);
                    }
                    PdfRequest::Save { id, path, reply } => {
                        let result = save_doc(&mut docs, id, path);
                        let _ = reply.send(result);
                    }
                    PdfRequest::ListAnnotations { id, page, reply } => {
                        let _ = reply.send(list_annotations(&docs, id, page));
                    }
                    PdfRequest::AddHighlight { id, page, left, bottom, right, top, color, reply } => {
                        let _ = reply.send(add_highlight(&mut docs, id, page, left, bottom, right, top, color));
                    }
                    PdfRequest::AddNote { id, page, x, y, contents, reply } => {
                        let _ = reply.send(add_note(&mut docs, id, page, x, y, &contents));
                    }
                    PdfRequest::AddInk { id, page, strokes, color, width, reply } => {
                        let _ = reply.send(add_ink(&mut docs, id, page, &strokes, color, width));
                    }
                    PdfRequest::DeleteAnnotation { id, page, index, reply } => {
                        let _ = reply.send(delete_annotation(&mut docs, id, page, index));
                    }
                }
            }
        })
        .expect("failed to spawn pdf-engine thread");

    PdfWorkerHandle { tx: std::sync::Mutex::new(tx) }
}

fn fail(req: PdfRequest, err: &str) {
    let e = err.to_string();
    match req {
        PdfRequest::Open { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::Close { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::Render { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::PageText { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::RotatePages { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::DeletePages { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::ReorderPages { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::Save { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::ListAnnotations { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::AddHighlight { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::AddNote { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::AddInk { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::DeleteAnnotation { reply, .. } => drop(reply.send(Err(e))),
    }
}

fn make_info(id: u32, entry: &OpenDoc) -> DocInfo {
    // page_sizes() reads dimensions without loading page objects — iterating
    // pages here froze the app on large documents.
    let pages: Vec<PageDims> = entry
        .doc
        .pages()
        .page_sizes()
        .map(|sizes| {
            sizes
                .iter()
                .map(|r| PageDims { width: r.width().value, height: r.height().value })
                .collect()
        })
        .unwrap_or_default();

    let title = entry
        .doc
        .metadata()
        .get(PdfDocumentMetadataTagType::Title)
        .map(|t| t.value().to_string())
        .filter(|t| !t.is_empty());

    DocInfo {
        id,
        page_count: pages.len() as u16,
        pages,
        title,
        path: entry.path.to_string_lossy().into_owned(),
    }
}

fn open_doc(
    pdfium: &'static Pdfium,
    docs: &mut HashMap<u32, OpenDoc>,
    next_id: &mut u32,
    path: PathBuf,
    password: Option<String>,
) -> Result<DocInfo, String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("Could not read file: {e}"))?;
    let doc = pdfium
        .load_pdf_from_byte_vec(bytes, password.as_deref())
        .map_err(friendly_open_error)?;

    let id = *next_id;
    *next_id += 1;
    let entry = OpenDoc { doc, path };
    let info = make_info(id, &entry);
    docs.insert(id, entry);
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
    docs: &HashMap<u32, OpenDoc>,
    id: u32,
    index: u16,
    target_width: u32,
    jpeg: bool,
) -> Result<RenderedPage, String> {
    let doc = &docs.get(&id).ok_or("Document is not open")?.doc;
    let page = doc
        .pages()
        .get(index.into())
        .map_err(|e| format!("Page {index} not found: {e}"))?;

    let width = target_width.clamp(64, 8192) as i32;
    let config = PdfRenderConfig::new()
        .set_target_width(width)
        .render_form_data(true)
        .render_annotations(true);

    let bitmap = page
        .render_with_config(&config)
        .map_err(|e| format!("Render failed: {e}"))?;
    let image = bitmap
        .as_image()
        .map_err(|e| format!("Bitmap conversion failed: {e}"))?;

    let (width_px, height_px) = (image.width(), image.height());
    let mut buf = Cursor::new(Vec::new());
    let mime = if jpeg {
        // JPEG can't carry alpha; pdfium bitmaps come back RGBA.
        image::DynamicImage::ImageRgb8(image.to_rgb8())
            .write_to(&mut buf, image::ImageFormat::Jpeg)
            .map_err(|e| format!("JPEG encode failed: {e}"))?;
        "image/jpeg"
    } else {
        image
            .write_to(&mut buf, image::ImageFormat::Png)
            .map_err(|e| format!("PNG encode failed: {e}"))?;
        "image/png"
    };

    let b64 = base64::engine::general_purpose::STANDARD.encode(buf.into_inner());
    Ok(RenderedPage {
        index,
        width_px,
        height_px,
        data_url: format!("data:{mime};base64,{b64}"),
    })
}

fn page_text(docs: &HashMap<u32, OpenDoc>, id: u32, index: u16) -> Result<String, String> {
    let doc = &docs.get(&id).ok_or("Document is not open")?.doc;
    let page = doc
        .pages()
        .get(index.into())
        .map_err(|e| format!("Page {index} not found: {e}"))?;
    let text = page.text().map_err(|e| format!("Text extraction failed: {e}"))?;
    Ok(text.all())
}

fn rotation_to_turns(r: PdfPageRenderRotation) -> i8 {
    match r {
        PdfPageRenderRotation::None => 0,
        PdfPageRenderRotation::Degrees90 => 1,
        PdfPageRenderRotation::Degrees180 => 2,
        PdfPageRenderRotation::Degrees270 => 3,
    }
}

fn turns_to_rotation(turns: i8) -> PdfPageRenderRotation {
    match turns.rem_euclid(4) {
        1 => PdfPageRenderRotation::Degrees90,
        2 => PdfPageRenderRotation::Degrees180,
        3 => PdfPageRenderRotation::Degrees270,
        _ => PdfPageRenderRotation::None,
    }
}

fn rotate_pages(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    indices: &[u16],
    quarter_turns: i8,
) -> Result<DocInfo, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    for &index in indices {
        let mut page = entry
            .doc
            .pages()
            .get(index.into())
            .map_err(|e| format!("Page {index} not found: {e}"))?;
        let current = page.rotation().unwrap_or(PdfPageRenderRotation::None);
        page.set_rotation(turns_to_rotation(rotation_to_turns(current) + quarter_turns));
    }
    Ok(make_info(id, entry))
}

fn delete_pages(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    mut indices: Vec<u16>,
) -> Result<DocInfo, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    if indices.len() >= entry.doc.pages().len() as usize {
        return Err("Cannot delete every page — a PDF needs at least one".to_string());
    }
    // Descending order so earlier deletions don't shift later indices.
    indices.sort_unstable_by(|a, b| b.cmp(a));
    indices.dedup();
    for index in indices {
        entry
            .doc
            .pages()
            .get(index.into())
            .map_err(|e| format!("Page {index} not found: {e}"))?
            .delete()
            .map_err(|e| format!("Could not delete page {index}: {e}"))?;
    }
    Ok(make_info(id, entry))
}

fn reorder_pages(
    pdfium: &'static Pdfium,
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    order: &[u16],
) -> Result<DocInfo, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    let count = entry.doc.pages().len();
    {
        let mut check: Vec<u16> = order.to_vec();
        check.sort_unstable();
        if check.len() != count as usize || check.iter().enumerate().any(|(i, &v)| i as u16 != v) {
            return Err("Reorder must be a permutation of all pages".to_string());
        }
    }

    // PDFium has no in-place move; rebuild into a fresh document in the new
    // order. Page content, annotations and resources are copied; exotic
    // document-level extras (bookmarks pointing at pages) may not survive.
    let mut new_doc = pdfium
        .create_new_pdf()
        .map_err(|e| format!("Could not create document: {e}"))?;
    for (dest, &src) in order.iter().enumerate() {
        new_doc
            .pages_mut()
            .copy_page_from_document(&entry.doc, src.into(), dest as PdfPageIndex)
            .map_err(|e| format!("Could not move page {src}: {e}"))?;
    }
    entry.doc = new_doc;
    Ok(make_info(id, entry))
}

fn save_doc(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    path: Option<PathBuf>,
) -> Result<DocInfo, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    let target = path.unwrap_or_else(|| entry.path.clone());
    entry
        .doc
        .save_to_file(&target)
        .map_err(|e| format!("Save failed: {e}"))?;
    entry.path = target;
    Ok(make_info(id, entry))
}

// ---- annotations ----

fn annot_infos(doc: &PdfDocument<'static>, page_index: u16) -> Result<Vec<AnnotInfo>, String> {
    let page = doc
        .pages()
        .get(page_index.into())
        .map_err(|e| format!("Page {page_index} not found: {e}"))?;
    let mut out = Vec::new();
    for (i, annot) in page.annotations().iter().enumerate() {
        let bounds = annot.bounds().unwrap_or(PdfRect::ZERO);
        out.push(AnnotInfo {
            page: page_index,
            index: i as u16,
            kind: format!("{:?}", annot.annotation_type()),
            left: bounds.left().value,
            bottom: bounds.bottom().value,
            right: bounds.right().value,
            top: bounds.top().value,
            contents: annot.contents(),
        });
    }
    Ok(out)
}

fn list_annotations(
    docs: &HashMap<u32, OpenDoc>,
    id: u32,
    page: u16,
) -> Result<Vec<AnnotInfo>, String> {
    let doc = &docs.get(&id).ok_or("Document is not open")?.doc;
    annot_infos(doc, page)
}

fn pdf_color(c: Rgba) -> PdfColor {
    PdfColor::new(c.r, c.g, c.b, c.a)
}

fn add_highlight(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    page_index: u16,
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
    color: Rgba,
) -> Result<Vec<AnnotInfo>, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    {
        let mut page = entry
            .doc
            .pages()
            .get(page_index.into())
            .map_err(|e| format!("Page {page_index} not found: {e}"))?;
        let rect = PdfRect::new(
            PdfPoints::new(bottom),
            PdfPoints::new(left),
            PdfPoints::new(top),
            PdfPoints::new(right),
        );
        let mut annot = page
            .annotations_mut()
            .create_highlight_annotation()
            .map_err(|e| format!("Could not create highlight: {e}"))?;
        annot.set_bounds(rect).map_err(|e| format!("Could not place highlight: {e}"))?;
        annot
            .attachment_points_mut()
            .create_attachment_point_at_end(PdfQuadPoints::from_rect(&rect))
            .map_err(|e| format!("Could not place highlight: {e}"))?;
        annot
            .set_fill_color(pdf_color(color))
            .map_err(|e| format!("Could not color highlight: {e}"))?;
    }
    annot_infos(&entry.doc, page_index)
}

fn add_note(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    page_index: u16,
    x: f32,
    y: f32,
    contents: &str,
) -> Result<Vec<AnnotInfo>, String> {
    const NOTE_SIZE: f32 = 22.0;
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    {
        let mut page = entry
            .doc
            .pages()
            .get(page_index.into())
            .map_err(|e| format!("Page {page_index} not found: {e}"))?;
        let mut annot = page
            .annotations_mut()
            .create_text_annotation(contents)
            .map_err(|e| format!("Could not create note: {e}"))?;
        annot
            .set_bounds(PdfRect::new(
                PdfPoints::new(y - NOTE_SIZE),
                PdfPoints::new(x),
                PdfPoints::new(y),
                PdfPoints::new(x + NOTE_SIZE),
            ))
            .map_err(|e| format!("Could not place note: {e}"))?;
    }
    annot_infos(&entry.doc, page_index)
}

fn add_ink(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    page_index: u16,
    strokes: &[Vec<(f32, f32)>],
    color: Rgba,
    width: f32,
) -> Result<Vec<AnnotInfo>, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    let strokes: Vec<&Vec<(f32, f32)>> = strokes.iter().filter(|s| s.len() >= 2).collect();
    if strokes.is_empty() {
        return Err("Ink stroke needs at least two points".to_string());
    }

    // Bounding rect over all strokes, padded by the stroke width.
    let (mut l, mut b, mut r, mut t) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in strokes.iter().flat_map(|s| s.iter()) {
        l = l.min(x);
        b = b.min(y);
        r = r.max(x);
        t = t.max(y);
    }
    let pad = width.max(1.0);

    let mut paths = Vec::new();
    for stroke in &strokes {
        let (x0, y0) = stroke[0];
        let mut path = PdfPagePathObject::new(
            &entry.doc,
            PdfPoints::new(x0),
            PdfPoints::new(y0),
            Some(pdf_color(color)),
            Some(PdfPoints::new(width)),
            None,
        )
        .map_err(|e| format!("Could not start stroke: {e}"))?;
        for &(x, y) in &stroke[1..] {
            path.line_to(PdfPoints::new(x), PdfPoints::new(y))
                .map_err(|e| format!("Could not draw stroke: {e}"))?;
        }
        paths.push(path);
    }

    {
        let mut page = entry
            .doc
            .pages()
            .get(page_index.into())
            .map_err(|e| format!("Page {page_index} not found: {e}"))?;
        let mut annot = page
            .annotations_mut()
            .create_ink_annotation()
            .map_err(|e| format!("Could not create ink annotation: {e}"))?;
        annot
            .set_bounds(PdfRect::new(
                PdfPoints::new(b - pad),
                PdfPoints::new(l - pad),
                PdfPoints::new(t + pad),
                PdfPoints::new(r + pad),
            ))
            .map_err(|e| format!("Could not place ink annotation: {e}"))?;
        for path in paths {
            annot
                .objects_mut()
                .add_path_object(path)
                .map_err(|e| format!("Could not attach stroke: {e}"))?;
        }
    }
    annot_infos(&entry.doc, page_index)
}

fn delete_annotation(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    page_index: u16,
    index: u16,
) -> Result<Vec<AnnotInfo>, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    {
        let mut page = entry
            .doc
            .pages()
            .get(page_index.into())
            .map_err(|e| format!("Page {page_index} not found: {e}"))?;
        let annot = page
            .annotations()
            .get(index.into())
            .map_err(|e| format!("Annotation not found: {e}"))?;
        page.annotations_mut()
            .delete_annotation(annot)
            .map_err(|e| format!("Could not delete annotation: {e}"))?;
    }
    annot_infos(&entry.doc, page_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// PDFium is not thread-safe and must be initialized once: tests share a
    /// single instance and serialize on this lock (cargo runs tests in
    /// parallel threads by default — unguarded, the process crashes natively).
    static PDFIUM: OnceLock<Option<&'static Pdfium>> = OnceLock::new();
    static ENGINE_LOCK: Mutex<()> = Mutex::new(());

    fn test_pdfium() -> Option<&'static Pdfium> {
        *PDFIUM.get_or_init(|| match bind_pdfium(None) {
            Ok(p) => Some(Box::leak(Box::new(p))),
            Err(_) => {
                eprintln!("pdfium.dll not present — skipping engine test");
                None
            }
        })
    }

    /// End-to-end engine test against the real pdfium.dll: build a 3-page PDF,
    /// then rotate, reorder, delete, save, and re-open it.
    #[test]
    fn organize_roundtrip() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };

        // Three pages with distinct heights so order is observable.
        let tmp = std::env::temp_dir().join("zugapdf-test-organize.pdf");
        {
            let mut doc = pdfium.create_new_pdf().unwrap();
            for height in [500.0, 600.0, 700.0] {
                doc.pages_mut()
                    .create_page_at_end(PdfPagePaperSize::Custom(
                        PdfPoints::new(400.0),
                        PdfPoints::new(height),
                    ))
                    .unwrap();
            }
            doc.save_to_file(&tmp).unwrap();
        }

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        assert_eq!(info.page_count, 3);
        let heights: Vec<i32> = info.pages.iter().map(|p| p.height as i32).collect();
        assert_eq!(heights, vec![500, 600, 700]);

        // Rotate page 0 by 90°: reported dims must swap (or rotation isn't
        // reflected in rendering either — this assert guards that contract).
        let info = rotate_pages(&mut docs, info.id, &[0], 1).unwrap();
        assert_eq!(info.pages[0].width as i32, 500);
        assert_eq!(info.pages[0].height as i32, 400);

        // Un-rotate, then reorder [2, 0, 1].
        let info = rotate_pages(&mut docs, info.id, &[0], -1).unwrap();
        let info = reorder_pages(pdfium, &mut docs, info.id, &[2, 0, 1]).unwrap();
        let heights: Vec<i32> = info.pages.iter().map(|p| p.height as i32).collect();
        assert_eq!(heights, vec![700, 500, 600]);

        // Delete the middle page.
        let info = delete_pages(&mut docs, info.id, vec![1]).unwrap();
        let heights: Vec<i32> = info.pages.iter().map(|p| p.height as i32).collect();
        assert_eq!(heights, vec![700, 600]);

        // Refuse to delete every remaining page.
        assert!(delete_pages(&mut docs, info.id, vec![0, 1]).is_err());

        // Save in place (byte-vec loading means no file-handle conflict),
        // re-open, and confirm the changes persisted.
        let info = save_doc(&mut docs, info.id, None).unwrap();
        let reopened = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        assert_eq!(reopened.page_count, 2);
        let heights: Vec<i32> = reopened.pages.iter().map(|p| p.height as i32).collect();
        assert_eq!(heights, vec![700, 600]);

        drop(info);
        let _ = std::fs::remove_file(tmp);
    }

    /// Highlight + note + ink survive save → reopen, and delete works.
    #[test]
    fn annotations_roundtrip() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };

        let tmp = std::env::temp_dir().join("zugapdf-test-annots.pdf");
        {
            let mut doc = pdfium.create_new_pdf().unwrap();
            doc.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
            doc.save_to_file(&tmp).unwrap();
        }

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        let yellow = Rgba { r: 255, g: 220, b: 60, a: 128 };

        let annots =
            add_highlight(&mut docs, info.id, 0, 100.0, 700.0, 300.0, 720.0, yellow).unwrap();
        assert_eq!(annots.len(), 1);
        assert_eq!(annots[0].kind, "Highlight");

        let annots = add_note(&mut docs, info.id, 0, 50.0, 650.0, "hello from zugapdf").unwrap();
        assert_eq!(annots.len(), 2);
        assert_eq!(annots[1].contents.as_deref(), Some("hello from zugapdf"));

        let stroke = vec![(120.0, 500.0), (180.0, 540.0), (240.0, 500.0)];
        let annots = add_ink(&mut docs, info.id, 0, &[stroke], yellow, 2.0).unwrap();
        assert_eq!(annots.len(), 3);
        assert_eq!(annots[2].kind, "Ink");

        // Persist and reopen: all three annotations must still be there.
        save_doc(&mut docs, info.id, None).unwrap();
        let reopened = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        let annots = list_annotations(&docs, reopened.id, 0).unwrap();
        assert_eq!(annots.len(), 3);
        assert_eq!(annots[1].contents.as_deref(), Some("hello from zugapdf"));

        // Delete the highlight; two remain.
        let annots = delete_annotation(&mut docs, reopened.id, 0, 0).unwrap();
        assert_eq!(annots.len(), 2);

        let _ = std::fs::remove_file(tmp);
    }
}

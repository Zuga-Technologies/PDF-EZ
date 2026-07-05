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

#[derive(Serialize, Clone, Debug)]
pub struct PageDims {
    /// PDF points (1/72 in)
    pub width: f32,
    pub height: f32,
}

#[derive(Serialize, Clone, Debug)]
pub struct DocInfo {
    pub id: u32,
    pub page_count: u16,
    pub pages: Vec<PageDims>,
    pub title: Option<String>,
    pub path: String,
    /// True when saves will be AES-256 encrypted with the stored password.
    pub has_password: bool,
}

#[derive(Serialize, Clone)]
pub struct CompressOutcome {
    pub info: DocInfo,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

#[derive(Serialize, Clone)]
pub struct ObjInfo {
    pub page: u16,
    pub index: u16,
    /// "Text" | "Image" — other object kinds aren't exposed for editing.
    pub kind: String,
    pub text: String,
    pub font_size: f32,
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

#[derive(Serialize, Clone)]
pub struct FieldInfo {
    pub page: u16,
    /// Index of the widget annotation hosting this field on its page.
    pub index: u16,
    /// "Text" | "Checkbox" | "RadioButton" | other (read-only in the UI)
    pub kind: String,
    pub name: String,
    pub value: String,
    pub checked: bool,
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

#[derive(Serialize, Clone)]
pub struct SearchHit {
    /// Matches sharing a hit number are one result spanning multiple lines.
    pub hit: u32,
    pub page: u16,
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

/// A sensitive-data match found by the pattern sweep. Bounds are display space
/// (what the UI draws), so the same rect round-trips straight back into a
/// redaction request.
#[derive(Serialize, Clone)]
pub struct RedactMatch {
    pub page: u16,
    /// "SSN" | "Email" | "Card" | "Phone"
    pub kind: String,
    /// The matched text, partially masked so the preview list never re-exposes
    /// the full secret it is about to remove.
    pub sample: String,
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

/// A rectangle to redact, in display space (origin bottom-left of the rendered,
/// rotated page) — the same space every coordinate-carrying command speaks.
#[derive(serde::Deserialize, Clone, Copy)]
pub struct RedactRect {
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

#[derive(Serialize, Clone, Debug)]
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
    /// OCR every page (Tesseract): pages are rasterized at the given DPI and
    /// rebuilt with an invisible searchable text layer.
    OcrDocument {
        id: u32,
        dpi: u16,
        reply: Sender<Result<DocInfo, String>>,
    },
    /// Render every page to PNG/JPEG files in a directory.
    ExportPages {
        id: u32,
        dir: PathBuf,
        jpeg: bool,
        dpi: u16,
        reply: Sender<Result<u16, String>>,
    },
    /// Extract all text to a .txt file.
    ExportText {
        id: u32,
        path: PathBuf,
        reply: Sender<Result<u16, String>>,
    },
    /// Build a new PDF from image files (one page per image).
    ImagesToPdf {
        paths: Vec<PathBuf>,
        out: PathBuf,
        reply: Sender<Result<(), String>>,
    },
    /// Diagonal or straight text watermark across every page.
    AddWatermark {
        id: u32,
        text: String,
        size: f32,
        /// 0-100
        opacity: u8,
        diagonal: bool,
        reply: Sender<Result<DocInfo, String>>,
    },
    /// Header or footer line on every page. `template` supports {page} and
    /// {pages}. `align`: 0 = left, 1 = center, 2 = right.
    AddHeaderFooter {
        id: u32,
        top: bool,
        align: u8,
        template: String,
        reply: Sender<Result<DocInfo, String>>,
    },
    /// Set (Some) or remove (None) the password applied at save time.
    SetDocumentPassword {
        id: u32,
        password: Option<String>,
        reply: Sender<Result<DocInfo, String>>,
    },
    ListPageObjects {
        id: u32,
        page: u16,
        reply: Sender<Result<Vec<ObjInfo>, String>>,
    },
    SetTextObject {
        id: u32,
        page: u16,
        index: u16,
        text: String,
        reply: Sender<Result<Vec<ObjInfo>, String>>,
    },
    DeletePageObject {
        id: u32,
        page: u16,
        index: u16,
        reply: Sender<Result<Vec<ObjInfo>, String>>,
    },
    ListFormFields {
        id: u32,
        page: u16,
        reply: Sender<Result<Vec<FieldInfo>, String>>,
    },
    /// Text fields: `value` is the new text. Checkboxes: "true"/"false".
    /// Radio buttons: any value checks this widget's option.
    SetFormField {
        id: u32,
        page: u16,
        index: u16,
        value: String,
        reply: Sender<Result<Vec<FieldInfo>, String>>,
    },
    SearchDocument {
        id: u32,
        query: String,
        match_case: bool,
        reply: Sender<Result<Vec<SearchHit>, String>>,
    },
    /// Structural compression via the bundled qpdf CLI (object streams,
    /// stream recompression). Result replaces the in-memory document;
    /// the user still saves to persist.
    Compress {
        id: u32,
        reply: Sender<Result<CompressOutcome, String>>,
    },
    /// Append every page of each file (in order) to the end of the document.
    MergeAppend {
        id: u32,
        paths: Vec<PathBuf>,
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
    /// Scan every page for the selected sensitive-data patterns. Read-only —
    /// returns candidate rects for the UI to preview before redacting.
    FindPatterns {
        id: u32,
        /// Any of "SSN" | "Email" | "Card" | "Phone".
        kinds: Vec<String>,
        reply: Sender<Result<Vec<RedactMatch>, String>>,
    },
    /// TRUE redaction: permanently remove every page object touching a rect,
    /// then lay an opaque black box over it. Content is gone from the file, not
    /// merely hidden.
    RedactRegions {
        id: u32,
        page: u16,
        rects: Vec<RedactRect>,
        reply: Sender<Result<Vec<ObjInfo>, String>>,
    },
    /// PAdES B-B digital signature from a PKCS#12 (.pfx). Writes the signed PDF
    /// to `out` and reloads it as the in-memory document.
    SignDocument {
        id: u32,
        pfx_path: PathBuf,
        password: String,
        reason: String,
        name: String,
        location: String,
        out: PathBuf,
        reply: Sender<Result<DocInfo, String>>,
    },
}

struct OpenDoc {
    doc: PdfDocument<'static>,
    path: PathBuf,
    /// Highlights created THIS session, keyed (page, annot index). They have
    /// no appearance stream yet (pdfium can't write one), so the renderer
    /// composites them; save writes real AP streams (ap.rs) and clears this.
    session_highlights: std::collections::HashSet<(u16, u16)>,
    /// Password the in-memory document was loaded with. PDFium KEEPS that
    /// encryption in every snapshot it saves, so qpdf needs this to read
    /// snapshots. Fixed for the lifetime of the load.
    doc_password: Option<String>,
    /// Password the user wants on saved files (Protect dialog). None =
    /// save unencrypted (decrypting the snapshot if needed).
    save_password: Option<String>,
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

/// Locate the bundled qpdf.exe: resource dir in production, repo copy in dev.
fn find_qpdf(resource_dir: Option<&PathBuf>) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(res) = resource_dir {
        candidates.push(res.join("qpdf").join("bin").join("qpdf.exe"));
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("qpdf").join("bin").join("qpdf.exe"),
    );
    candidates.into_iter().find(|p| p.exists())
}

/// Locate the bundled tesseract.exe (same convention as qpdf).
fn find_tesseract(resource_dir: Option<&PathBuf>) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(res) = resource_dir {
        candidates.push(res.join("tesseract").join("tesseract.exe"));
    }
    candidates
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tesseract").join("tesseract.exe"));
    candidates.into_iter().find(|p| p.exists())
}

pub fn spawn(resource_dir: Option<PathBuf>) -> PdfWorkerHandle {
    let (tx, rx) = channel::<PdfRequest>();

    std::thread::Builder::new()
        .name("pdf-engine".into())
        .spawn(move || {
            let qpdf = find_qpdf(resource_dir.as_ref());
            let tesseract = find_tesseract(resource_dir.as_ref());
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
                        let result = save_doc(pdfium, &mut docs, id, path, qpdf.as_deref());
                        let _ = reply.send(result);
                    }
                    PdfRequest::OcrDocument { id, dpi, reply } => {
                        let _ = reply.send(ocr_document(pdfium, &mut docs, id, dpi, tesseract.as_deref()));
                    }
                    PdfRequest::ExportPages { id, dir, jpeg, dpi, reply } => {
                        let _ = reply.send(export_pages(&docs, id, &dir, jpeg, dpi));
                    }
                    PdfRequest::ExportText { id, path, reply } => {
                        let _ = reply.send(export_text(&docs, id, &path));
                    }
                    PdfRequest::ImagesToPdf { paths, out, reply } => {
                        let _ = reply.send(images_to_pdf(pdfium, &paths, &out));
                    }
                    PdfRequest::AddWatermark { id, text, size, opacity, diagonal, reply } => {
                        let _ = reply.send(add_watermark(&mut docs, id, &text, size, opacity, diagonal));
                    }
                    PdfRequest::AddHeaderFooter { id, top, align, template, reply } => {
                        let _ = reply.send(add_header_footer(&mut docs, id, top, align, &template));
                    }
                    PdfRequest::SetDocumentPassword { id, password, reply } => {
                        let result = docs
                            .get_mut(&id)
                            .ok_or("Document is not open".to_string())
                            .map(|entry| {
                                entry.save_password = password.filter(|p| !p.is_empty());
                                make_info(id, entry)
                            });
                        let _ = reply.send(result);
                    }
                    PdfRequest::ListPageObjects { id, page, reply } => {
                        let _ = reply.send(list_page_objects(&docs, id, page));
                    }
                    PdfRequest::SetTextObject { id, page, index, text, reply } => {
                        let _ = reply.send(set_text_object(&mut docs, id, page, index, &text));
                    }
                    PdfRequest::DeletePageObject { id, page, index, reply } => {
                        let _ = reply.send(delete_page_object(&mut docs, id, page, index));
                    }
                    PdfRequest::ListFormFields { id, page, reply } => {
                        let _ = reply.send(list_form_fields(&docs, id, page));
                    }
                    PdfRequest::SetFormField { id, page, index, value, reply } => {
                        let _ = reply.send(set_form_field(&mut docs, id, page, index, &value));
                    }
                    PdfRequest::SearchDocument { id, query, match_case, reply } => {
                        let _ = reply.send(search_document(&docs, id, &query, match_case));
                    }
                    PdfRequest::Compress { id, reply } => {
                        let _ = reply.send(compress(pdfium, &mut docs, id, qpdf.as_deref()));
                    }
                    PdfRequest::MergeAppend { id, paths, reply } => {
                        let _ = reply.send(merge_append(pdfium, &mut docs, id, &paths));
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
                    PdfRequest::FindPatterns { id, kinds, reply } => {
                        let _ = reply.send(find_patterns(&docs, id, &kinds));
                    }
                    PdfRequest::RedactRegions { id, page, rects, reply } => {
                        let _ = reply.send(redact_regions(&mut docs, id, page, &rects));
                    }
                    PdfRequest::SignDocument {
                        id, pfx_path, password, reason, name, location, out, reply,
                    } => {
                        let _ = reply.send(sign_document(
                            pdfium, &mut docs, id, &pfx_path, &password, &reason, &name,
                            &location, out, qpdf.as_deref(),
                        ));
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
        PdfRequest::SetDocumentPassword { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::OcrDocument { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::ExportPages { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::ExportText { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::ImagesToPdf { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::AddWatermark { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::AddHeaderFooter { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::ListPageObjects { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::SetTextObject { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::DeletePageObject { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::ListFormFields { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::SetFormField { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::SearchDocument { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::Compress { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::MergeAppend { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::ListAnnotations { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::AddHighlight { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::AddNote { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::AddInk { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::DeleteAnnotation { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::FindPatterns { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::RedactRegions { reply, .. } => drop(reply.send(Err(e))),
        PdfRequest::SignDocument { reply, .. } => drop(reply.send(Err(e))),
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
        has_password: entry.save_password.is_some(),
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
    // Give AP-less markup annotations real appearance streams before pdfium
    // sees the bytes — highlights then render natively (encrypted bytes pass
    // through and get fixed on the decrypted save path instead).
    let bytes = crate::ap::fix_appearance_streams(bytes);
    let doc = pdfium
        .load_pdf_from_byte_vec(bytes, password.as_deref())
        .map_err(friendly_open_error)?;

    let id = *next_id;
    *next_id += 1;
    let password = password.filter(|p| !p.is_empty());
    let entry = OpenDoc {
        doc,
        path,
        doc_password: password.clone(),
        save_password: password,
        session_highlights: Default::default(),
    };
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
    let entry = docs.get(&id).ok_or("Document is not open")?;
    let doc = &entry.doc;
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
    let mut image = bitmap
        .as_image()
        .map_err(|e| format!("Bitmap conversion failed: {e}"))?;

    composite_highlights(&page, index, &entry.session_highlights, &mut image);

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

/// Highlights created THIS SESSION have no appearance stream yet (pdfium
/// can't write one), so they'd render as nothing — multiply-blend them onto
/// the bitmap. Highlights from opened/saved files carry real AP streams
/// (ap.rs) and render natively, so they're skipped here.
fn composite_highlights(
    page: &PdfPage,
    page_index: u16,
    session: &std::collections::HashSet<(u16, u16)>,
    image: &mut image::DynamicImage,
) {
    use image::{GenericImage, GenericImageView};

    let w_pts = page.width().value;
    let h_pts = page.height().value;
    if w_pts <= 0.0 || h_pts <= 0.0 {
        return;
    }
    let rotation = page.rotation().unwrap_or(PdfPageRenderRotation::None);
    // width()/height() already reflect /Rotate; bounds are in UNROTATED space.
    let (uw, uh) = match rotation {
        PdfPageRenderRotation::Degrees90 | PdfPageRenderRotation::Degrees270 => (h_pts, w_pts),
        _ => (w_pts, h_pts),
    };
    let scale = image.width() as f32 / w_pts;

    for (i, annot) in page.annotations().iter().enumerate() {
        if annot.annotation_type() != PdfPageAnnotationType::Highlight
            || !session.contains(&(page_index, i as u16))
        {
            continue;
        }
        let Ok(bounds) = annot.bounds() else { continue };
        // NOTE: annot.fill_color() (FPDFAnnot_GetColor) access-violates here,
        // so the highlight color is fixed to the app's marker yellow for now.
        let color = PdfColor::new(255, 220, 60, 255);
        let alpha = 0.45;

        // Map unrotated PDF-space corners into bitmap pixels.
        let map = |x: f32, y: f32| -> (f32, f32) {
            match rotation {
                PdfPageRenderRotation::None => (x * scale, (uh - y) * scale),
                PdfPageRenderRotation::Degrees90 => (y * scale, x * scale),
                PdfPageRenderRotation::Degrees180 => ((uw - x) * scale, y * scale),
                PdfPageRenderRotation::Degrees270 => ((uh - y) * scale, (uw - x) * scale),
            }
        };
        let (u1, v1) = map(bounds.left().value, bounds.bottom().value);
        let (u2, v2) = map(bounds.right().value, bounds.top().value);
        let (x0, x1) = (u1.min(u2).max(0.0) as u32, u1.max(u2).min(image.width() as f32) as u32);
        let (y0, y1) = (v1.min(v2).max(0.0) as u32, v1.max(v2).min(image.height() as f32) as u32);

        for py in y0..y1 {
            for px in x0..x1 {
                let mut p = image.get_pixel(px, py);
                for (c, tint) in [(0, color.red()), (1, color.green()), (2, color.blue())] {
                    let base = p[c] as f32;
                    let multiplied = base * tint as f32 / 255.0;
                    p[c] = (base * (1.0 - alpha) + multiplied * alpha) as u8;
                }
                image.put_pixel(px, py, p);
            }
        }
    }
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

/// Display space (what the UI sees: the rendered, rotated page with a
/// bottom-left origin) vs page space (unrotated coordinates annotations and
/// objects are stored in). All coordinate-carrying APIs speak DISPLAY space;
/// these helpers convert at the engine boundary.
#[derive(Clone, Copy)]
struct SpaceCtx {
    turns: i8,
    /// Unrotated page dims in points.
    uw: f32,
    uh: f32,
}

fn space_ctx(page: &PdfPage) -> SpaceCtx {
    let turns = rotation_to_turns(page.rotation().unwrap_or(PdfPageRenderRotation::None));
    // width()/height() reflect /Rotate — swap back for unrotated dims.
    let (uw, uh) = if turns % 2 == 1 {
        (page.height().value, page.width().value)
    } else {
        (page.width().value, page.height().value)
    };
    SpaceCtx { turns, uw, uh }
}

fn point_to_display(c: SpaceCtx, x: f32, y: f32) -> (f32, f32) {
    match c.turns.rem_euclid(4) {
        1 => (y, c.uw - x),
        2 => (c.uw - x, c.uh - y),
        3 => (c.uh - y, x),
        _ => (x, y),
    }
}

fn point_to_page(c: SpaceCtx, dx: f32, dy: f32) -> (f32, f32) {
    match c.turns.rem_euclid(4) {
        1 => (c.uw - dy, dx),
        2 => (c.uw - dx, c.uh - dy),
        3 => (dy, c.uh - dx),
        _ => (dx, dy),
    }
}

/// (left, bottom, right, top) page space → display space.
fn rect_to_display(c: SpaceCtx, l: f32, b: f32, r: f32, t: f32) -> (f32, f32, f32, f32) {
    let (x1, y1) = point_to_display(c, l, b);
    let (x2, y2) = point_to_display(c, r, t);
    (x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2))
}

/// (left, bottom, right, top) display space → page space.
fn rect_to_page(c: SpaceCtx, l: f32, b: f32, r: f32, t: f32) -> (f32, f32, f32, f32) {
    let (x1, y1) = point_to_page(c, l, b);
    let (x2, y2) = point_to_page(c, r, t);
    (x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2))
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
    for index in indices.iter().copied() {
        entry
            .doc
            .pages()
            .get(index.into())
            .map_err(|e| format!("Page {index} not found: {e}"))?
            .delete()
            .map_err(|e| format!("Could not delete page {index}: {e}"))?;
    }
    // Remap session-highlight page numbers past the removed pages.
    entry.session_highlights = entry
        .session_highlights
        .iter()
        .filter(|&&(p, _)| !indices.contains(&p))
        .map(|&(p, i)| (p - indices.iter().filter(|&&d| d < p).count() as u16, i))
        .collect();
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
    // Remap session-highlight page numbers to their new positions.
    entry.session_highlights = entry
        .session_highlights
        .iter()
        .filter_map(|&(p, i)| {
            order.iter().position(|&src| src == p).map(|dest| (dest as u16, i))
        })
        .collect();
    Ok(make_info(id, entry))
}

fn run_qpdf(cmd: &mut std::process::Command) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = cmd.output().map_err(|e| format!("Could not run qpdf: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "Encryption step failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Save pipeline: snapshot → decrypt (if the in-memory doc carries load-time
/// encryption) → write real appearance streams (ap.rs) → encrypt to the
/// user's save password or write plain → reload the fixed bytes so the
/// in-memory doc matches the file exactly.
fn save_doc(
    pdfium: &'static Pdfium,
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    path: Option<PathBuf>,
    qpdf: Option<&std::path::Path>,
) -> Result<DocInfo, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    let target = path.unwrap_or_else(|| entry.path.clone());
    let stamp = std::process::id();

    let temp1 = std::env::temp_dir().join(format!("pdez-save-a-{stamp}-{id}.pdf"));
    entry.doc.save_to_file(&temp1).map_err(|e| format!("Save failed: {e}"))?;

    let plain_bytes = if let Some(old) = &entry.doc_password {
        let qpdf = qpdf.ok_or("qpdf is not installed — cannot save an encrypted PDF")?;
        let temp2 = std::env::temp_dir().join(format!("pdez-save-b-{stamp}-{id}.pdf"));
        let result = run_qpdf(
            std::process::Command::new(qpdf)
                .arg(format!("--password={old}"))
                .arg("--decrypt")
                .arg(&temp1)
                .arg(&temp2),
        );
        let bytes = result.and_then(|_| {
            std::fs::read(&temp2).map_err(|e| format!("Save failed: {e}"))
        });
        let _ = std::fs::remove_file(&temp2);
        bytes
    } else {
        std::fs::read(&temp1).map_err(|e| format!("Save failed: {e}"))
    };
    let _ = std::fs::remove_file(&temp1);
    let plain_bytes = plain_bytes?;

    let fixed = crate::ap::fix_appearance_streams(plain_bytes);

    match &entry.save_password {
        None => {
            std::fs::write(&target, &fixed).map_err(|e| format!("Save failed: {e}"))?;
        }
        Some(pw) => {
            let qpdf = qpdf.ok_or("qpdf is not installed — cannot save with a password")?;
            let temp3 = std::env::temp_dir().join(format!("pdez-save-c-{stamp}-{id}.pdf"));
            std::fs::write(&temp3, &fixed).map_err(|e| format!("Save failed: {e}"))?;
            let result = run_qpdf(
                std::process::Command::new(qpdf)
                    .arg("--encrypt")
                    .arg(pw)
                    .arg(pw)
                    .arg("256")
                    .arg("--")
                    .arg(&temp3)
                    .arg(&target),
            );
            let _ = std::fs::remove_file(&temp3);
            result?;
        }
    }

    // Reload so the in-memory doc has the appearance streams too — session
    // highlights now render natively instead of via compositing.
    entry.doc = pdfium
        .load_pdf_from_byte_vec(fixed, None)
        .map_err(|e| format!("Saved file failed to reload: {e}"))?;
    entry.doc_password = None;
    entry.session_highlights.clear();
    entry.path = target;
    Ok(make_info(id, entry))
}

// ---- OCR ----

/// Rasterize each page, run Tesseract's searchable-PDF output on it, and
/// rebuild the document from the per-page results. Pages become images with
/// an invisible text layer — the standard treatment for scanned documents.
fn ocr_document(
    pdfium: &'static Pdfium,
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    dpi: u16,
    tesseract: Option<&std::path::Path>,
) -> Result<DocInfo, String> {
    let tesseract = tesseract.ok_or("Tesseract OCR is not installed with this build")?;
    let tessdata = tesseract.parent().map(|d| d.join("tessdata"));
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    let dpi = dpi.clamp(150, 600);
    let stamp = std::process::id();

    let mut page_pdfs: Vec<Vec<u8>> = Vec::new();
    for (i, page) in entry.doc.pages().iter().enumerate() {
        let width_px = (page.width().value * dpi as f32 / 72.0).round() as i32;
        let config = PdfRenderConfig::new()
            .set_target_width(width_px.clamp(64, 10_000))
            .render_form_data(true)
            .render_annotations(true);
        let bitmap = page
            .render_with_config(&config)
            .map_err(|e| format!("Page {} render failed: {e}", i + 1))?;
        let mut image = bitmap
            .as_image()
            .map_err(|e| format!("Page {} render failed: {e}", i + 1))?;
        composite_highlights(&page, i as u16, &entry.session_highlights, &mut image);

        let png = std::env::temp_dir().join(format!("pdez-ocr-{stamp}-{id}-{i}.png"));
        let out_base = std::env::temp_dir().join(format!("pdez-ocr-{stamp}-{id}-{i}"));
        image
            .save_with_format(&png, image::ImageFormat::Png)
            .map_err(|e| format!("Page {} OCR prep failed: {e}", i + 1))?;

        let mut cmd = std::process::Command::new(tesseract);
        cmd.arg(&png)
            .arg(&out_base)
            .arg("--dpi")
            .arg(dpi.to_string())
            .arg("-l")
            .arg("eng")
            .arg("pdf");
        if let Some(td) = &tessdata {
            cmd.env("TESSDATA_PREFIX", td);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let output = cmd.output().map_err(|e| format!("Could not run Tesseract: {e}"))?;
        let _ = std::fs::remove_file(&png);
        let page_pdf = out_base.with_extension("pdf");
        if !output.status.success() || !page_pdf.exists() {
            let _ = std::fs::remove_file(&page_pdf);
            return Err(format!(
                "OCR failed on page {}: {}",
                i + 1,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let bytes = std::fs::read(&page_pdf).map_err(|e| format!("OCR read failed: {e}"))?;
        let _ = std::fs::remove_file(&page_pdf);
        page_pdfs.push(bytes);
    }

    // Rebuild the document from the per-page searchable PDFs.
    let mut new_doc =
        pdfium.create_new_pdf().map_err(|e| format!("OCR assembly failed: {e}"))?;
    for (i, bytes) in page_pdfs.into_iter().enumerate() {
        let part = pdfium
            .load_pdf_from_byte_vec(bytes, None)
            .map_err(|e| format!("OCR result for page {} failed to open: {e}", i + 1))?;
        new_doc
            .pages_mut()
            .append(&part)
            .map_err(|e| format!("OCR assembly failed on page {}: {e}", i + 1))?;
    }
    entry.doc = new_doc;
    entry.doc_password = None;
    entry.session_highlights.clear(); // annotations are baked into the raster
    Ok(make_info(id, entry))
}

// ---- export & create ----

fn export_pages(
    docs: &HashMap<u32, OpenDoc>,
    id: u32,
    dir: &std::path::Path,
    jpeg: bool,
    dpi: u16,
) -> Result<u16, String> {
    let entry = docs.get(&id).ok_or("Document is not open")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("Could not create folder: {e}"))?;
    let stem = entry
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".to_string());
    let ext = if jpeg { "jpg" } else { "png" };
    let dpi = dpi.clamp(36, 600) as f32;

    let count = entry.doc.pages().len();
    for (i, page) in entry.doc.pages().iter().enumerate() {
        let width_px = (page.width().value * dpi / 72.0).round() as i32;
        let config = PdfRenderConfig::new()
            .set_target_width(width_px.clamp(64, 10_000))
            .render_form_data(true)
            .render_annotations(true);
        let bitmap = page
            .render_with_config(&config)
            .map_err(|e| format!("Page {} render failed: {e}", i + 1))?;
        let mut image = bitmap
            .as_image()
            .map_err(|e| format!("Page {} render failed: {e}", i + 1))?;
        composite_highlights(&page, i as u16, &entry.session_highlights, &mut image);
        let out = dir.join(format!("{stem}-page-{:03}.{ext}", i + 1));
        if jpeg {
            image::DynamicImage::ImageRgb8(image.to_rgb8())
                .save_with_format(&out, image::ImageFormat::Jpeg)
        } else {
            image.save_with_format(&out, image::ImageFormat::Png)
        }
        .map_err(|e| format!("Could not write {}: {e}", out.display()))?;
    }
    Ok(count as u16)
}

fn export_text(docs: &HashMap<u32, OpenDoc>, id: u32, path: &std::path::Path) -> Result<u16, String> {
    let entry = docs.get(&id).ok_or("Document is not open")?;
    let mut out = String::new();
    let count = entry.doc.pages().len();
    for page in entry.doc.pages().iter() {
        if let Ok(text) = page.text() {
            out.push_str(&text.all());
            out.push_str("\n\n");
        }
    }
    std::fs::write(path, out).map_err(|e| format!("Could not write file: {e}"))?;
    Ok(count as u16)
}

fn images_to_pdf(
    pdfium: &'static Pdfium,
    paths: &[PathBuf],
    out: &std::path::Path,
) -> Result<(), String> {
    if paths.is_empty() {
        return Err("No images selected".to_string());
    }
    let mut doc = pdfium.create_new_pdf().map_err(|e| format!("Could not create PDF: {e}"))?;
    for path in paths {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let image = image::open(path).map_err(|e| format!("Could not read {name}: {e}"))?;
        // Page sized so the image lands at ~96 dpi.
        let (w_pts, h_pts) = (image.width() as f32 * 72.0 / 96.0, image.height() as f32 * 72.0 / 96.0);
        let mut page = doc
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::Custom(
                PdfPoints::new(w_pts),
                PdfPoints::new(h_pts),
            ))
            .map_err(|e| format!("Could not add page for {name}: {e}"))?;
        let mut object = PdfPageImageObject::new(&doc, &image)
            .map_err(|e| format!("Could not embed {name}: {e}"))?;
        object
            .scale(w_pts, h_pts)
            .map_err(|e| format!("Could not place {name}: {e}"))?;
        page.objects_mut()
            .add_image_object(object)
            .map_err(|e| format!("Could not place {name}: {e}"))?;
    }
    doc.save_to_file(out).map_err(|e| format!("Could not save PDF: {e}"))?;
    Ok(())
}

// ---- object-level editing (edit text & images) ----

fn obj_infos(doc: &PdfDocument<'static>, page_index: u16) -> Result<Vec<ObjInfo>, String> {
    let page = doc
        .pages()
        .get(page_index.into())
        .map_err(|e| format!("Page {page_index} not found: {e}"))?;
    let ctx = space_ctx(&page);
    let mut out = Vec::new();
    for (i, object) in page.objects().iter().enumerate() {
        let (kind, text, font_size) = if let Some(text_obj) = object.as_text_object() {
            ("Text", text_obj.text(), text_obj.scaled_font_size().value)
        } else if object.as_image_object().is_some() {
            ("Image", String::new(), 0.0)
        } else {
            continue;
        };
        let Ok(bounds) = object.bounds() else { continue };
        let (left, bottom, right, top) = rect_to_display(
            ctx,
            bounds.left().value,
            bounds.bottom().value,
            bounds.right().value,
            bounds.top().value,
        );
        out.push(ObjInfo {
            page: page_index,
            index: i as u16,
            kind: kind.to_string(),
            text,
            font_size,
            left,
            bottom,
            right,
            top,
        });
    }
    Ok(out)
}

fn list_page_objects(
    docs: &HashMap<u32, OpenDoc>,
    id: u32,
    page: u16,
) -> Result<Vec<ObjInfo>, String> {
    let doc = &docs.get(&id).ok_or("Document is not open")?.doc;
    obj_infos(doc, page)
}

fn set_text_object(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    page_index: u16,
    index: u16,
    text: &str,
) -> Result<Vec<ObjInfo>, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;

    // Subset-embedded fonts (name like "ABCDEF+Calibri") only carry the glyphs
    // the document originally used — setting new text through them fails or
    // renders blanks. Detect that up front and rebuild the object with a full
    // standard font instead (same position/size/color), which is what other
    // editors do too.
    let (needs_replacement, size, color, left, bottom) = {
        let page = entry
            .doc
            .pages()
            .get(page_index.into())
            .map_err(|e| format!("Page {page_index} not found: {e}"))?;
        let object = page
            .objects()
            .get(index.into())
            .map_err(|e| format!("Object not found: {e}"))?;
        let text_obj = object.as_text_object().ok_or("That object is not text")?;
        let font_name = text_obj.font().name();
        let subset = font_name.len() > 7 && font_name.as_bytes()[6] == b'+';
        let bounds = object.bounds().map_err(|e| format!("Object bounds: {e}"))?;
        (
            subset,
            text_obj.scaled_font_size(),
            text_obj.fill_color().unwrap_or(PdfColor::new(0, 0, 0, 255)),
            bounds.left(),
            bounds.bottom(),
        )
    };

    if !needs_replacement {
        let mut page = entry
            .doc
            .pages()
            .get(page_index.into())
            .map_err(|e| format!("Page {page_index} not found: {e}"))?;
        let mut object = page
            .objects()
            .get(index.into())
            .map_err(|e| format!("Object not found: {e}"))?;
        let set_result = object
            .as_text_object_mut()
            .ok_or("That object is not text")?
            .set_text(text);
        if set_result.is_ok() {
            page.regenerate_content().map_err(|e| format!("Could not update the page: {e}"))?;
            return obj_infos(&entry.doc, page_index);
        }
        // set_text failed even on a non-subset font — fall through to replace.
    }

    // Rebuild: new Helvetica text object at the old position, then remove the
    // original (remove AFTER add so `index` stays valid).
    let font = entry.doc.fonts_mut().helvetica();
    let mut new_obj = PdfPageTextObject::new(&entry.doc, text, font, size)
        .map_err(|e| format!("Could not rebuild text: {e}"))?;
    new_obj.set_fill_color(color).map_err(|e| format!("Could not rebuild text: {e}"))?;
    {
        let mut page = entry
            .doc
            .pages()
            .get(page_index.into())
            .map_err(|e| format!("Page {page_index} not found: {e}"))?;
        page.set_content_regeneration_strategy(PdfPageContentRegenerationStrategy::Manual);
        // Position at the old object's left/bottom (new objects sit at origin).
        let b = new_obj.bounds().map_err(|e| format!("Could not rebuild text: {e}"))?;
        new_obj
            .translate(left - b.left(), bottom - b.bottom())
            .map_err(|e| format!("Could not rebuild text: {e}"))?;
        page.objects_mut()
            .add_text_object(new_obj)
            .map_err(|e| format!("Could not rebuild text: {e}"))?;
        let removed = page
            .objects_mut()
            .remove_object_at_index(index.into())
            .map_err(|e| format!("Could not replace the original text: {e}"))?;
        page.regenerate_content().map_err(|e| format!("Could not update the page: {e}"))?;
        // Same double-free hazard as delete_page_object — leak the handle.
        std::mem::forget(removed);
    }
    obj_infos(&entry.doc, page_index)
}

fn delete_page_object(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    page_index: u16,
    index: u16,
) -> Result<Vec<ObjInfo>, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    {
        let mut page = entry
            .doc
            .pages()
            .get(page_index.into())
            .map_err(|e| format!("Page {page_index} not found: {e}"))?;
        page.set_content_regeneration_strategy(PdfPageContentRegenerationStrategy::Manual);
        let removed = page
            .objects_mut()
            .remove_object_at_index(index.into())
            .map_err(|e| format!("Could not delete object: {e}"))?;
        page.regenerate_content().map_err(|e| format!("Could not update the page: {e}"))?;
        // Dropping the removed object double-frees inside pdfium (native AV,
        // verified empirically) — deliberately leak the tiny wrapper instead.
        std::mem::forget(removed);
    }
    obj_infos(&entry.doc, page_index)
}

// ---- watermark / header-footer ----

fn add_watermark(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    text: &str,
    size: f32,
    opacity: u8,
    diagonal: bool,
) -> Result<DocInfo, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    if text.trim().is_empty() {
        return Err("Watermark text is empty".to_string());
    }
    let font = entry.doc.fonts_mut().helvetica_bold();
    let alpha = (opacity.min(100) as f32 * 2.55) as u8;
    let size = size.clamp(8.0, 300.0);

    entry
        .doc
        .pages()
        .watermark(|group, _index, width, height| {
            let mut object = PdfPageTextObject::new(
                &entry.doc,
                text,
                font,
                PdfPoints::new(size),
            )?;
            object.set_fill_color(PdfColor::new(128, 128, 128, alpha))?;
            if diagonal {
                object.rotate_counter_clockwise_degrees(45.0)?;
            }
            // Center via the object's post-rotation bounds.
            let bounds = object.bounds()?;
            let cx = (bounds.left().value + bounds.right().value) / 2.0;
            let cy = (bounds.bottom().value + bounds.top().value) / 2.0;
            object.translate(
                PdfPoints::new(width.value / 2.0 - cx),
                PdfPoints::new(height.value / 2.0 - cy),
            )?;
            group.push(&mut object.into())?;
            Ok(())
        })
        .map_err(|e| format!("Watermark failed: {e}"))?;

    Ok(make_info(id, entry))
}

fn add_header_footer(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    top: bool,
    align: u8,
    template: &str,
) -> Result<DocInfo, String> {
    const MARGIN: f32 = 28.0;
    const SIZE: f32 = 10.0;

    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    if template.trim().is_empty() {
        return Err("Header/footer text is empty".to_string());
    }
    let font = entry.doc.fonts_mut().helvetica();
    let total = entry.doc.pages().len();

    entry
        .doc
        .pages()
        .watermark(|group, index, width, height| {
            let line = template
                .replace("{page}", &(index + 1).to_string())
                .replace("{pages}", &total.to_string());
            let mut object =
                PdfPageTextObject::new(&entry.doc, &line, font, PdfPoints::new(SIZE))?;
            object.set_fill_color(PdfColor::new(60, 60, 60, 255))?;
            let text_width = {
                let b = object.bounds()?;
                b.right().value - b.left().value
            };
            let x = match align {
                0 => MARGIN,
                2 => width.value - MARGIN - text_width,
                _ => (width.value - text_width) / 2.0,
            };
            let y = if top { height.value - MARGIN } else { MARGIN - SIZE / 2.0 };
            object.translate(PdfPoints::new(x), PdfPoints::new(y))?;
            group.push(&mut object.into())?;
            Ok(())
        })
        .map_err(|e| format!("Header/footer failed: {e}"))?;

    Ok(make_info(id, entry))
}

// ---- form fields ----

fn field_infos(doc: &PdfDocument<'static>, page_index: u16) -> Result<Vec<FieldInfo>, String> {
    let page = doc
        .pages()
        .get(page_index.into())
        .map_err(|e| format!("Page {page_index} not found: {e}"))?;
    let ctx = space_ctx(&page);
    let mut out = Vec::new();
    for (i, annot) in page.annotations().iter().enumerate() {
        let Some(field) = annot.as_form_field() else { continue };
        let bounds = annot.bounds().unwrap_or(PdfRect::ZERO);
        let (kind, value, checked) = match field.field_type() {
            PdfFormFieldType::Text => (
                "Text",
                field.as_text_field().and_then(|f| f.value()).unwrap_or_default(),
                false,
            ),
            PdfFormFieldType::Checkbox => (
                "Checkbox",
                String::new(),
                field.as_checkbox_field().and_then(|f| f.is_checked().ok()).unwrap_or(false),
            ),
            PdfFormFieldType::RadioButton => (
                "RadioButton",
                String::new(),
                field.as_radio_button_field().and_then(|f| f.is_checked().ok()).unwrap_or(false),
            ),
            other => (
                // Combo/list/button/signature: shown read-only for now.
                match other {
                    PdfFormFieldType::ComboBox => "ComboBox",
                    PdfFormFieldType::ListBox => "ListBox",
                    _ => "Other",
                },
                String::new(),
                false,
            ),
        };
        let (left, bottom, right, top) = rect_to_display(
            ctx,
            bounds.left().value,
            bounds.bottom().value,
            bounds.right().value,
            bounds.top().value,
        );
        out.push(FieldInfo {
            page: page_index,
            index: i as u16,
            kind: kind.to_string(),
            name: field.name().unwrap_or_default(),
            value,
            checked,
            left,
            bottom,
            right,
            top,
        });
    }
    Ok(out)
}

fn list_form_fields(
    docs: &HashMap<u32, OpenDoc>,
    id: u32,
    page: u16,
) -> Result<Vec<FieldInfo>, String> {
    let doc = &docs.get(&id).ok_or("Document is not open")?.doc;
    field_infos(doc, page)
}

fn set_form_field(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    page_index: u16,
    index: u16,
    value: &str,
) -> Result<Vec<FieldInfo>, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    {
        let mut page = entry
            .doc
            .pages()
            .get(page_index.into())
            .map_err(|e| format!("Page {page_index} not found: {e}"))?;
        let mut annots = page.annotations_mut();
        let mut annot = annots
            .get(index.into())
            .map_err(|e| format!("Field not found: {e}"))?;
        let field = annot
            .as_form_field_mut()
            .ok_or("That annotation is not a form field")?;
        match field.field_type() {
            PdfFormFieldType::Text => {
                field
                    .as_text_field_mut()
                    .ok_or("Not a text field")?
                    .set_value(value)
                    .map_err(|e| format!("Could not set value: {e}"))?;
            }
            PdfFormFieldType::Checkbox => {
                field
                    .as_checkbox_field_mut()
                    .ok_or("Not a checkbox")?
                    .set_checked(value == "true")
                    .map_err(|e| format!("Could not toggle checkbox: {e}"))?;
            }
            PdfFormFieldType::RadioButton => {
                field
                    .as_radio_button_field_mut()
                    .ok_or("Not a radio button")?
                    .set_checked()
                    .map_err(|e| format!("Could not select option: {e}"))?;
            }
            _ => return Err("This field type isn't editable yet".to_string()),
        }
    }
    field_infos(&entry.doc, page_index)
}

const MAX_SEARCH_HITS: u32 = 500;

fn search_document(
    docs: &HashMap<u32, OpenDoc>,
    id: u32,
    query: &str,
    match_case: bool,
) -> Result<Vec<SearchHit>, String> {
    let doc = &docs.get(&id).ok_or("Document is not open")?.doc;
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let options = PdfSearchOptions::new().match_case(match_case);
    let mut hits = Vec::new();
    let mut hit_no: u32 = 0;

    'pages: for (page_index, page) in doc.pages().iter().enumerate() {
        let ctx = space_ctx(&page);
        let Ok(text) = page.text() else { continue };
        let Ok(search) = text.search(query, &options) else { continue };
        for segments in search.iter(PdfSearchDirection::SearchForward) {
            for segment in segments.iter() {
                let bounds = segment.bounds();
                let (left, bottom, right, top) = rect_to_display(
                    ctx,
                    bounds.left().value,
                    bounds.bottom().value,
                    bounds.right().value,
                    bounds.top().value,
                );
                hits.push(SearchHit { hit: hit_no, page: page_index as u16, left, bottom, right, top });
            }
            hit_no += 1;
            if hit_no >= MAX_SEARCH_HITS {
                break 'pages;
            }
        }
    }
    Ok(hits)
}

fn compress(
    pdfium: &'static Pdfium,
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    qpdf: Option<&std::path::Path>,
) -> Result<CompressOutcome, String> {
    let qpdf = qpdf.ok_or("qpdf is not installed with this build")?;
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;

    let stamp = std::process::id();
    let temp_in = std::env::temp_dir().join(format!("pdez-compress-in-{stamp}-{id}.pdf"));
    let temp_out = std::env::temp_dir().join(format!("pdez-compress-out-{stamp}-{id}.pdf"));

    // Snapshot the CURRENT in-memory state (including unsaved edits).
    entry
        .doc
        .save_to_file(&temp_in)
        .map_err(|e| format!("Compress failed while snapshotting: {e}"))?;
    let bytes_before = std::fs::metadata(&temp_in).map_err(|e| e.to_string())?.len();

    let mut cmd = std::process::Command::new(qpdf);
    if let Some(pw) = &entry.doc_password {
        // Snapshot carries the load-time encryption; decrypt into memory —
        // save_password re-encrypts at save time.
        cmd.arg(format!("--password={pw}")).arg("--decrypt");
    }
    cmd.arg("--object-streams=generate")
        .arg("--compress-streams=y")
        .arg("--recompress-flate")
        .arg("--compression-level=9")
        .arg(&temp_in)
        .arg(&temp_out);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = cmd.output().map_err(|e| format!("Could not run qpdf: {e}"))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&temp_in);
        let _ = std::fs::remove_file(&temp_out);
        return Err(format!(
            "Compression failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let compressed = std::fs::read(&temp_out).map_err(|e| format!("Compress failed: {e}"))?;
    let bytes_after = compressed.len() as u64;
    let new_doc = pdfium
        .load_pdf_from_byte_vec(compressed, None)
        .map_err(|e| format!("Compressed output failed to reopen: {e}"))?;
    entry.doc = new_doc;
    entry.doc_password = None; // compressed snapshot is decrypted in memory

    let _ = std::fs::remove_file(&temp_in);
    let _ = std::fs::remove_file(&temp_out);
    Ok(CompressOutcome { info: make_info(id, entry), bytes_before, bytes_after })
}

fn merge_append(
    pdfium: &'static Pdfium,
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    paths: &[PathBuf],
) -> Result<DocInfo, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    for path in paths {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let bytes =
            std::fs::read(path).map_err(|e| format!("Could not read {name}: {e}"))?;
        let other = pdfium
            .load_pdf_from_byte_vec(bytes, None)
            .map_err(|e| match e {
                PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
                    format!("{name} is password-protected — open it separately, remove the password, then merge")
                }
                other => format!("Could not open {name}: {other}"),
            })?;
        entry
            .doc
            .pages_mut()
            .append(&other)
            .map_err(|e| format!("Could not merge {name}: {e}"))?;
    }
    Ok(make_info(id, entry))
}

// ---- annotations ----

fn annot_infos(doc: &PdfDocument<'static>, page_index: u16) -> Result<Vec<AnnotInfo>, String> {
    let page = doc
        .pages()
        .get(page_index.into())
        .map_err(|e| format!("Page {page_index} not found: {e}"))?;
    let ctx = space_ctx(&page);
    let mut out = Vec::new();
    for (i, annot) in page.annotations().iter().enumerate() {
        let bounds = annot.bounds().unwrap_or(PdfRect::ZERO);
        let (left, bottom, right, top) = rect_to_display(
            ctx,
            bounds.left().value,
            bounds.bottom().value,
            bounds.right().value,
            bounds.top().value,
        );
        out.push(AnnotInfo {
            page: page_index,
            index: i as u16,
            kind: format!("{:?}", annot.annotation_type()),
            left,
            bottom,
            right,
            top,
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
        // UI coords are display space; annotations store page space.
        let (left, bottom, right, top) =
            rect_to_page(space_ctx(&page), left, bottom, right, top);
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
        let new_index = (page.annotations().len() - 1) as u16;
        entry.session_highlights.insert((page_index, new_index));
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
        let (x, y) = point_to_page(space_ctx(&page), x, y);
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
    let ctx = {
        let page = entry
            .doc
            .pages()
            .get(page_index.into())
            .map_err(|e| format!("Page {page_index} not found: {e}"))?;
        space_ctx(&page)
    };
    // UI stroke points are display space; store page space.
    let strokes: Vec<Vec<(f32, f32)>> = strokes
        .iter()
        .filter(|s| s.len() >= 2)
        .map(|s| s.iter().map(|&(x, y)| point_to_page(ctx, x, y)).collect())
        .collect();
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
    for stroke in strokes.iter() {
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
    // Shift session-highlight indices on this page down past the removal.
    entry.session_highlights = entry
        .session_highlights
        .iter()
        .filter(|&&(p, i)| !(p == page_index && i == index))
        .map(|&(p, i)| if p == page_index && i > index { (p, i - 1) } else { (p, i) })
        .collect();
    annot_infos(&entry.doc, page_index)
}

// ---- redaction ----

/// Two axis-aligned rects (each `left, bottom, right, top`) overlap?
fn rects_intersect(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

/// TRUE redaction: for every page object whose bounds touch a redaction rect,
/// remove it outright (its operators vanish when the content stream is
/// regenerated — this is real deletion, not a cover-up), then draw an opaque
/// black box over each rect as the visible mark. Over-removes a text run that
/// only partly overlaps a rect; that is the safe failure direction — better to
/// drop a neighbouring word than to leak the redacted one.
fn redact_regions(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    page_index: u16,
    rects: &[RedactRect],
) -> Result<Vec<ObjInfo>, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    if rects.is_empty() {
        return obj_infos(&entry.doc, page_index);
    }

    // Display-space rects → page space, and collect the objects they touch.
    let (page_rects, to_remove) = {
        let page = entry
            .doc
            .pages()
            .get(page_index.into())
            .map_err(|e| format!("Page {page_index} not found: {e}"))?;
        let ctx = space_ctx(&page);
        let page_rects: Vec<(f32, f32, f32, f32)> = rects
            .iter()
            .map(|r| rect_to_page(ctx, r.left, r.bottom, r.right, r.top))
            .collect();
        let mut to_remove: Vec<u16> = Vec::new();
        for (i, object) in page.objects().iter().enumerate() {
            let Ok(b) = object.bounds() else { continue };
            let ob = (b.left().value, b.bottom().value, b.right().value, b.top().value);
            if page_rects.iter().any(|&r| rects_intersect(r, ob)) {
                to_remove.push(i as u16);
            }
        }
        (page_rects, to_remove)
    };

    // Build the black cover boxes before taking the page mutably (each borrows
    // the document immutably, same as add_ink builds its strokes up front).
    let mut boxes = Vec::with_capacity(page_rects.len());
    for &(l, b, r, t) in &page_rects {
        let rect = PdfRect::new(
            PdfPoints::new(b),
            PdfPoints::new(l),
            PdfPoints::new(t),
            PdfPoints::new(r),
        );
        let black = PdfColor::new(0, 0, 0, 255);
        let path = PdfPagePathObject::new_rect(
            &entry.doc,
            rect,
            Some(black),
            Some(PdfPoints::new(1.0)),
            Some(black),
        )
        .map_err(|e| format!("Could not build redaction box: {e}"))?;
        boxes.push(path);
    }

    {
        let mut page = entry
            .doc
            .pages()
            .get(page_index.into())
            .map_err(|e| format!("Page {page_index} not found: {e}"))?;
        page.set_content_regeneration_strategy(PdfPageContentRegenerationStrategy::Manual);
        // Descending so each removal keeps the remaining indices valid.
        for &i in to_remove.iter().rev() {
            let removed = page
                .objects_mut()
                .remove_object_at_index(i.into())
                .map_err(|e| format!("Could not remove redacted content: {e}"))?;
            // Same double-free hazard as delete_page_object — leak the wrapper.
            std::mem::forget(removed);
        }
        for path in boxes {
            page.objects_mut()
                .add_path_object(path)
                .map_err(|e| format!("Could not place redaction box: {e}"))?;
        }
        page.regenerate_content().map_err(|e| format!("Could not update the page: {e}"))?;
    }
    obj_infos(&entry.doc, page_index)
}

/// Cap on returned matches so a pathological document can't build an unbounded
/// preview list.
const MAX_REDACT_MATCHES: usize = 1000;

fn pattern_regexes(kinds: &[String]) -> Vec<(&'static str, regex::Regex)> {
    let mut out = Vec::new();
    for k in kinds {
        let spec: Option<(&str, &str)> = match k.as_str() {
            "SSN" => Some(("SSN", r"\b\d{3}-\d{2}-\d{4}\b")),
            "Email" => {
                Some(("Email", r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b"))
            }
            "Phone" => {
                Some(("Phone", r"(?:\+?1[ .\-]?)?\(?\d{3}\)?[ .\-]?\d{3}[ .\-]?\d{4}"))
            }
            "Card" => Some(("Card", r"\b(?:\d[ \-]?){13,19}\b")),
            _ => None,
        };
        if let Some((label, pat)) = spec {
            if let Ok(re) = regex::Regex::new(pat) {
                out.push((label, re));
            }
        }
    }
    out
}

/// Luhn check — filters the loose 13–19 digit card pattern down to real numbers.
fn luhn_ok(s: &str) -> bool {
    let digits: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let mut sum = 0u32;
    for (i, &d) in digits.iter().rev().enumerate() {
        let v = if i % 2 == 1 {
            let x = d * 2;
            if x > 9 { x - 9 } else { x }
        } else {
            d
        };
        sum += v;
    }
    sum % 10 == 0
}

/// Keep the first and last two characters, mask the middle — enough for a user
/// to recognise the match without the preview list re-printing the secret.
fn mask_sample(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 4 {
        return "•".repeat(chars.len());
    }
    let mut out: String = chars[..2].iter().collect();
    out.push_str(&"•".repeat(chars.len() - 4));
    out.extend(chars[chars.len() - 2..].iter());
    out
}

/// Read-only sweep for sensitive-data patterns across every page. Returns
/// candidate rects (display space) for the UI to preview; nothing is altered.
fn find_patterns(
    docs: &HashMap<u32, OpenDoc>,
    id: u32,
    kinds: &[String],
) -> Result<Vec<RedactMatch>, String> {
    let doc = &docs.get(&id).ok_or("Document is not open")?.doc;
    let regexes = pattern_regexes(kinds);
    if regexes.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    'pages: for (page_index, page) in doc.pages().iter().enumerate() {
        let ctx = space_ctx(&page);
        let Ok(text) = page.text() else { continue };

        // Rebuild the page string ourselves so byte offsets line up with a
        // parallel per-character bounds table.
        let mut s = String::new();
        let mut spans: Vec<(usize, Option<PdfRect>)> = Vec::new();
        for c in text.chars().iter() {
            let Some(ch) = c.unicode_char() else { continue };
            spans.push((s.len(), c.tight_bounds().ok()));
            s.push(ch);
        }

        for (label, re) in &regexes {
            for m in re.find_iter(&s) {
                if *label == "Card" && !luhn_ok(m.as_str()) {
                    continue;
                }
                // Union the bounds of every character inside the match.
                let (mut l, mut b, mut r, mut t) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                for (start, bounds) in &spans {
                    if *start >= m.start() && *start < m.end() {
                        if let Some(rc) = bounds {
                            l = l.min(rc.left().value);
                            b = b.min(rc.bottom().value);
                            r = r.max(rc.right().value);
                            t = t.max(rc.top().value);
                        }
                    }
                }
                if l > r || b > t {
                    continue; // no positioned glyphs for this match
                }
                let (left, bottom, right, top) = rect_to_display(ctx, l, b, r, t);
                out.push(RedactMatch {
                    page: page_index as u16,
                    kind: label.to_string(),
                    sample: mask_sample(m.as_str()),
                    left,
                    bottom,
                    right,
                    top,
                });
                if out.len() >= MAX_REDACT_MATCHES {
                    break 'pages;
                }
            }
        }
    }
    Ok(out)
}

// ---- signing ----

/// The current document as plain (decrypted, appearance-fixed) bytes — the same
/// pipeline `save_doc` runs before it optionally re-encrypts.
fn snapshot_plain_bytes(
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    qpdf: Option<&std::path::Path>,
) -> Result<Vec<u8>, String> {
    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    let stamp = std::process::id();
    let temp1 = std::env::temp_dir().join(format!("pdez-sign-a-{stamp}-{id}.pdf"));
    entry.doc.save_to_file(&temp1).map_err(|e| format!("Snapshot failed: {e}"))?;

    let plain = if let Some(old) = &entry.doc_password {
        let qpdf = qpdf.ok_or("qpdf is not installed — cannot sign an encrypted PDF")?;
        let temp2 = std::env::temp_dir().join(format!("pdez-sign-b-{stamp}-{id}.pdf"));
        let result = run_qpdf(
            std::process::Command::new(qpdf)
                .arg(format!("--password={old}"))
                .arg("--decrypt")
                .arg(&temp1)
                .arg(&temp2),
        );
        let bytes = result.and_then(|_| std::fs::read(&temp2).map_err(|e| format!("Snapshot failed: {e}")));
        let _ = std::fs::remove_file(&temp2);
        bytes
    } else {
        std::fs::read(&temp1).map_err(|e| format!("Snapshot failed: {e}"))
    };
    let _ = std::fs::remove_file(&temp1);
    Ok(crate::ap::fix_appearance_streams(plain?))
}

/// Sign the current document and write the result to `out`, then reload the
/// signed PDF as the in-memory document. The signed output is unencrypted (a
/// following qpdf pass would rewrite the file and break the byte range).
#[allow(clippy::too_many_arguments)]
fn sign_document(
    pdfium: &'static Pdfium,
    docs: &mut HashMap<u32, OpenDoc>,
    id: u32,
    pfx_path: &std::path::Path,
    password: &str,
    reason: &str,
    name: &str,
    location: &str,
    out: PathBuf,
    qpdf: Option<&std::path::Path>,
) -> Result<DocInfo, String> {
    let plain = snapshot_plain_bytes(docs, id, qpdf)?;
    let pfx = std::fs::read(pfx_path).map_err(|e| format!("Could not read the .pfx: {e}"))?;
    let params = crate::sign::SignParams {
        reason: reason.to_string(),
        name: name.to_string(),
        location: location.to_string(),
    };
    let signed = crate::sign::sign_pdf(plain, &pfx, password, &params)?;
    std::fs::write(&out, &signed).map_err(|e| format!("Could not write the signed PDF: {e}"))?;

    let entry = docs.get_mut(&id).ok_or("Document is not open")?;
    entry.doc = pdfium
        .load_pdf_from_byte_vec(signed, None)
        .map_err(|e| format!("Signed file failed to reload: {e}"))?;
    entry.doc_password = None;
    entry.save_password = None;
    entry.session_highlights.clear();
    entry.path = out;
    Ok(make_info(id, entry))
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
        let tmp = std::env::temp_dir().join("pdez-test-organize.pdf");
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
        let info = save_doc(pdfium, &mut docs, info.id, None, None).unwrap();
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

        let tmp = std::env::temp_dir().join("pdez-test-annots.pdf");
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
        // Bounds feed the click-to-select hit test — assert, don't just create.
        assert_eq!(annots[0].left as i32, 100);
        assert_eq!(annots[0].top as i32, 720);

        let annots = add_note(&mut docs, info.id, 0, 50.0, 650.0, "hello from pdez").unwrap();
        assert_eq!(annots.len(), 2);
        assert_eq!(annots[1].contents.as_deref(), Some("hello from pdez"));

        let stroke = vec![(120.0, 500.0), (180.0, 540.0), (240.0, 500.0)];
        let annots = add_ink(&mut docs, info.id, 0, &[stroke], yellow, 2.0).unwrap();
        assert_eq!(annots.len(), 3);
        assert_eq!(annots[2].kind, "Ink");
        assert!(annots[2].right > annots[2].left && annots[2].top > annots[2].bottom);

        // Persist and reopen: all three annotations must still be there.
        save_doc(pdfium, &mut docs, info.id, None, None).unwrap();
        let reopened = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        let annots = list_annotations(&docs, reopened.id, 0).unwrap();
        assert_eq!(annots.len(), 3);
        assert_eq!(annots[1].contents.as_deref(), Some("hello from pdez"));

        // Delete the highlight; two remain.
        let annots = delete_annotation(&mut docs, reopened.id, 0, 0).unwrap();
        assert_eq!(annots.len(), 2);

        let _ = std::fs::remove_file(tmp);
    }

    /// The hand-rolled testdata fixtures must parse and render in our engine.
    #[test]
    fn testdata_fixtures_parse_and_render() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };

        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("testdata");
        let fixtures = [("Alpha.pdf", 3u16), ("Bravo.pdf", 2), ("Charlie.pdf", 4)];
        let mut docs = HashMap::new();
        let mut next_id = 1;
        for (name, expected_pages) in fixtures {
            let path = dir.join(name);
            if !path.exists() {
                eprintln!("{name} not generated — skipping");
                continue;
            }
            let info = open_doc(pdfium, &mut docs, &mut next_id, path, None)
                .unwrap_or_else(|e| panic!("{name} failed to open: {e}"));
            assert_eq!(info.page_count, expected_pages, "{name} page count");
            let rendered = render_page(&docs, info.id, 0, 300, true)
                .unwrap_or_else(|e| panic!("{name} failed to render: {e}"));
            assert!(rendered.width_px > 0);
        }
    }

    /// OCR: an image-only (scanned-style) PDF has no searchable text; after
    /// OCR the words are findable, and that survives save → reopen.
    #[test]
    fn ocr_makes_scanned_pdf_searchable() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };
        let Some(_) = find_tesseract(None) else {
            eprintln!("tesseract not fetched — skipping");
            return;
        };
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("testdata").join("Alpha.pdf");
        if !fixture.exists() {
            return;
        }

        let _ = fixture;
        let mut docs = HashMap::new();
        let mut next_id = 1;

        // Build a "scan": a page of normal-sized text rendered to an image,
        // wrapped in an image-only PDF (no text layer).
        let src = std::env::temp_dir().join("pdez-test-scan-text.pdf");
        {
            let mut doc = pdfium.create_new_pdf().unwrap();
            let font = doc.fonts_mut().helvetica();
            let mut page =
                doc.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
            for (i, line) in [
                "The quick brown fox jumps over the lazy dog.",
                "Invoices are due on the last day of the month.",
                "Scanned documents become searchable after recognition.",
            ]
            .iter()
            .enumerate()
            {
                let mut obj =
                    PdfPageTextObject::new(&doc, *line, font, PdfPoints::new(18.0)).unwrap();
                obj.translate(PdfPoints::new(60.0), PdfPoints::new(700.0 - 40.0 * i as f32))
                    .unwrap();
                page.objects_mut().add_text_object(obj).unwrap();
            }
            doc.save_to_file(&src).unwrap();
        }
        let info = open_doc(pdfium, &mut docs, &mut next_id, src.clone(), None).unwrap();
        let png = std::env::temp_dir().join("pdez-test-scan-src.png");
        {
            let rendered = render_page(&docs, info.id, 0, 2480, false).unwrap(); // ~300dpi
            let b64 = rendered.data_url.split(',').nth(1).unwrap();
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
            std::fs::write(&png, bytes).unwrap();
        }
        let scan = std::env::temp_dir().join("pdez-test-scan.pdf");
        images_to_pdf(pdfium, &[png.clone()], &scan).unwrap();
        let _ = std::fs::remove_file(src);

        let sinfo = open_doc(pdfium, &mut docs, &mut next_id, scan.clone(), None).unwrap();
        assert!(
            search_document(&docs, sinfo.id, "quick brown", false).unwrap().is_empty(),
            "scan must start unsearchable"
        );

        let tesseract = find_tesseract(None);
        let after = ocr_document(pdfium, &mut docs, sinfo.id, 300, tesseract.as_deref()).unwrap();
        assert_eq!(after.page_count, 1);
        let hits = search_document(&docs, sinfo.id, "quick brown fox", false).unwrap();
        assert!(!hits.is_empty(), "OCR text layer must be searchable");

        // Survives save → reopen.
        save_doc(pdfium, &mut docs, sinfo.id, None, None).unwrap();
        let reopened = open_doc(pdfium, &mut docs, &mut next_id, scan.clone(), None).unwrap();
        assert!(
            !search_document(&docs, reopened.id, "lazy dog", false).unwrap().is_empty()
        );

        let _ = std::fs::remove_file(png);
        let _ = std::fs::remove_file(scan);
    }

    /// Pattern sweep finds an SSN + email; redaction truly removes the content
    /// (gone from extracted text, survives save→reopen) without touching an
    /// unrelated line, and the preview sample never re-exposes the secret.
    #[test]
    fn redaction_removes_content_and_finds_patterns() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let src = std::env::temp_dir().join("pdez-test-redact.pdf");
        {
            let mut doc = pdfium.create_new_pdf().unwrap();
            let font = doc.fonts_mut().helvetica();
            let mut page =
                doc.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
            for (i, line) in [
                "Employee SSN: 123-45-6789 on file.",
                "Contact: jane.doe@example.com anytime.",
                "This line has nothing sensitive at all.",
            ]
            .iter()
            .enumerate()
            {
                let mut obj =
                    PdfPageTextObject::new(&doc, *line, font, PdfPoints::new(18.0)).unwrap();
                obj.translate(PdfPoints::new(60.0), PdfPoints::new(700.0 - 40.0 * i as f32))
                    .unwrap();
                page.objects_mut().add_text_object(obj).unwrap();
            }
            doc.save_to_file(&src).unwrap();
        }

        let info = open_doc(pdfium, &mut docs, &mut next_id, src.clone(), None).unwrap();

        // Read-only sweep finds both patterns; the sample is masked.
        let matches =
            find_patterns(&docs, info.id, &["SSN".to_string(), "Email".to_string()]).unwrap();
        assert!(matches.iter().any(|m| m.kind == "SSN"), "SSN not found");
        assert!(matches.iter().any(|m| m.kind == "Email"), "Email not found");
        let ssn = matches.iter().find(|m| m.kind == "SSN").unwrap();
        assert!(
            !ssn.sample.contains("6789"),
            "preview must not re-expose the secret: {}",
            ssn.sample
        );

        // Redact the SSN's rectangle.
        let rect = RedactRect { left: ssn.left, bottom: ssn.bottom, right: ssn.right, top: ssn.top };
        redact_regions(&mut docs, info.id, ssn.page, &[rect]).unwrap();

        // In memory: the SSN is truly gone; the unrelated last line survives.
        let text_now =
            docs.get(&info.id).unwrap().doc.pages().get(0).unwrap().text().unwrap().all();
        assert!(
            !text_now.contains("123-45-6789"),
            "redacted SSN still extractable: {text_now}"
        );
        assert!(
            text_now.contains("nothing sensitive"),
            "redaction destroyed an unrelated line: {text_now}"
        );

        // Persist and reopen — the content must not come back from the file.
        save_doc(pdfium, &mut docs, info.id, None, None).unwrap();
        let reopened = open_doc(pdfium, &mut docs, &mut next_id, src.clone(), None).unwrap();
        let text_after =
            docs.get(&reopened.id).unwrap().doc.pages().get(0).unwrap().text().unwrap().all();
        assert!(
            !text_after.contains("123-45-6789"),
            "SSN leaked back into the saved file: {text_after}"
        );

        let _ = std::fs::remove_file(src);
    }

    /// Full live signing path: PDFium snapshot → lopdf sign → reload. Gated on
    /// a local test identity at E:/test-id.pfx (password "test").
    #[test]
    fn sign_real_pdf_through_engine() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };
        let pfx = std::path::Path::new("E:/test-id.pfx");
        if !pfx.exists() {
            eprintln!("no test pfx at E:/test-id.pfx — skipping");
            return;
        }
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("testdata").join("Alpha.pdf");
        if !fixture.exists() {
            return;
        }
        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, fixture, None).unwrap();
        let out = std::env::temp_dir().join("pdez-engine-signed.pdf");
        let qpdf = find_qpdf(None);
        let signed = sign_document(
            pdfium, &mut docs, info.id, pfx, "test", "Approved", "Buga", "", out.clone(),
            qpdf.as_deref(),
        )
        .unwrap();
        assert!(signed.page_count >= 1);

        // The written file carries the signature and reopens in PDFium.
        let bytes = std::fs::read(&out).unwrap();
        let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"adbe.pkcs7.detached"), "missing signature subfilter");
        assert!(has(b"/ByteRange"), "missing byte range");
        open_doc(pdfium, &mut docs, &mut next_id, out.clone(), None)
            .expect("signed PDF must reopen");
        let _ = std::fs::remove_file(&out);
    }

    /// Export: pages → images on disk, text → .txt, images → new PDF.
    #[test]
    fn export_and_create_roundtrip() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("testdata").join("Alpha.pdf");
        if !fixture.exists() {
            eprintln!("Alpha.pdf not generated — skipping");
            return;
        }

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, fixture, None).unwrap();

        // Pages → PNG files.
        let dir = std::env::temp_dir().join("pdez-test-export");
        let _ = std::fs::remove_dir_all(&dir);
        let count = export_pages(&docs, info.id, &dir, false, 96).unwrap();
        assert_eq!(count, 3);
        for i in 1..=3 {
            let f = dir.join(format!("Alpha-page-{i:03}.png"));
            assert!(std::fs::metadata(&f).map(|m| m.len() > 1000).unwrap_or(false), "{f:?}");
        }

        // Text export.
        let txt = std::env::temp_dir().join("pdez-test-export.txt");
        export_text(&docs, info.id, &txt).unwrap();
        let content = std::fs::read_to_string(&txt).unwrap();
        assert!(content.contains("Alpha.pdf - page 2"));

        // Images → PDF: build a small PNG, convert, reopen.
        let png = std::env::temp_dir().join("pdez-test-src.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            192,
            96,
            image::Rgb([200, 40, 40]),
        ))
        .save(&png)
        .unwrap();
        let out = std::env::temp_dir().join("pdez-test-frompix.pdf");
        images_to_pdf(pdfium, &[png.clone(), png.clone()], &out).unwrap();
        let created = open_doc(pdfium, &mut docs, &mut next_id, out.clone(), None).unwrap();
        assert_eq!(created.page_count, 2);
        assert_eq!(created.pages[0].width as i32, 144); // 192px @ 96dpi = 144pt

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_file(txt);
        let _ = std::fs::remove_file(png);
        let _ = std::fs::remove_file(out);
    }

    /// Object edit: rewrite a text object, delete one, persist through save.
    #[test]
    fn edit_objects_roundtrip() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("testdata").join("Alpha.pdf");
        if !fixture.exists() {
            eprintln!("Alpha.pdf not generated — skipping");
            return;
        }
        let tmp = std::env::temp_dir().join("pdez-test-edit.pdf");
        std::fs::copy(&fixture, &tmp).unwrap();

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();

        let objects = list_page_objects(&docs, info.id, 0).unwrap();
        let label = objects.iter().find(|o| o.text.contains("Alpha1")).expect("big label object");
        assert_eq!(label.kind, "Text");
        assert!(label.font_size > 100.0, "center label is 120pt");

        set_text_object(&mut docs, info.id, 0, label.index, "REWRITTEN").unwrap();
        let objects = list_page_objects(&docs, info.id, 0).unwrap();
        assert!(objects.iter().any(|o| o.text == "REWRITTEN"), "object text updated");
        render_page(&docs, info.id, 0, 300, true).unwrap();
        assert_eq!(search_document(&docs, info.id, "REWRITTEN", true).unwrap().len(), 1);
        assert!(search_document(&docs, info.id, "Alpha1", true).unwrap().is_empty());

        // Delete the header text object (contains "page 1").
        let objects = list_page_objects(&docs, info.id, 0).unwrap();
        let header = objects.iter().find(|o| o.text.contains("page 1")).unwrap();
        let remaining = delete_page_object(&mut docs, info.id, 0, header.index).unwrap();
        assert_eq!(remaining.len(), objects.len() - 1);

        save_doc(pdfium, &mut docs, info.id, None, None).unwrap();
        let reopened = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        assert!(!search_document(&docs, reopened.id, "REWRITTEN", true).unwrap().is_empty());

        let _ = std::fs::remove_file(tmp);
    }

    /// Watermark and header/footer add REAL text objects — provable by search.
    #[test]
    fn watermark_and_header_footer() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("testdata").join("Alpha.pdf");
        if !fixture.exists() {
            eprintln!("Alpha.pdf not generated — skipping");
            return;
        }
        let tmp = std::env::temp_dir().join("pdez-test-stamp.pdf");
        std::fs::copy(&fixture, &tmp).unwrap();

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();

        add_watermark(&mut docs, info.id, "CONFIDENTIAL", 60.0, 40, true).unwrap();
        let hits = search_document(&docs, info.id, "CONFIDENTIAL", true).unwrap();
        let count = hits.iter().map(|h| h.hit).max().map(|m| m + 1).unwrap_or(0);
        assert_eq!(count, 3, "watermark on every page");

        add_header_footer(&mut docs, info.id, false, 1, "Page {page} of {pages}").unwrap();
        let hits = search_document(&docs, info.id, "Page 2 of 3", true).unwrap();
        assert!(!hits.is_empty(), "footer with substituted page numbers");

        // Persists through save → reopen.
        save_doc(pdfium, &mut docs, info.id, None, None).unwrap();
        let reopened = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        let hits = search_document(&docs, reopened.id, "CONFIDENTIAL", true).unwrap();
        assert!(!hits.is_empty());

        let _ = std::fs::remove_file(tmp);
    }

    /// Password: set → save encrypts (AES-256); wrong/no password fails to
    /// open; correct password opens; remove → save is unencrypted again.
    #[test]
    fn password_roundtrip() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };
        let Some(qpdf) = find_qpdf(None) else {
            eprintln!("qpdf not fetched — skipping");
            return;
        };

        let tmp = std::env::temp_dir().join("pdez-test-password.pdf");
        {
            let mut doc = pdfium.create_new_pdf().unwrap();
            doc.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
            doc.save_to_file(&tmp).unwrap();
        }

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        assert!(!info.has_password);

        // Set password and save.
        docs.get_mut(&info.id).unwrap().save_password = Some("hunter2".into());
        let saved = save_doc(pdfium, &mut docs, info.id, None, Some(&qpdf)).unwrap();
        assert!(saved.has_password);

        // No password / wrong password → PASSWORD_REQUIRED; correct opens.
        let err = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap_err();
        assert_eq!(err, "PASSWORD_REQUIRED");
        let err =
            open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), Some("wrong".into()))
                .unwrap_err();
        assert_eq!(err, "PASSWORD_REQUIRED");
        let reopened =
            open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), Some("hunter2".into()))
                .unwrap();
        assert!(reopened.has_password, "open password should stick for future saves");

        // Remove password, save, reopen without one.
        docs.get_mut(&reopened.id).unwrap().save_password = None;
        save_doc(pdfium, &mut docs, reopened.id, None, Some(&qpdf)).unwrap();
        let plain = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        assert!(!plain.has_password);

        let _ = std::fs::remove_file(tmp);
    }

    /// Form fill: list fields, set text + checkbox, persist through save.
    #[test]
    fn form_fill_roundtrip() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("testdata").join("Echo.pdf");
        if !fixture.exists() {
            eprintln!("Echo.pdf not generated — skipping");
            return;
        }
        // Work on a copy so the fixture stays pristine.
        let tmp = std::env::temp_dir().join("pdez-test-form.pdf");
        std::fs::copy(&fixture, &tmp).unwrap();

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();

        let fields = list_form_fields(&docs, info.id, 0).unwrap();
        assert_eq!(fields.len(), 5, "3 text + 2 checkbox fields");
        let text = fields.iter().find(|f| f.name == "full_name").unwrap();
        assert_eq!(text.kind, "Text");
        let check = fields.iter().find(|f| f.name == "agree_terms").unwrap();
        assert_eq!(check.kind, "Checkbox");
        assert!(!check.checked);

        set_form_field(&mut docs, info.id, 0, text.index, "Buga Delgado").unwrap();
        let fields = set_form_field(&mut docs, info.id, 0, check.index, "true").unwrap();
        assert!(fields.iter().find(|f| f.name == "agree_terms").unwrap().checked);
        assert_eq!(
            fields.iter().find(|f| f.name == "full_name").unwrap().value,
            "Buga Delgado"
        );

        // Persist and reopen.
        save_doc(pdfium, &mut docs, info.id, None, None).unwrap();
        let reopened = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        let fields = list_form_fields(&docs, reopened.id, 0).unwrap();
        assert_eq!(fields.iter().find(|f| f.name == "full_name").unwrap().value, "Buga Delgado");
        assert!(fields.iter().find(|f| f.name == "agree_terms").unwrap().checked);

        let _ = std::fs::remove_file(tmp);
    }

    /// Search finds per-page hits with sane bounds in the Alpha fixture.
    #[test]
    fn search_finds_hits() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("testdata").join("Alpha.pdf");
        if !path.exists() {
            eprintln!("Alpha.pdf not generated — skipping");
            return;
        }

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, path, None).unwrap();

        // "Alpha" appears twice per page: the header ("Alpha.pdf - page N")
        // and the big center label ("AlphaN").
        let hits = search_document(&docs, info.id, "Alpha", true).unwrap();
        let hit_count = hits.iter().map(|h| h.hit).max().map(|m| m + 1).unwrap_or(0);
        assert_eq!(hit_count, 6, "expected two hits per page");
        let pages: Vec<u16> = hits.iter().map(|h| h.page).collect();
        assert!(pages.contains(&0) && pages.contains(&1) && pages.contains(&2));
        for h in &hits {
            assert!(h.right > h.left && h.top > h.bottom, "degenerate hit bounds");
        }

        // Case-sensitive miss.
        let none = search_document(&docs, info.id, "ALPHA.PDF", true).unwrap();
        assert!(none.is_empty());
    }

    /// Compression round-trips through qpdf and the result opens with the
    /// same page count.
    #[test]
    fn compress_roundtrip() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };
        let Some(qpdf) = find_qpdf(None) else {
            eprintln!("qpdf not fetched — skipping");
            return;
        };

        let tmp = std::env::temp_dir().join("pdez-test-compress.pdf");
        {
            let mut doc = pdfium.create_new_pdf().unwrap();
            for _ in 0..4 {
                doc.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
            }
            doc.save_to_file(&tmp).unwrap();
        }

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        let outcome = compress(pdfium, &mut docs, info.id, Some(&qpdf)).unwrap();
        assert_eq!(outcome.info.page_count, 4);
        assert!(outcome.bytes_before > 0 && outcome.bytes_after > 0);

        // The compressed doc replaced the in-memory one; it must still save.
        save_doc(pdfium, &mut docs, info.id, None, None).unwrap();
        let reopened = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        assert_eq!(reopened.page_count, 4);

        let _ = std::fs::remove_file(tmp);
    }

    /// Merging appends the other file's pages and survives save → reopen.
    #[test]
    fn merge_roundtrip() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };

        let base = std::env::temp_dir().join("pdez-test-merge-base.pdf");
        let extra = std::env::temp_dir().join("pdez-test-merge-extra.pdf");
        for (path, pages, height) in [(&base, 2, 700.0), (&extra, 3, 500.0)] {
            let mut doc = pdfium.create_new_pdf().unwrap();
            for _ in 0..pages {
                doc.pages_mut()
                    .create_page_at_end(PdfPagePaperSize::Custom(
                        PdfPoints::new(400.0),
                        PdfPoints::new(height),
                    ))
                    .unwrap();
            }
            doc.save_to_file(path).unwrap();
        }

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, base.clone(), None).unwrap();
        let info = merge_append(pdfium, &mut docs, info.id, &[extra.clone()]).unwrap();
        assert_eq!(info.page_count, 5);
        let heights: Vec<i32> = info.pages.iter().map(|p| p.height as i32).collect();
        assert_eq!(heights, vec![700, 700, 500, 500, 500]);

        save_doc(pdfium, &mut docs, info.id, None, None).unwrap();
        let reopened = open_doc(pdfium, &mut docs, &mut next_id, base.clone(), None).unwrap();
        assert_eq!(reopened.page_count, 5);

        let _ = std::fs::remove_file(base);
        let _ = std::fs::remove_file(extra);
    }

    /// Manual verification helper: builds a highlighted+saved sample at a
    /// fixed path for opening in external viewers. Run explicitly:
    /// cargo test make_ap_sample -- --ignored --nocapture
    #[test]
    #[ignore]
    fn make_ap_sample() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("testdata").join("Alpha.pdf");
        let out = std::env::temp_dir().join("pdez-ap-sample.pdf");
        std::fs::copy(&fixture, &out).unwrap();

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, out.clone(), None).unwrap();
        let yellow = Rgba { r: 255, g: 220, b: 60, a: 128 };
        // CENTER-page highlight — a mapping bug that collapses appearance
        // streams toward the origin shows immediately with this placement.
        add_highlight(&mut docs, info.id, 0, 200.0, 380.0, 420.0, 420.0, yellow).unwrap();
        save_doc(pdfium, &mut docs, info.id, None, None).unwrap();

        // Fresh-engine render: where does the highlight actually land?
        let reopened = open_doc(pdfium, &mut docs, &mut next_id, out.clone(), None).unwrap();
        let rendered = render_page(&docs, reopened.id, 0, 612, false).unwrap();
        let b64 = rendered.data_url.split(',').nth(1).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let img = image::load_from_memory(&bytes).unwrap().to_rgb8();
        // Page 612x792 at 612px → 1px/pt. Center of highlight: (310, 792-400=392).
        let at_center = img.get_pixel(310, 392);
        let at_origin_area = img.get_pixel(110, 30);
        eprintln!("PROBE center px = {at_center:?}, top-left-area px = {at_origin_area:?}");
        eprintln!("sample at {}", out.display());

    }

    /// Highlights placed on ROTATED pages land where the user pointed, both
    /// in-session (composite) and after save (AP stream). Regression for the
    /// display-space vs page-space mismatch Buga caught in Edge.
    #[test]
    fn rotated_page_highlight_placement() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("testdata").join("Alpha.pdf");
        if !fixture.exists() {
            return;
        }
        let tmp = std::env::temp_dir().join("pdez-test-rot-hl.pdf");
        std::fs::copy(&fixture, &tmp).unwrap();

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        rotate_pages(&mut docs, info.id, &[0], 1).unwrap();

        // Rotated display: 792 wide x 612 tall. Place over a WHITE region
        // (display left-middle) so multiply-yellow is unambiguous in pixels.
        let yellow = Rgba { r: 255, g: 220, b: 60, a: 128 };
        let annots =
            add_highlight(&mut docs, info.id, 0, 100.0, 280.0, 300.0, 320.0, yellow).unwrap();
        // Reported bounds are display space: must match what was requested.
        let hl = annots.last().unwrap();
        assert!((hl.left - 100.0).abs() < 1.0 && (hl.top - 320.0).abs() < 1.0, "{hl:?}");

        let check_center = |docs: &HashMap<u32, OpenDoc>, id: u32, label: &str| {
            let rendered = render_page(docs, id, 0, 792, false).unwrap();
            let b64 = rendered.data_url.split(',').nth(1).unwrap();
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
            let img = image::load_from_memory(&bytes).unwrap().to_rgb8();
            // Display 792x612 at 792px → 1px/pt; inside rect: (200, 612-300).
            let px = img.get_pixel(200, 312);
            // Multiply-yellow over white: red stays high, blue drops hard.
            assert!(px[2] < 200 && px[0] > 150, "{label}: pixel = {px:?}");
        };
        check_center(&docs, info.id, "in-session composite");

        save_doc(pdfium, &mut docs, info.id, None, None).unwrap();
        check_center(&docs, info.id, "post-save AP stream");
        let reopened = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        check_center(&docs, reopened.id, "reopened file");

        let _ = std::fs::remove_file(tmp);
    }

    /// The save pipeline writes REAL appearance streams: a saved highlight
    /// renders natively (no compositing) — i.e., it's visible in Chrome/Edge.
    #[test]
    fn saved_highlight_has_appearance_stream() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };

        let tmp = std::env::temp_dir().join("pdez-test-ap.pdf");
        {
            let mut doc = pdfium.create_new_pdf().unwrap();
            doc.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
            doc.save_to_file(&tmp).unwrap();
        }

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        let yellow = Rgba { r: 255, g: 220, b: 60, a: 128 };
        add_highlight(&mut docs, info.id, 0, 100.0, 700.0, 300.0, 720.0, yellow).unwrap();
        save_doc(pdfium, &mut docs, info.id, None, None).unwrap();

        // 1) Structurally: the saved file's highlight carries /AP.
        let saved = std::fs::read(&tmp).unwrap();
        let parsed = lopdf::Document::load_mem(&saved).unwrap();
        let mut found_ap = false;
        for (_, page_id) in parsed.get_pages() {
            if let Ok(page) = parsed.get_dictionary(page_id) {
                if let Ok(annots) = page.get(b"Annots").and_then(|a| a.as_array()) {
                    for a in annots {
                        // Annot may be inline or a reference.
                        let dict = match a {
                            lopdf::Object::Dictionary(d) => Some(d),
                            lopdf::Object::Reference(r) => {
                                parsed.get_object(*r).and_then(|o| o.as_dict()).ok()
                            }
                            _ => None,
                        };
                        if let Some(dict) = dict {
                            if dict.get(b"Subtype").and_then(|s| s.as_name()).ok()
                                == Some(b"Highlight".as_ref())
                            {
                                found_ap = dict.has(b"AP");
                            }
                        }
                    }
                }
            }
        }
        assert!(found_ap, "saved highlight must carry an /AP appearance stream");

        // 2) Visually: session set cleared by save, so this render uses the
        // AP stream alone — the pixel must still be yellowish.
        assert!(docs.get(&info.id).unwrap().session_highlights.is_empty());
        let rendered = render_page(&docs, info.id, 0, 595, false).unwrap();
        let b64 = rendered.data_url.split(',').nth(1).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let img = image::load_from_memory(&bytes).unwrap().to_rgb8();
        let px = img.get_pixel(200, 132);
        assert!(
            px[0] > 230 && px[1] > 200 && px[2] < 200,
            "AP-stream highlight did not render: pixel = {px:?}"
        );

        let _ = std::fs::remove_file(tmp);
    }

    /// A placed highlight must actually RENDER — PDFium doesn't synthesize
    /// appearance streams, so this catches invisible-annotation regressions.
    #[test]
    fn highlight_renders_visibly() {
        let _guard = ENGINE_LOCK.lock().unwrap();
        let Some(pdfium) = test_pdfium() else { return };

        let tmp = std::env::temp_dir().join("pdez-test-hl-render.pdf");
        {
            let mut doc = pdfium.create_new_pdf().unwrap();
            doc.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
            doc.save_to_file(&tmp).unwrap();
        }

        let mut docs = HashMap::new();
        let mut next_id = 1;
        let info = open_doc(pdfium, &mut docs, &mut next_id, tmp.clone(), None).unwrap();
        let yellow = Rgba { r: 255, g: 220, b: 60, a: 128 };
        add_highlight(&mut docs, info.id, 0, 100.0, 700.0, 300.0, 720.0, yellow).unwrap();

        let rendered = render_page(&docs, info.id, 0, 595, false).unwrap();
        let b64 = rendered.data_url.split(',').nth(1).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let img = image::load_from_memory(&bytes).unwrap().to_rgb8();

        // A4 is 595x842pt rendered at 595px wide → 1px/pt. Sample inside the
        // highlight rect (y flipped: pdf y=710 → px y=842-710=132).
        let px = img.get_pixel(200, 132);
        let is_yellowish = px[0] > 230 && px[1] > 200 && px[2] < 200;
        assert!(is_yellowish, "highlight did not render: pixel at (200,132) = {px:?}");

        let _ = std::fs::remove_file(tmp);
    }
}

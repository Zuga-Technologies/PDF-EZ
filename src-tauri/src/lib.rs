mod ap;
mod license;
mod pdf_worker;

use std::path::PathBuf;
use std::sync::mpsc::channel;

use pdf_worker::{
    AnnotInfo, CompressOutcome, DocInfo, FieldInfo, ObjInfo, PdfRequest, PdfWorkerHandle,
    RenderedPage, Rgba, SearchHit,
};
use tauri::Manager;

fn app_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|e| e.to_string())
}

/// Round-trip a request to the PDF engine thread without blocking the async
/// runtime: the blocking recv happens on a blocking-pool thread.
async fn engine_call<T: Send + 'static>(
    app: tauri::AppHandle,
    make: impl FnOnce(std::sync::mpsc::Sender<Result<T, String>>) -> PdfRequest + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let (tx, rx) = channel();
        app.state::<PdfWorkerHandle>().send(make(tx))?;
        rx.recv().map_err(|_| "PDF engine dropped the request".to_string())?
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn open_document(
    app: tauri::AppHandle,
    path: String,
    password: Option<String>,
) -> Result<DocInfo, String> {
    engine_call(app, move |reply| PdfRequest::Open { path: PathBuf::from(path), password, reply }).await
}

#[tauri::command]
async fn close_document(app: tauri::AppHandle, id: u32) -> Result<(), String> {
    engine_call(app, move |reply| PdfRequest::Close { id, reply }).await
}

#[tauri::command]
async fn render_page(
    app: tauri::AppHandle,
    id: u32,
    index: u16,
    target_width: u32,
    jpeg: bool,
) -> Result<RenderedPage, String> {
    engine_call(app, move |reply| PdfRequest::Render { id, index, target_width, jpeg, reply }).await
}

#[tauri::command]
async fn get_page_text(app: tauri::AppHandle, id: u32, index: u16) -> Result<String, String> {
    engine_call(app, move |reply| PdfRequest::PageText { id, index, reply }).await
}

#[tauri::command]
async fn rotate_pages(
    app: tauri::AppHandle,
    id: u32,
    indices: Vec<u16>,
    quarter_turns: i8,
) -> Result<DocInfo, String> {
    engine_call(app, move |reply| PdfRequest::RotatePages { id, indices, quarter_turns, reply })
        .await
}

#[tauri::command]
async fn delete_pages(app: tauri::AppHandle, id: u32, indices: Vec<u16>) -> Result<DocInfo, String> {
    engine_call(app, move |reply| PdfRequest::DeletePages { id, indices, reply }).await
}

#[tauri::command]
async fn reorder_pages(app: tauri::AppHandle, id: u32, order: Vec<u16>) -> Result<DocInfo, String> {
    engine_call(app, move |reply| PdfRequest::ReorderPages { id, order, reply }).await
}

#[tauri::command]
async fn save_document(
    app: tauri::AppHandle,
    id: u32,
    path: Option<String>,
) -> Result<DocInfo, String> {
    engine_call(app, move |reply| PdfRequest::Save { id, path: path.map(PathBuf::from), reply }).await
}

#[tauri::command]
async fn export_pages(
    app: tauri::AppHandle,
    id: u32,
    dir: String,
    jpeg: bool,
    dpi: u16,
) -> Result<u16, String> {
    engine_call(app, move |reply| PdfRequest::ExportPages { id, dir: PathBuf::from(dir), jpeg, dpi, reply })
        .await
}

#[tauri::command]
async fn export_text(app: tauri::AppHandle, id: u32, path: String) -> Result<u16, String> {
    engine_call(app, move |reply| PdfRequest::ExportText { id, path: PathBuf::from(path), reply })
        .await
}

#[tauri::command]
async fn images_to_pdf(app: tauri::AppHandle, paths: Vec<String>, out: String) -> Result<(), String> {
    engine_call(app, move |reply| PdfRequest::ImagesToPdf {
        paths: paths.into_iter().map(PathBuf::from).collect(),
        out: PathBuf::from(out),
        reply,
    })
    .await
}

#[tauri::command]
async fn add_watermark(
    app: tauri::AppHandle,
    id: u32,
    text: String,
    size: f32,
    opacity: u8,
    diagonal: bool,
) -> Result<DocInfo, String> {
    engine_call(app, move |reply| PdfRequest::AddWatermark { id, text, size, opacity, diagonal, reply })
        .await
}

#[tauri::command]
async fn add_header_footer(
    app: tauri::AppHandle,
    id: u32,
    top: bool,
    align: u8,
    template: String,
) -> Result<DocInfo, String> {
    engine_call(app, move |reply| PdfRequest::AddHeaderFooter { id, top, align, template, reply })
        .await
}

#[tauri::command]
async fn set_document_password(
    app: tauri::AppHandle,
    id: u32,
    password: Option<String>,
) -> Result<DocInfo, String> {
    engine_call(app, move |reply| PdfRequest::SetDocumentPassword { id, password, reply }).await
}

#[tauri::command]
async fn list_page_objects(
    app: tauri::AppHandle,
    id: u32,
    page: u16,
) -> Result<Vec<ObjInfo>, String> {
    engine_call(app, move |reply| PdfRequest::ListPageObjects { id, page, reply }).await
}

#[tauri::command]
async fn set_text_object(
    app: tauri::AppHandle,
    id: u32,
    page: u16,
    index: u16,
    text: String,
) -> Result<Vec<ObjInfo>, String> {
    engine_call(app, move |reply| PdfRequest::SetTextObject { id, page, index, text, reply }).await
}

#[tauri::command]
async fn delete_page_object(
    app: tauri::AppHandle,
    id: u32,
    page: u16,
    index: u16,
) -> Result<Vec<ObjInfo>, String> {
    engine_call(app, move |reply| PdfRequest::DeletePageObject { id, page, index, reply }).await
}

#[tauri::command]
async fn list_form_fields(
    app: tauri::AppHandle,
    id: u32,
    page: u16,
) -> Result<Vec<FieldInfo>, String> {
    engine_call(app, move |reply| PdfRequest::ListFormFields { id, page, reply }).await
}

#[tauri::command]
async fn set_form_field(
    app: tauri::AppHandle,
    id: u32,
    page: u16,
    index: u16,
    value: String,
) -> Result<Vec<FieldInfo>, String> {
    engine_call(app, move |reply| PdfRequest::SetFormField { id, page, index, value, reply }).await
}

#[tauri::command]
async fn search_document(
    app: tauri::AppHandle,
    id: u32,
    query: String,
    match_case: bool,
) -> Result<Vec<SearchHit>, String> {
    engine_call(app, move |reply| PdfRequest::SearchDocument { id, query, match_case, reply })
        .await
}

#[tauri::command]
async fn compress_document(app: tauri::AppHandle, id: u32) -> Result<CompressOutcome, String> {
    engine_call(app, move |reply| PdfRequest::Compress { id, reply }).await
}

#[tauri::command]
async fn merge_append(
    app: tauri::AppHandle,
    id: u32,
    paths: Vec<String>,
) -> Result<DocInfo, String> {
    engine_call(app, move |reply| PdfRequest::MergeAppend {
        id,
        paths: paths.into_iter().map(PathBuf::from).collect(),
        reply,
    })
    .await
}

#[tauri::command]
async fn list_annotations(
    app: tauri::AppHandle,
    id: u32,
    page: u16,
) -> Result<Vec<AnnotInfo>, String> {
    engine_call(app, move |reply| PdfRequest::ListAnnotations { id, page, reply }).await
}

#[tauri::command]
async fn add_highlight(
    app: tauri::AppHandle,
    id: u32,
    page: u16,
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
    color: Rgba,
) -> Result<Vec<AnnotInfo>, String> {
    engine_call(app, move |reply| PdfRequest::AddHighlight {
        id, page, left, bottom, right, top, color, reply,
    })
    .await
}

#[tauri::command]
async fn add_note(
    app: tauri::AppHandle,
    id: u32,
    page: u16,
    x: f32,
    y: f32,
    contents: String,
) -> Result<Vec<AnnotInfo>, String> {
    engine_call(app, move |reply| PdfRequest::AddNote { id, page, x, y, contents, reply }).await
}

#[tauri::command]
async fn add_ink(
    app: tauri::AppHandle,
    id: u32,
    page: u16,
    strokes: Vec<Vec<(f32, f32)>>,
    color: Rgba,
    width: f32,
) -> Result<Vec<AnnotInfo>, String> {
    engine_call(app, move |reply| PdfRequest::AddInk { id, page, strokes, color, width, reply })
        .await
}

#[tauri::command]
async fn delete_annotation(
    app: tauri::AppHandle,
    id: u32,
    page: u16,
    index: u16,
) -> Result<Vec<AnnotInfo>, String> {
    engine_call(app, move |reply| PdfRequest::DeleteAnnotation { id, page, index, reply }).await
}

#[tauri::command]
fn check_license(app: tauri::AppHandle) -> Result<license::LicenseStatus, String> {
    Ok(license::check(&app_data_dir(&app)?))
}

#[tauri::command]
fn activate_license_file(
    app: tauri::AppHandle,
    lease_path: String,
) -> Result<license::LicenseStatus, String> {
    license::activate_from_file(&app_data_dir(&app)?, PathBuf::from(lease_path).as_path())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let resource_dir = app.path().resource_dir().ok();
            app.manage(pdf_worker::spawn(resource_dir));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            open_document,
            close_document,
            render_page,
            get_page_text,
            rotate_pages,
            delete_pages,
            reorder_pages,
            save_document,
            set_document_password,
            add_watermark,
            add_header_footer,
            export_pages,
            export_text,
            images_to_pdf,
            compress_document,
            search_document,
            list_page_objects,
            set_text_object,
            delete_page_object,
            list_form_fields,
            set_form_field,
            merge_append,
            list_annotations,
            add_highlight,
            add_note,
            add_ink,
            delete_annotation,
            check_license,
            activate_license_file
        ])
        .run(tauri::generate_context!())
        .expect("error while running ZugaPDF");
}

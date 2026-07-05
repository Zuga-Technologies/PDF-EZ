mod license;
mod pdf_worker;

use std::path::PathBuf;
use std::sync::mpsc::channel;

use pdf_worker::{DocInfo, PdfRequest, PdfWorkerHandle, RenderedPage};
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
) -> Result<RenderedPage, String> {
    engine_call(app, move |reply| PdfRequest::Render { id, index, target_width, reply }).await
}

#[tauri::command]
async fn get_page_text(app: tauri::AppHandle, id: u32, index: u16) -> Result<String, String> {
    engine_call(app, move |reply| PdfRequest::PageText { id, index, reply }).await
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
            check_license,
            activate_license_file
        ])
        .run(tauri::generate_context!())
        .expect("error while running ZugaPDF");
}

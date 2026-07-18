//! PDFium glue for the shell extension. PDFium is not thread-safe and the
//! shell may call from several apartment threads in one surrogate process, so
//! every render takes a global lock and binds/loads per call (LoadLibrary is
//! refcounted — cheap after the first call).

use std::path::PathBuf;
use std::sync::Mutex;

use pdfium_render::prelude::*;

static ENGINE_LOCK: Mutex<()> = Mutex::new(());

/// Directory containing this module (the DLL for shell use, the exe for the
/// test bins). pdfium.dll is expected to sit next to it.
pub fn module_dir() -> PathBuf {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };

    let mut module = HMODULE::default();
    unsafe {
        let _ = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PWSTR(module_dir as *mut u16),
            &mut module,
        );
        let mut buf = [0u16; 1024];
        let len = GetModuleFileNameW(module, &mut buf) as usize;
        let path = PathBuf::from(String::from_utf16_lossy(&buf[..len]));
        path.parent().map(|p| p.to_path_buf()).unwrap_or_default()
    }
}

fn bind_pdfium() -> Result<Pdfium, String> {
    let candidates = [
        module_dir().join("pdfium.dll"),
        module_dir().join("pdfium").join("bin").join("pdfium.dll"),
    ];
    for c in &candidates {
        if let Ok(bindings) = Pdfium::bind_to_library(c) {
            return Ok(Pdfium::new(bindings));
        }
    }
    Err(format!("pdfium.dll not found near {}", module_dir().display()))
}

/// Render page `index` of `bytes` scaled to fit inside `max_w` x `max_h`.
/// Returns (width, height, BGRA bytes with alpha forced opaque over white).
pub fn render_page_fit(
    bytes: &[u8],
    index: u16,
    max_w: u32,
    max_h: u32,
) -> Result<(u32, u32, Vec<u8>), String> {
    let _guard = ENGINE_LOCK.lock().map_err(|_| "engine lock poisoned".to_string())?;

    let pdfium = bind_pdfium()?;
    let doc = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|e| format!("load failed: {e}"))?;
    let page = doc
        .pages()
        .get(index.into())
        .map_err(|e| format!("page {index} not found: {e}"))?;

    let (pw, ph) = (page.width().value.max(1.0), page.height().value.max(1.0));
    let scale = (max_w.max(16) as f32 / pw).min(max_h.max(16) as f32 / ph);
    let px_w = ((pw * scale).round() as i32).clamp(16, 4096);

    let config = PdfRenderConfig::new()
        .set_target_width(px_w)
        .render_form_data(true)
        .render_annotations(true);
    let bitmap = page
        .render_with_config(&config)
        .map_err(|e| format!("render failed: {e}"))?;
    let image = bitmap
        .as_image()
        .map_err(|e| format!("bitmap conversion failed: {e}"))?;
    let rgba = image.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());

    // Composite over white and swizzle RGBA -> BGRA for GDI.
    let mut out = rgba.into_raw();
    for px in out.chunks_exact_mut(4) {
        let a = px[3] as u16;
        let blend = |c: u8| ((c as u16 * a + 255 * (255 - a)) / 255) as u8;
        let (r, g, b) = (blend(px[0]), blend(px[1]), blend(px[2]));
        px[0] = b;
        px[1] = g;
        px[2] = r;
        px[3] = 255;
    }
    Ok((w, h, out))
}

/// Page count of the document (for the test bins).
pub fn page_count(bytes: &[u8]) -> Result<i32, String> {
    let _guard = ENGINE_LOCK.lock().map_err(|_| "engine lock poisoned".to_string())?;
    let pdfium = bind_pdfium()?;
    let doc = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|e| format!("load failed: {e}"))?;
    Ok(doc.pages().len())
}

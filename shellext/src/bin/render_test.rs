//! Render every page of a PDF exactly the way the app does — once through the
//! PNG path (thumbnails) and once through the JPEG path (main pages) — so a
//! page that looks wrong in the viewer can be blamed on the engine or the
//! encode step. Usage: render_test <pdf> <outdir> [width]

use pdfium_render::prelude::*;

fn main() {
    let mut args = std::env::args().skip(1);
    let pdf = args.next().expect("usage: render_test <pdf> <outdir> [width]");
    let outdir = std::path::PathBuf::from(args.next().expect("outdir required"));
    let width: i32 = args.next().and_then(|w| w.parse().ok()).unwrap_or(1000);
    std::fs::create_dir_all(&outdir).expect("create outdir");

    let bytes = std::fs::read(&pdf).expect("read pdf");
    let pdfium = Pdfium::new(
        Pdfium::bind_to_library(pdez_shell::engine::module_dir().join("pdfium.dll"))
            .or_else(|_| Pdfium::bind_to_system_library())
            .expect("bind pdfium"),
    );
    let doc = pdfium.load_pdf_from_byte_slice(&bytes, None).expect("load pdf");
    let n = doc.pages().len();
    println!("{n} pages");

    for i in 0..n {
        let page = doc.pages().get(i).expect("page");
        let config = PdfRenderConfig::new()
            .set_target_width(width)
            .render_form_data(true)
            .render_annotations(true);
        let bitmap = page.render_with_config(&config).expect("render");
        let image = bitmap.as_image().expect("as_image");

        // PNG path (what thumbnails use)
        image
            .save_with_format(outdir.join(format!("p{i:03}.png")), image::ImageFormat::Png)
            .expect("save png");
        // JPEG path (what main pages use): to_rgb8 drops alpha, like the app
        image::DynamicImage::ImageRgb8(image.to_rgb8())
            .save_with_format(outdir.join(format!("p{i:03}.jpg")), image::ImageFormat::Jpeg)
            .expect("save jpg");
        println!("page {i}: {}x{}", image.width(), image.height());
    }
}

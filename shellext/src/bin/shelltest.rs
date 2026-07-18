//! End-to-end shell extension test host.
//!   shelltest thumb <pdf> <out.png>    — asks the REAL shell thumbnail
//!       pipeline (IShellItemImageFactory), exercising registry + surrogate.
//!   shelltest preview <pdf> <out.png>  — hosts our preview handler in a
//!       window the way prevhost would, then captures the result.

use std::ffi::c_void;

use windows::core::{w, Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetDC, GetDIBits, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
    HGDIOBJ,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IStream, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    STGM_READ,
};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{
    IPreviewHandler, IShellItemImageFactory, SHCreateItemFromParsingName,
    SHCreateStreamOnFileEx, SIIGBF_BIGGERSIZEOK, SIIGBF_THUMBNAILONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

fn hbitmap_to_png(hbmp: HBITMAP, out: &str) {
    unsafe {
        let hdc = GetDC(HWND::default());
        let mut header = BITMAPINFOHEADER::default();
        header.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        let mut bmi = BITMAPINFO { bmiHeader: header, ..Default::default() };
        // First call fills the header.
        GetDIBits(hdc, hbmp, 0, 0, None, &mut bmi, DIB_RGB_COLORS);
        let w = bmi.bmiHeader.biWidth;
        let h = bmi.bmiHeader.biHeight.abs();
        bmi.bmiHeader.biHeight = -h; // top-down
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;
        bmi.bmiHeader.biSizeImage = 0;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        GetDIBits(
            hdc,
            hbmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(HWND::default(), hdc);
        // BGRA -> RGBA
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
        }
        let img = image::RgbaImage::from_raw(w as u32, h as u32, buf).expect("raw image");
        img.save(out).expect("save png");
        println!("wrote {out} ({w}x{h})");
    }
}

fn pump_for(ms: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    unsafe {
        while std::time::Instant::now() < deadline {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("usage: shelltest <thumb|preview> <pdf> <out.png>");
    let pdf = args.next().expect("pdf path required");
    let out = args.next().expect("out.png required");

    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().expect("CoInitializeEx");
    }

    match mode.as_str() {
        "thumb" => unsafe {
            let item: IShellItemImageFactory =
                SHCreateItemFromParsingName(PCWSTR(HSTRING::from(pdf.as_str()).as_ptr()), None)
                    .expect("SHCreateItemFromParsingName");
            let hbmp = item
                .GetImage(SIZE { cx: 256, cy: 256 }, SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK)
                .expect("GetImage — thumbnail pipeline rejected us");
            hbitmap_to_png(hbmp, &out);
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(HGDIOBJ(hbmp.0));
        },
        "preview" => unsafe {
            let host = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("PD-EZ preview host"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                720,
                940,
                HWND::default(),
                None,
                None,
                None,
            )
            .expect("host window");

            let handler: IPreviewHandler =
                CoCreateInstance(&pdez_shell::CLSID_PREVIEW, None, CLSCTX_INPROC_SERVER)
                    .expect("CoCreateInstance preview handler (is it registered?)");
            let stream: IStream =
                SHCreateStreamOnFileEx(PCWSTR(HSTRING::from(pdf.as_str()).as_ptr()), STGM_READ.0, 0, false, None)
                    .expect("open stream");
            let init: IInitializeWithStream = handler.cast().expect("IInitializeWithStream");
            init.Initialize(&stream, STGM_READ.0).expect("Initialize");

            let rect = RECT { left: 0, top: 0, right: 700, bottom: 900 };
            handler.SetWindow(host, &rect).expect("SetWindow");
            handler.DoPreview().expect("DoPreview");
            pump_for(2500);

            // Capture the host window contents.
            let mut wrect = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(host, &mut wrect);
            let hdc_screen = GetDC(host);
            let (cw, ch) = (wrect.right - wrect.left, wrect.bottom - wrect.top);
            let hdc_mem = windows::Win32::Graphics::Gdi::CreateCompatibleDC(hdc_screen);
            let hbmp = windows::Win32::Graphics::Gdi::CreateCompatibleBitmap(hdc_screen, cw, ch);
            let old = windows::Win32::Graphics::Gdi::SelectObject(hdc_mem, HGDIOBJ(hbmp.0));
            let _ = windows::Win32::Graphics::Gdi::BitBlt(
                hdc_mem,
                0,
                0,
                cw,
                ch,
                hdc_screen,
                0,
                0,
                windows::Win32::Graphics::Gdi::SRCCOPY,
            );
            windows::Win32::Graphics::Gdi::SelectObject(hdc_mem, old);
            hbitmap_to_png(hbmp, &out);
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(HGDIOBJ(hbmp.0));
            let _ = windows::Win32::Graphics::Gdi::DeleteDC(hdc_mem);
            ReleaseDC(host, hdc_screen);
            handler.Unload().ok();
            let _ = SendCleanup(host);
        },
        other => panic!("unknown mode {other}"),
    }
}

#[allow(non_snake_case)]
fn SendCleanup(hwnd: HWND) -> windows::core::Result<()> {
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            hwnd,
            0x0010, // WM_CLOSE
            WPARAM(0),
            LPARAM(0),
        );
    }
    Ok(())
}

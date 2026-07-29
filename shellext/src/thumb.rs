//! IThumbnailProvider: Explorer asks for a square thumbnail of page 1.
//! Runs out-of-proc in the COM surrogate (dllhost), initialized with a stream.

use std::cell::RefCell;
use std::ffi::c_void;

use windows::core::{implement, Result};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
};
use windows::Win32::System::Com::IStream;
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{
    IThumbnailProvider, IThumbnailProvider_Impl, WTSAT_RGB, WTS_ALPHATYPE,
};

use crate::engine;

pub fn read_stream(stream: &IStream) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        let mut read = 0u32;
        let hr = unsafe {
            stream.Read(chunk.as_mut_ptr() as *mut c_void, chunk.len() as u32, Some(&mut read))
        };
        if read > 0 {
            data.extend_from_slice(&chunk[..read as usize]);
        }
        if hr.is_err() || read == 0 {
            break;
        }
    }
    if data.is_empty() {
        return Err(E_FAIL.into());
    }
    Ok(data)
}

pub fn bgra_to_hbitmap(w: u32, h: u32, bgra: &[u8]) -> Result<HBITMAP> {
    let mut header = BITMAPINFOHEADER::default();
    header.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    header.biWidth = w as i32;
    header.biHeight = -(h as i32); // top-down
    header.biPlanes = 1;
    header.biBitCount = 32;
    header.biCompression = BI_RGB.0;
    let bmi = BITMAPINFO { bmiHeader: header, ..Default::default() };

    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbmp = unsafe { CreateDIBSection(HDC::default(), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
        .map_err(|_| windows::core::Error::from(E_FAIL))?;
    if bits.is_null() {
        return Err(E_FAIL.into());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, (w * h * 4) as usize);
    }
    Ok(hbmp)
}

#[implement(IInitializeWithStream, IThumbnailProvider)]
pub struct ThumbProvider {
    data: RefCell<Vec<u8>>,
}

impl ThumbProvider {
    pub fn new() -> Self {
        Self { data: RefCell::new(Vec::new()) }
    }
}

impl IInitializeWithStream_Impl for ThumbProvider_Impl {
    fn Initialize(&self, pstream: Option<&IStream>, _grfmode: u32) -> Result<()> {
        let stream = pstream.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        *self.data.borrow_mut() = read_stream(stream)?;
        Ok(())
    }
}

impl IThumbnailProvider_Impl for ThumbProvider_Impl {
    fn GetThumbnail(
        &self,
        cx: u32,
        phbmp: *mut HBITMAP,
        pdwalpha: *mut WTS_ALPHATYPE,
    ) -> Result<()> {
        let data = self.data.borrow();
        let (w, h, bgra) = if crate::md_engine::looks_like_pdf(&data) {
            engine::render_page_fit(&data, 0, cx, cx)
        } else {
            crate::md_engine::render_markdown_fit(&data, cx, cx)
        }
        .map_err(|_| windows::core::Error::from(E_FAIL))?;
        let hbmp = bgra_to_hbitmap(w, h, &bgra)?;
        unsafe {
            *phbmp = hbmp;
            *pdwalpha = WTSAT_RGB;
        }
        Ok(())
    }
}

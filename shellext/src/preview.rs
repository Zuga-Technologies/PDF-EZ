//! IPreviewHandler: the Explorer preview pane. Hosted by prevhost.exe, which
//! hands us a parent HWND + rect; we show page 1 fitted to the pane in a
//! STATIC child control (SS_BITMAP does the painting for us).

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use windows::core::{implement, w, IUnknown, Interface, Result};
use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP};
use windows::Win32::System::Com::IStream;
use windows::Win32::System::Ole::{IObjectWithSite, IObjectWithSite_Impl, IOleWindow, IOleWindow_Impl};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{IPreviewHandler, IPreviewHandler_Impl};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, MoveWindow, SendMessageW, WINDOW_EX_STYLE, WINDOW_STYLE,
    WS_CHILD, WS_VISIBLE,
};

// comctl32 STATIC control styles (not exposed by the windows crate features we use)
const SS_BITMAP: u32 = 0x0000_000E;
const SS_CENTERIMAGE: u32 = 0x0000_0200;

use crate::thumb::{bgra_to_hbitmap, read_stream};
use crate::engine;

const STM_SETIMAGE: u32 = 0x0172;
const IMAGE_BITMAP: usize = 0;

#[implement(IPreviewHandler, IInitializeWithStream, IObjectWithSite, IOleWindow)]
pub struct PreviewHandler {
    data: RefCell<Vec<u8>>,
    parent: Cell<HWND>,
    rect: Cell<RECT>,
    child: Cell<HWND>,
    hbmp: Cell<HBITMAP>,
    site: RefCell<Option<IUnknown>>,
}

impl PreviewHandler {
    pub fn new() -> Self {
        Self {
            data: RefCell::new(Vec::new()),
            parent: Cell::new(HWND::default()),
            rect: Cell::new(RECT::default()),
            child: Cell::new(HWND::default()),
            hbmp: Cell::new(HBITMAP::default()),
            site: RefCell::new(None),
        }
    }
}

impl PreviewHandler_Impl {
    fn swap_bitmap(&self, hbmp: HBITMAP) {
        let child = self.child.get();
        if !child.is_invalid() {
            unsafe {
                SendMessageW(child, STM_SETIMAGE, WPARAM(IMAGE_BITMAP), LPARAM(hbmp.0 as isize));
            }
        }
        let old = self.hbmp.replace(hbmp);
        if !old.is_invalid() {
            unsafe {
                let _ = DeleteObject(old);
            }
        }
    }

    fn render_into_child(&self) {
        let rect = self.rect.get();
        let (w, h) = ((rect.right - rect.left).max(32) as u32, (rect.bottom - rect.top).max(32) as u32);
        let data = self.data.borrow();
        if data.is_empty() {
            return;
        }
        let rendered = if crate::md_engine::looks_like_pdf(&data) {
            engine::render_page_fit(&data, 0, w, h)
        } else {
            crate::md_engine::render_markdown_fit(&data, w, h)
        };
        if let Ok((bw, bh, bgra)) = rendered {
            if let Ok(hbmp) = bgra_to_hbitmap(bw, bh, &bgra) {
                self.swap_bitmap(hbmp);
            }
        }
    }
}

impl IInitializeWithStream_Impl for PreviewHandler_Impl {
    fn Initialize(&self, pstream: Option<&IStream>, _grfmode: u32) -> Result<()> {
        let stream = pstream.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        *self.data.borrow_mut() = read_stream(stream)?;
        Ok(())
    }
}

impl IPreviewHandler_Impl for PreviewHandler_Impl {
    fn SetWindow(&self, hwnd: HWND, prc: *const RECT) -> Result<()> {
        self.parent.set(hwnd);
        if !prc.is_null() {
            self.rect.set(unsafe { *prc });
        }
        Ok(())
    }

    fn SetRect(&self, prc: *const RECT) -> Result<()> {
        if prc.is_null() {
            return Ok(());
        }
        let rect = unsafe { *prc };
        self.rect.set(rect);
        let child = self.child.get();
        if !child.is_invalid() {
            unsafe {
                let _ = MoveWindow(
                    child,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    true,
                );
            }
            self.render_into_child();
        }
        Ok(())
    }

    fn DoPreview(&self) -> Result<()> {
        let parent = self.parent.get();
        if parent.is_invalid() || self.data.borrow().is_empty() {
            return Err(E_FAIL.into());
        }
        if self.child.get().is_invalid() {
            let rect = self.rect.get();
            let child = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    None,
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SS_BITMAP | SS_CENTERIMAGE),
                    rect.left,
                    rect.top,
                    (rect.right - rect.left).max(1),
                    (rect.bottom - rect.top).max(1),
                    parent,
                    None,
                    None,
                    None,
                )
            }
            .map_err(|_| windows::core::Error::from(E_FAIL))?;
            self.child.set(child);
        }
        self.render_into_child();
        Ok(())
    }

    fn Unload(&self) -> Result<()> {
        let child = self.child.replace(HWND::default());
        if !child.is_invalid() {
            unsafe {
                let _ = DestroyWindow(child);
            }
        }
        let old = self.hbmp.replace(HBITMAP::default());
        if !old.is_invalid() {
            unsafe {
                let _ = DeleteObject(old);
            }
        }
        self.data.borrow_mut().clear();
        Ok(())
    }

    fn SetFocus(&self) -> Result<()> {
        let child = self.child.get();
        if !child.is_invalid() {
            unsafe {
                let _ = SetFocus(child);
            }
        }
        Ok(())
    }

    fn QueryFocus(&self) -> Result<HWND> {
        Ok(unsafe { GetFocus() })
    }

    fn TranslateAccelerator(
        &self,
        _pmsg: *const windows::Win32::UI::WindowsAndMessaging::MSG,
    ) -> Result<()> {
        Err(windows::Win32::Foundation::S_FALSE.into())
    }
}

impl IObjectWithSite_Impl for PreviewHandler_Impl {
    fn SetSite(&self, punksite: Option<&IUnknown>) -> Result<()> {
        *self.site.borrow_mut() = punksite.cloned();
        Ok(())
    }

    fn GetSite(&self, riid: *const windows::core::GUID, ppvsite: *mut *mut c_void) -> Result<()> {
        let site = self.site.borrow();
        match site.as_ref() {
            Some(s) => unsafe { s.query(riid, ppvsite).ok() },
            None => Err(E_FAIL.into()),
        }
    }
}

impl IOleWindow_Impl for PreviewHandler_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        let child = self.child.get();
        Ok(if child.is_invalid() { self.parent.get() } else { child })
    }

    fn ContextSensitiveHelp(&self, _fentermode: windows::Win32::Foundation::BOOL) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
}

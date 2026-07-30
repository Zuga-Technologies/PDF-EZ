//! pdez_shell.dll — PDF-EZ's Explorer integration (PDF + Markdown thumbnails
//! and preview pane). PDF pages render via the same PDFium build the app
//! ships; Markdown renders via a lightweight GDI text rasterizer (md_engine).
//! Both document types share the same two COM classes — `Initialize()`
//! content-sniffs the stream to decide which engine renders it. Registered
//! per-user by scripts/register-shellext.ps1.

pub mod engine;
pub mod md_engine;
pub mod preview;
pub mod thumb;

use std::ffi::c_void;

use windows::core::{implement, GUID, HRESULT, IUnknown, Interface};
use windows::Win32::Foundation::{BOOL, CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, S_FALSE};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};

use preview::PreviewHandler;
use thumb::ThumbProvider;

pub const CLSID_THUMB: GUID = GUID::from_u128(0xB7C0A2D4_8E11_4E5C_9F2A_01D6E4C3A9B1);
pub const CLSID_PREVIEW: GUID = GUID::from_u128(0xB7C0A2D4_8E11_4E5C_9F2A_01D6E4C3A9B2);

#[implement(IClassFactory)]
struct Factory {
    preview: bool,
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Option<&IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if punkouter.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let unknown: IUnknown = if self.preview {
            PreviewHandler::new().into()
        } else {
            ThumbProvider::new().into()
        };
        unsafe { unknown.query(riid, ppvobject).ok() }
    }

    fn LockServer(&self, _flock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

#[no_mangle]
extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    let clsid = unsafe { *rclsid };
    let factory: IClassFactory = if clsid == CLSID_THUMB {
        Factory { preview: false }.into()
    } else if clsid == CLSID_PREVIEW {
        Factory { preview: true }.into()
    } else {
        return CLASS_E_CLASSNOTAVAILABLE;
    };
    unsafe { factory.query(riid, ppv) }
}

#[no_mangle]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    // Surrogate processes are short-lived; never claim unloadable mid-render.
    S_FALSE
}

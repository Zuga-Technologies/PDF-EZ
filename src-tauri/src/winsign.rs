//! Windows Certificate Store signing via CNG.
//!
//! The differentiator this enables: sign with a certificate that already
//! lives in Windows (CurrentUser\MY), including TPM-backed and smart-card
//! keys. The private key is never exported — the only operation we ever
//! perform is `NCryptSignHash` on an acquired handle, so a TPM/card key
//! physically cannot leave its hardware. Windows itself shows any PIN or
//! consent UI; we never see secrets.
//!
//! `CngSigner` implements the same RustCrypto traits (`Signer`, `Keypair`,
//! `DynSignatureAlgorithmIdentifier`) as `rsa::pkcs1v15::SigningKey`, so the
//! whole CMS/PAdES pipeline in `sign.rs` is reused unchanged.

use serde::Serialize;
use sha2::{Digest, Sha256};
use windows::core::PCWSTR;
use windows::Win32::Security::Cryptography::{
    CertCloseStore, CertEnumCertificatesInStore, CertFindCertificateInStore,
    CertFreeCertificateChain, CertFreeCertificateContext, CertGetCertificateChain,
    CertGetCertificateContextProperty, CertGetNameStringW, CertOpenStore,
    CryptAcquireCertificatePrivateKey, NCryptFreeObject, NCryptSignHash,
    BCRYPT_PKCS1_PADDING_INFO, BCRYPT_SHA256_ALGORITHM, CERT_CHAIN_CONTEXT, CERT_CHAIN_PARA,
    CERT_CONTEXT, CERT_FIND_HASH, CERT_HASH_PROP_ID, CERT_KEY_PROV_INFO_PROP_ID,
    CERT_NAME_ISSUER_FLAG, CERT_NAME_SIMPLE_DISPLAY_TYPE, CERT_OPEN_STORE_FLAGS,
    CERT_QUERY_ENCODING_TYPE, CERT_STORE_PROV_SYSTEM_W, CERT_SYSTEM_STORE_CURRENT_USER_ID,
    CERT_SYSTEM_STORE_LOCATION_SHIFT, CRYPT_ACQUIRE_CACHE_FLAG, CRYPT_ACQUIRE_FLAGS,
    CRYPT_ACQUIRE_ONLY_NCRYPT_KEY_FLAG, CRYPT_INTEGER_BLOB, CRYPT_KEY_PROV_INFO,
    HCERTCHAINENGINE, HCERTSTORE, HCRYPTPROV_LEGACY, HCRYPTPROV_OR_NCRYPT_KEY_HANDLE,
    NCRYPT_KEY_HANDLE, NCRYPT_PAD_PKCS1_FLAG, X509_ASN_ENCODING,
};

/// What the UI needs to render one store certificate.
#[derive(Serialize, Clone)]
pub struct StoreCert {
    pub thumbprint: String, // SHA-1, lowercase hex — the stable selector
    pub subject: String,
    pub issuer: String,
    pub not_after: String, // ISO date
    pub algorithm: String, // "RSA" | "ECDSA" | other OID
    pub provider: String,  // KSP name, e.g. "Microsoft Platform Crypto Provider"
    pub tpm_backed: bool,
    pub rsa: bool, // only RSA certs are signable this pass
}

const OID_RSA: &str = "1.2.840.113549.1.1.1";
const OID_EC: &str = "1.2.840.10045.2.1";
const TPM_PROVIDER: &str = "Microsoft Platform Crypto Provider";

struct Store(HCERTSTORE);

impl Store {
    fn open_my() -> Result<Self, String> {
        // CurrentUser\MY, read-only.
        let handle = unsafe {
            CertOpenStore(
                CERT_STORE_PROV_SYSTEM_W,
                CERT_QUERY_ENCODING_TYPE(0),
                HCRYPTPROV_LEGACY::default(),
                CERT_OPEN_STORE_FLAGS(
                    (CERT_SYSTEM_STORE_CURRENT_USER_ID << CERT_SYSTEM_STORE_LOCATION_SHIFT)
                        | 0x00008000, // CERT_STORE_READONLY_FLAG
                ),
                Some(windows::core::w!("MY").as_ptr() as *const core::ffi::c_void),
            )
        }
        .map_err(|e| format!("Could not open the Windows certificate store: {e}"))?;
        Ok(Self(handle))
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        unsafe {
            let _ = CertCloseStore(self.0, 0);
        }
    }
}

fn name_string(ctx: *const CERT_CONTEXT, flags: u32) -> String {
    unsafe {
        let len = CertGetNameStringW(ctx, CERT_NAME_SIMPLE_DISPLAY_TYPE, flags, None, None);
        if len <= 1 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize];
        CertGetNameStringW(ctx, CERT_NAME_SIMPLE_DISPLAY_TYPE, flags, None, Some(&mut buf));
        String::from_utf16_lossy(&buf[..len as usize - 1])
    }
}

fn context_property(ctx: *const CERT_CONTEXT, prop: u32) -> Option<Vec<u8>> {
    unsafe {
        let mut len = 0u32;
        if CertGetCertificateContextProperty(ctx, prop, None, &mut len).is_err() {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        CertGetCertificateContextProperty(
            ctx,
            prop,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut len,
        )
        .ok()?;
        buf.truncate(len as usize);
        Some(buf)
    }
}

fn provider_name(ctx: *const CERT_CONTEXT) -> String {
    let Some(blob) = context_property(ctx, CERT_KEY_PROV_INFO_PROP_ID) else {
        return String::new();
    };
    unsafe {
        let info = &*(blob.as_ptr() as *const CRYPT_KEY_PROV_INFO);
        if info.pwszProvName.is_null() {
            return String::new();
        }
        info.pwszProvName.to_string().unwrap_or_default()
    }
}

/// FILETIME (100ns ticks since 1601) → ISO date.
fn filetime_to_iso(ft: windows::Win32::Foundation::FILETIME) -> String {
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600; // 1601 → 1970
    let unix = (ticks / 10_000_000).saturating_sub(EPOCH_DIFF_SECS) as i64;
    let days = unix.div_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

fn spki_algorithm(ctx: *const CERT_CONTEXT) -> String {
    unsafe {
        let info = &*(*ctx).pCertInfo;
        let oid_ptr = info.SubjectPublicKeyInfo.Algorithm.pszObjId;
        if oid_ptr.is_null() {
            return String::new();
        }
        oid_ptr.to_string().unwrap_or_default()
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn filetime_unix(ft: windows::Win32::Foundation::FILETIME) -> i64 {
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    (ticks / 10_000_000) as i64 - 11_644_473_600
}

/// Enumerate CurrentUser\MY: certificates that have a private key and are not
/// expired. RSA-ness and provider are reported so the UI can badge/grey.
pub fn list_certs() -> Result<Vec<StoreCert>, String> {
    let store = Store::open_my()?;
    let mut out = Vec::new();
    let mut ctx: *const CERT_CONTEXT = std::ptr::null();
    loop {
        ctx = unsafe { CertEnumCertificatesInStore(store.0, Some(ctx)) };
        if ctx.is_null() {
            break;
        }
        // Must have an associated private key (any kind).
        if context_property(ctx, CERT_KEY_PROV_INFO_PROP_ID).is_none() {
            continue;
        }
        let not_after = unsafe { (*(*ctx).pCertInfo).NotAfter };
        if filetime_unix(not_after) < now_unix() {
            continue; // expired
        }
        let thumb = context_property(ctx, CERT_HASH_PROP_ID)
            .map(|h| h.iter().map(|b| format!("{b:02x}")).collect::<String>())
            .unwrap_or_default();
        if thumb.is_empty() {
            continue;
        }
        let algorithm_oid = spki_algorithm(ctx);
        let (algorithm, rsa) = match algorithm_oid.as_str() {
            OID_RSA => ("RSA".to_string(), true),
            OID_EC => ("ECDSA".to_string(), false),
            other => (other.to_string(), false),
        };
        let provider = provider_name(ctx);
        out.push(StoreCert {
            thumbprint: thumb,
            subject: name_string(ctx, 0),
            issuer: name_string(ctx, CERT_NAME_ISSUER_FLAG),
            not_after: filetime_to_iso(not_after),
            tpm_backed: provider == TPM_PROVIDER,
            provider,
            algorithm,
            rsa,
        });
    }
    out.sort_by(|a, b| b.not_after.cmp(&a.not_after));
    Ok(out)
}

/// A signing handle acquired from the store. Owns the cert context + NCrypt
/// key handle; frees both on drop.
pub struct CngSigner {
    key: NCRYPT_KEY_HANDLE,
    free_key: bool,
    ctx: *const CERT_CONTEXT,
    _store: Store, // keep the store open while the context lives
    /// DER of the leaf followed by the rest of the chain (excluding roots is
    /// fine; validators accept either).
    pub chain_der: Vec<Vec<u8>>,
    verifying_key: rsa::pkcs1v15::VerifyingKey<Sha256>,
}

// The handle is used from the engine's blocking pool; NCrypt handles are
// process-global and safe to move between threads.
unsafe impl Send for CngSigner {}

impl CngSigner {
    pub fn acquire(thumbprint: &str) -> Result<Self, String> {
        let hash: Vec<u8> = (0..thumbprint.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&thumbprint[i..i + 2], 16))
            .collect::<Result<_, _>>()
            .map_err(|_| "Bad certificate thumbprint".to_string())?;
        let store = Store::open_my()?;
        let blob = CRYPT_INTEGER_BLOB { cbData: hash.len() as u32, pbData: hash.as_ptr() as *mut u8 };
        let ctx = unsafe {
            CertFindCertificateInStore(
                store.0,
                X509_ASN_ENCODING,
                0,
                CERT_FIND_HASH,
                Some(&blob as *const _ as *const core::ffi::c_void),
                None,
            )
        };
        if ctx.is_null() {
            return Err("That certificate is no longer in the Windows store".to_string());
        }

        // Private key handle — CNG only, allow Windows to show PIN UI.
        let mut raw_key = HCRYPTPROV_OR_NCRYPT_KEY_HANDLE::default();
        let mut key_spec = windows::Win32::Security::Cryptography::CERT_KEY_SPEC(0);
        let mut caller_free = windows::Win32::Foundation::BOOL(0);
        unsafe {
            CryptAcquireCertificatePrivateKey(
                ctx,
                CRYPT_ACQUIRE_FLAGS(
                    CRYPT_ACQUIRE_ONLY_NCRYPT_KEY_FLAG.0 | CRYPT_ACQUIRE_CACHE_FLAG.0,
                ),
                None,
                &mut raw_key,
                Some(&mut key_spec),
                Some(&mut caller_free),
            )
        }
        .map_err(|_| {
            "Could not access this certificate's private key (was it imported without one?)"
                .to_string()
        })?;
        // ONLY_NCRYPT guarantees this handle is an NCrypt key handle.
        let key = NCRYPT_KEY_HANDLE(raw_key.0 as _);

        // Leaf public key → RustCrypto verifying key (also validates RSA-ness).
        let leaf_der = unsafe {
            std::slice::from_raw_parts((*ctx).pbCertEncoded, (*ctx).cbCertEncoded as usize).to_vec()
        };
        let verifying_key = verifying_key_from_der(&leaf_der)?;

        // Chain for embedding (leaf first). Best effort — leaf-only on failure.
        let chain_der = chain_for(ctx).unwrap_or_else(|| vec![leaf_der.clone()]);

        Ok(Self {
            key,
            free_key: caller_free.as_bool(),
            ctx,
            _store: store,
            chain_der,
            verifying_key,
        })
    }

    pub fn leaf_der(&self) -> &[u8] {
        &self.chain_der[0]
    }

    fn sign_digest(&self, digest: &[u8]) -> Result<Vec<u8>, String> {
        let padding = BCRYPT_PKCS1_PADDING_INFO { pszAlgId: PCWSTR(BCRYPT_SHA256_ALGORITHM.as_ptr()) };
        unsafe {
            let mut needed = 0u32;
            NCryptSignHash(
                self.key,
                Some(&padding as *const _ as *const core::ffi::c_void),
                digest,
                None,
                &mut needed,
                NCRYPT_PAD_PKCS1_FLAG,
            )
            .map_err(|e| format!("Windows refused to size the signature: {e}"))?;
            let mut sig = vec![0u8; needed as usize];
            let mut written = 0u32;
            NCryptSignHash(
                self.key,
                Some(&padding as *const _ as *const core::ffi::c_void),
                digest,
                Some(&mut sig),
                &mut written,
                NCRYPT_PAD_PKCS1_FLAG,
            )
            .map_err(|e| format!("Windows refused to sign (PIN cancelled?): {e}"))?;
            sig.truncate(written as usize);
            Ok(sig)
        }
    }
}

impl Drop for CngSigner {
    fn drop(&mut self) {
        unsafe {
            if self.free_key {
                let _ = NCryptFreeObject(windows::Win32::Security::Cryptography::NCRYPT_HANDLE(
                    self.key.0,
                ));
            }
            if !self.ctx.is_null() {
                let _ = CertFreeCertificateContext(Some(self.ctx));
            }
        }
    }
}

fn verifying_key_from_der(leaf_der: &[u8]) -> Result<rsa::pkcs1v15::VerifyingKey<Sha256>, String> {
    use der::Decode;
    use rsa::pkcs1::DecodeRsaPublicKey;
    let cert = x509_cert::Certificate::from_der(leaf_der)
        .map_err(|e| format!("Certificate parse failed: {e}"))?;
    let spki = &cert.tbs_certificate.subject_public_key_info;
    if spki.algorithm.oid.to_string() != OID_RSA {
        return Err(
            "This certificate uses an ECDSA key — PDF-EZ signs with RSA store certificates for now"
                .to_string(),
        );
    }
    let key_bytes = spki
        .subject_public_key
        .as_bytes()
        .ok_or("Certificate public key is not byte-aligned")?;
    let pubkey = rsa::RsaPublicKey::from_pkcs1_der(key_bytes)
        .map_err(|e| format!("RSA public key parse failed: {e}"))?;
    Ok(rsa::pkcs1v15::VerifyingKey::<Sha256>::new(pubkey))
}

/// Full chain DER via the Windows chain engine, leaf first.
fn chain_for(ctx: *const CERT_CONTEXT) -> Option<Vec<Vec<u8>>> {
    unsafe {
        let para = CERT_CHAIN_PARA {
            cbSize: std::mem::size_of::<CERT_CHAIN_PARA>() as u32,
            ..Default::default()
        };
        let mut chain_ctx: *mut CERT_CHAIN_CONTEXT = std::ptr::null_mut();
        CertGetCertificateChain(
            HCERTCHAINENGINE::default(),
            ctx,
            None,
            HCERTSTORE::default(),
            &para,
            0,
            None,
            &mut chain_ctx,
        )
        .ok()?;
        if chain_ctx.is_null() {
            return None;
        }
        let chains = std::slice::from_raw_parts((*chain_ctx).rgpChain, (*chain_ctx).cChain as usize);
        let mut out = Vec::new();
        if let Some(first) = chains.first() {
            let elements =
                std::slice::from_raw_parts((**first).rgpElement, (**first).cElement as usize);
            for el in elements {
                let c = (**el).pCertContext;
                out.push(
                    std::slice::from_raw_parts((*c).pbCertEncoded, (*c).cbCertEncoded as usize)
                        .to_vec(),
                );
            }
        }
        CertFreeCertificateChain(chain_ctx);
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }
}

// ---- RustCrypto trait bridge: the CMS builder sees a normal RSA signer ----

impl signature::Keypair for CngSigner {
    type VerifyingKey = rsa::pkcs1v15::VerifyingKey<Sha256>;
    fn verifying_key(&self) -> Self::VerifyingKey {
        self.verifying_key.clone()
    }
}

impl spki::DynSignatureAlgorithmIdentifier for CngSigner {
    fn signature_algorithm_identifier(&self) -> spki::Result<spki::AlgorithmIdentifierOwned> {
        // sha256WithRSAEncryption, NULL params — identical to rsa::SigningKey.
        Ok(spki::AlgorithmIdentifierOwned {
            oid: "1.2.840.113549.1.1.11".parse().map_err(|_| spki::Error::KeyMalformed)?,
            parameters: Some(der::asn1::AnyRef::from(der::asn1::Null).into()),
        })
    }
}

impl signature::Signer<rsa::pkcs1v15::Signature> for CngSigner {
    fn try_sign(&self, msg: &[u8]) -> Result<rsa::pkcs1v15::Signature, signature::Error> {
        let digest = Sha256::digest(msg);
        let raw = self.sign_digest(&digest).map_err(signature::Error::from_source)?;
        rsa::pkcs1v15::Signature::try_from(raw.as_slice()).map_err(|_| signature::Error::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live end-to-end: for EVERY "PDF-EZ CNG Test*" RSA cert in the store
    /// (software-key and TPM-backed variants are minted by the test runner),
    /// sign a minimal PDF through the full store path and cryptographically
    /// verify the output. Skips silently when no test cert is present.
    #[test]
    fn store_signing_produces_verifiable_pkcs7() {
        let certs = match list_certs() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("store unavailable ({e}) — skipping");
                return;
            }
        };
        let test_certs: Vec<_> =
            certs.iter().filter(|c| c.subject.starts_with("PDF-EZ CNG Test") && c.rsa).collect();
        if test_certs.is_empty() {
            eprintln!("no PDF-EZ CNG Test cert in CurrentUser\\MY — skipping");
            return;
        }
        for cert in test_certs {
            let params = crate::sign::SignParams {
                reason: "CNG test".into(),
                name: "PDF-EZ".into(),
                location: "".into(),
            };
            let signed = crate::sign::sign_pdf_windows_store(
                crate::sign::test_support::minimal_pdf(),
                &cert.thumbprint,
                &params,
            )
            .unwrap_or_else(|e| panic!("store signing failed for {}: {e}", cert.subject));
            crate::sign::test_support::verify_signed_pdf(&signed);
            println!(
                "verified store signature: {} (provider: {}, tpm: {})",
                cert.subject, cert.provider, cert.tpm_backed
            );
        }
    }
}


//! PAdES B-B digital signing.
//!
//! Produces a cryptographically valid PKCS#7/CMS *detached* signature
//! (`SubFilter /adbe.pkcs7.detached`) embedded in the PDF, signed with an RSA
//! key loaded from a PKCS#12 (`.pfx` / `.p12`) file. Fully offline: no
//! timestamp authority and no revocation lookup — those are the optional,
//! user-initiated online upgrades (see the network policy). The self-asserted
//! signing time goes in the signature dictionary `/M` entry.
//!
//! Layout: add an (invisible) signature field to the document, serialize it
//! with a zero-filled `/Contents` gap and a fixed-width `/ByteRange`
//! placeholder, then patch the byte range, SHA-256 everything *except* the gap,
//! sign that hash, and splice the DER back into the gap. This is the standard
//! two-region signing layout every PDF validator expects. Because both the
//! `/ByteRange` patch and the `/Contents` fill preserve their exact byte
//! length, neither ever shifts the other or the total file length.

use cms::builder::{SignedDataBuilder, SignerInfoBuilder};
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::signed_data::{EncapsulatedContentInfo, SignerIdentifier};
use der::{Decode, Encode};
use lopdf::{Dictionary, Document, Object, StringFormat};
use p12_keystore::KeyStore;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::RsaPrivateKey;
use sha2::{Digest, Sha256};
use spki::AlgorithmIdentifierOwned;
use x509_cert::Certificate;

/// id-data (RFC 5652 §4).
const OID_ID_DATA: &str = "1.2.840.113549.1.7.1";
/// id-sha256.
const OID_SHA256: &str = "2.16.840.1.101.3.4.2.1";

/// Bytes reserved for the `/Contents` gap. The hex string holds twice this
/// many characters — ample for an RSA-2048/4096 signature plus a cert chain.
const CONTENTS_RESERVE: usize = 16384;

pub struct SignParams {
    pub reason: String,
    pub name: String,
    pub location: String,
}

/// Sign `pdf` and return the signed bytes. `pfx`/`password` unlock the signing
/// identity; `params` fills the human-readable signature-dictionary fields.
pub fn sign_pdf(
    pdf: Vec<u8>,
    pfx: &[u8],
    password: &str,
    params: &SignParams,
) -> Result<Vec<u8>, String> {
    // ---- private key + certificate chain from the PKCS#12 ----
    let store = KeyStore::from_pkcs12(pfx, password)
        .map_err(|_| "Could not open the .pfx — wrong password or unsupported file".to_string())?;
    let (_, chain) = store.private_key_chain().ok_or("The .pfx has no private key")?;
    let signing_key = load_rsa_key(chain.key())?;
    let certs = chain.chain();
    let leaf_der = certs.first().ok_or("The .pfx has no certificate")?.as_der();
    let leaf = Certificate::from_der(leaf_der).map_err(|e| format!("Certificate parse failed: {e}"))?;

    // ---- add the signature field, then serialize with placeholders ----
    let mut doc = Document::load_mem(&pdf).map_err(|e| format!("Could not read the PDF: {e}"))?;
    add_signature_field(&mut doc, params)?;
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).map_err(|e| format!("Could not write the PDF: {e}"))?;

    // ---- byte range = whole file minus the /Contents value ----
    let (lt, gt) = find_contents_gap(&bytes)?; // positions of '<' and '>'
    let file_len = bytes.len();
    let range = [0i64, lt as i64, (gt + 1) as i64, (file_len - gt - 1) as i64];
    patch_byte_range(&mut bytes, range)?;

    // ---- hash both signed regions (everything but the gap) ----
    let mut hasher = Sha256::new();
    hasher.update(&bytes[0..lt]);
    hasher.update(&bytes[gt + 1..file_len]);
    let digest = hasher.finalize();

    // ---- CMS detached signature over that digest ----
    let cms_der = build_cms(&signing_key, &leaf, certs, &digest)?;
    let hex = hex_lower(&cms_der);
    let gap = gt - lt - 1; // hex characters available between the brackets
    if hex.len() > gap {
        return Err("Signature is larger than the reserved space".to_string());
    }
    // Overwrite the zero-filled hex in place; trailing zeros stay as padding.
    bytes[lt + 1..lt + 1 + hex.len()].copy_from_slice(hex.as_bytes());
    Ok(bytes)
}

fn load_rsa_key(der: &[u8]) -> Result<SigningKey<Sha256>, String> {
    let key = RsaPrivateKey::from_pkcs8_der(der)
        .or_else(|_| {
            use rsa::pkcs1::DecodeRsaPrivateKey;
            RsaPrivateKey::from_pkcs1_der(der)
        })
        .map_err(|_| "Unsupported key — this version signs with RSA .pfx files only".to_string())?;
    Ok(SigningKey::<Sha256>::new(key))
}

fn build_cms(
    signing_key: &SigningKey<Sha256>,
    leaf: &Certificate,
    chain: &[p12_keystore::Certificate],
    digest: &[u8],
) -> Result<Vec<u8>, String> {
    let sha256 = AlgorithmIdentifierOwned {
        oid: OID_SHA256.parse().map_err(|_| "bad sha256 oid")?,
        parameters: None,
    };
    let eci = EncapsulatedContentInfo {
        econtent_type: OID_ID_DATA.parse().map_err(|_| "bad data oid")?,
        econtent: None, // detached
    };
    let sid = SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
        issuer: leaf.tbs_certificate.issuer.clone(),
        serial_number: leaf.tbs_certificate.serial_number.clone(),
    });
    let signer_info = SignerInfoBuilder::new(signing_key, sid, sha256.clone(), &eci, Some(digest))
        .map_err(|_| "Could not prepare the signer".to_string())?;

    let mut builder = SignedDataBuilder::new(&eci);
    builder
        .add_digest_algorithm(sha256)
        .map_err(|_| "Could not add digest algorithm".to_string())?;
    // Embed the full chain so a validator can build the path offline.
    for c in chain {
        let cert = Certificate::from_der(c.as_der()).map_err(|e| format!("Certificate parse: {e}"))?;
        builder
            .add_certificate(CertificateChoices::Certificate(cert))
            .map_err(|_| "Could not embed a certificate".to_string())?;
    }
    builder
        .add_signer_info::<_, rsa::pkcs1v15::Signature>(signer_info)
        .map_err(|_| "Signing failed".to_string())?;
    let ci = builder.build().map_err(|_| "Could not assemble the signature".to_string())?;
    ci.to_der().map_err(|_| "Could not encode the signature".to_string())
}

/// Build the signature dictionary + an invisible widget field and register it
/// in the AcroForm.
fn add_signature_field(doc: &mut Document, params: &SignParams) -> Result<(), String> {
    // Signature dictionary. /Contents and /ByteRange are inserted first, but
    // in-place patching keeps their lengths fixed so dict order is irrelevant.
    let mut sig = Dictionary::new();
    sig.set("Type", Object::Name(b"Sig".to_vec()));
    sig.set("Filter", Object::Name(b"Adobe.PPKLite".to_vec()));
    sig.set("SubFilter", Object::Name(b"adbe.pkcs7.detached".to_vec()));
    sig.set("Contents", Object::String(vec![0u8; CONTENTS_RESERVE], StringFormat::Hexadecimal));
    sig.set(
        "ByteRange",
        Object::Array(vec![
            Object::Integer(0),
            Object::Integer(9_999_999_999),
            Object::Integer(9_999_999_999),
            Object::Integer(9_999_999_999),
        ]),
    );
    if let Some(m) = pdf_date_now() {
        sig.set("M", Object::String(m.into_bytes(), StringFormat::Literal));
    }
    for (key, val) in [
        ("Reason", &params.reason),
        ("Name", &params.name),
        ("Location", &params.location),
    ] {
        if !val.is_empty() {
            sig.set(key, Object::String(val.clone().into_bytes(), StringFormat::Literal));
        }
    }
    let sig_id = doc.add_object(Object::Dictionary(sig));

    // Invisible signature widget (zero rect) on the first page.
    let first_page = doc
        .get_pages()
        .into_values()
        .next()
        .ok_or("The PDF has no pages")?;
    let mut field = Dictionary::new();
    field.set("Type", Object::Name(b"Annot".to_vec()));
    field.set("Subtype", Object::Name(b"Widget".to_vec()));
    field.set("FT", Object::Name(b"Sig".to_vec()));
    field.set("T", Object::String(b"ZugaSignature1".to_vec(), StringFormat::Literal));
    field.set("V", Object::Reference(sig_id));
    field.set("F", Object::Integer(132)); // Print (4) | Locked (128)
    field.set("Rect", Object::Array(vec![Object::Real(0.0); 4]));
    field.set("P", Object::Reference(first_page));
    let field_id = doc.add_object(Object::Dictionary(field));

    // Attach the widget to the page's /Annots.
    let page = doc
        .get_object_mut(first_page)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| format!("Page dictionary: {e}"))?;
    match page.get_mut(b"Annots") {
        Ok(Object::Array(a)) => a.push(Object::Reference(field_id)),
        _ => page.set("Annots", Object::Array(vec![Object::Reference(field_id)])),
    }

    // Register the field in the AcroForm and enable SigFlags.
    let root_id = match doc.trailer.get(b"Root") {
        Ok(Object::Reference(id)) => *id,
        _ => return Err("The PDF has no document catalog".to_string()),
    };
    let existing_af = doc
        .get_object(root_id)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .and_then(|c| c.get(b"AcroForm").ok().cloned());
    let mut af = match &existing_af {
        Some(Object::Reference(id)) => {
            doc.get_object(*id).ok().and_then(|o| o.as_dict().ok()).cloned().unwrap_or_default()
        }
        Some(Object::Dictionary(d)) => d.clone(),
        _ => Dictionary::new(),
    };
    match af.get_mut(b"Fields") {
        Ok(Object::Array(a)) => a.push(Object::Reference(field_id)),
        _ => af.set("Fields", Object::Array(vec![Object::Reference(field_id)])),
    }
    af.set("SigFlags", Object::Integer(3)); // SignaturesExist | AppendOnly
    let af_id = match existing_af {
        Some(Object::Reference(id)) => {
            doc.objects.insert(id, Object::Dictionary(af));
            id
        }
        _ => doc.add_object(Object::Dictionary(af)),
    };
    let catalog = doc
        .get_object_mut(root_id)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| format!("Document catalog: {e}"))?;
    catalog.set("AcroForm", Object::Reference(af_id));
    Ok(())
}

/// Positions of the `<` and `>` bounding the /Contents placeholder. Located by
/// the long run of ASCII `0` the placeholder is made of — searching for the
/// `/Contents` keyword would wrongly match a page's `/Contents` stream ref that
/// every real PDF carries.
fn find_contents_gap(bytes: &[u8]) -> Result<(usize, usize), String> {
    const MIN_RUN: usize = 1024; // far longer than any incidental hex string
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'0' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] == b'0' {
            i += 1;
        }
        if i - start >= MIN_RUN {
            let lt = start
                .checked_sub(1)
                .filter(|&p| bytes[p] == b'<')
                .ok_or("Signature placeholder is not bracketed")?;
            if bytes.get(i) == Some(&b'>') {
                return Ok((lt, i));
            }
            return Err("Signature placeholder is not closed".to_string());
        }
    }
    Err("No signature placeholder found in the serialized PDF".to_string())
}

/// Overwrite the fixed-width `/ByteRange [...]` placeholder with the real
/// values, right-padding with spaces so the span length is unchanged.
fn patch_byte_range(bytes: &mut [u8], range: [i64; 4]) -> Result<(), String> {
    let key = find(bytes, b"/ByteRange").ok_or("No /ByteRange in the signed PDF")?;
    let open = key + bytes[key..].iter().position(|&b| b == b'[').ok_or("Malformed /ByteRange")?;
    let close = open + bytes[open..].iter().position(|&b| b == b']').ok_or("Malformed /ByteRange")?;
    let span = close - open + 1; // inclusive of both brackets
    let text = format!("[0 {} {} {}]", range[1], range[2], range[3]);
    if text.len() > span {
        return Err("Byte range did not fit the placeholder".to_string());
    }
    let mut buf = text.into_bytes();
    buf.resize(span, b' '); // pad; the trailing ']' already sits inside buf
    // Ensure the region still ends with ']' so the array parses.
    let last = buf.len() - 1;
    if buf[last] == b' ' {
        // Move the closing bracket to the very end, blanks before it.
        let bracket = buf.iter().position(|&b| b == b']').unwrap();
        buf[bracket] = b' ';
        buf[last] = b']';
    }
    bytes[open..=close].copy_from_slice(&buf);
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// Self-asserted signing time as a PDF date string (`D:YYYYMMDDHHmmSSZ`, UTC).
fn pdf_date_now() -> Option<String> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    Some(format!(
        "D:{:04}{:02}{:02}{:02}{:02}{:02}Z",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
    ))
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use der::asn1::OctetString;
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::traits::PublicKeyParts as _;
    use std::str::FromStr;
    use x509_cert::builder::{Builder, CertificateBuilder, Profile};
    use x509_cert::name::Name;
    use x509_cert::serial_number::SerialNumber;
    use x509_cert::spki::SubjectPublicKeyInfoOwned;
    use x509_cert::time::Validity;

    /// A throwaway self-signed RSA identity packaged as a PKCS#12.
    fn make_test_pfx(password: &str) -> Vec<u8> {
        let mut rng = rand_core::OsRng;
        let key = RsaPrivateKey::new(&mut rng, 1024).unwrap(); // small = fast test
        let signer = SigningKey::<Sha256>::new(key.clone());
        let spki = SubjectPublicKeyInfoOwned::from_key(key.to_public_key()).unwrap();
        let subject = Name::from_str("CN=PD-EZ Test Root").unwrap();
        let validity = Validity::from_now(std::time::Duration::from_secs(3650 * 86_400)).unwrap();
        let builder = CertificateBuilder::new(
            Profile::Root,
            SerialNumber::from(1u32),
            validity,
            subject,
            spki,
            &signer,
        )
        .unwrap();
        let cert = builder.build::<rsa::pkcs1v15::Signature>().unwrap();
        let cert_der = cert.to_der().unwrap();
        let key_der = key.to_pkcs8_der().unwrap().as_bytes().to_vec();

        let mut store = KeyStore::new();
        let chain = p12_keystore::PrivateKeyChain::new(
            key_der,
            b"zuga-test-kid",
            vec![p12_keystore::Certificate::from_der(&cert_der).unwrap()],
        );
        store.add_entry("test", p12_keystore::KeyStoreEntry::PrivateKeyChain(chain));
        store.writer(password).write().unwrap()
    }

    /// A minimal one-page PDF that lopdf can round-trip.
    fn minimal_pdf() -> Vec<u8> {
        use lopdf::dictionary;
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    fn hex_decode(hex: &[u8]) -> Vec<u8> {
        hex.chunks(2)
            .map(|c| {
                let hi = (c[0] as char).to_digit(16).unwrap();
                let lo = (c[1] as char).to_digit(16).unwrap();
                (hi * 16 + lo) as u8
            })
            .collect()
    }

    /// Length of the leading DER TLV, so we can ignore the zero padding after it.
    fn der_total_len(b: &[u8]) -> usize {
        let first = b[1];
        if first < 0x80 {
            2 + first as usize
        } else {
            let n = (first & 0x7f) as usize;
            let mut len = 0usize;
            for i in 0..n {
                len = (len << 8) | b[2 + i] as usize;
            }
            2 + n + len
        }
    }

    fn parse_byte_range(bytes: &[u8]) -> [usize; 4] {
        let key = find(bytes, b"/ByteRange").unwrap();
        let open = key + bytes[key..].iter().position(|&b| b == b'[').unwrap();
        let close = open + bytes[open..].iter().position(|&b| b == b']').unwrap();
        let text = std::str::from_utf8(&bytes[open + 1..close]).unwrap();
        let nums: Vec<usize> = text.split_whitespace().map(|n| n.parse().unwrap()).collect();
        [nums[0], nums[1], nums[2], nums[3]]
    }

    /// End-to-end: sign a PDF, then cryptographically verify the result — the
    /// byte range tiles the file, the CMS parses, its messageDigest commits to
    /// the exact signed bytes, and the RSA signature over the signed attributes
    /// verifies. A signature that fails any of these is worse than none.
    #[test]
    fn sign_produces_verifiable_pkcs7() {
        let pfx = make_test_pfx("zuga");
        let params = SignParams {
            reason: "Approved".to_string(),
            name: "PD-EZ".to_string(),
            location: "".to_string(),
        };
        let signed = sign_pdf(minimal_pdf(), &pfx, "zuga", &params).unwrap();

        // 1. Byte range exactly tiles the file around the /Contents gap.
        // (find_contents_gap only works on the pre-sign placeholder; on signed
        // output the gap is derived from the byte range itself.)
        let br = parse_byte_range(&signed);
        let (lt, gt) = (br[1], br[2] - 1);
        assert_eq!(br[0], 0);
        assert_eq!(signed[lt], b'<', "range1 must end at the '<'");
        assert_eq!(signed[gt], b'>', "range2 must start after the '>'");
        assert_eq!(br[2] + br[3], signed.len(), "ranges must tile the file");

        // 2. /Contents decodes to a CMS SignedData.
        let der = hex_decode(&signed[lt + 1..gt]);
        let ci = cms::content_info::ContentInfo::from_der(&der[..der_total_len(&der)]).unwrap();
        let sd = ci.content.decode_as::<cms::signed_data::SignedData>().unwrap();
        let si = sd.signer_infos.0.as_ref().iter().next().unwrap();

        // 3. The signed messageDigest attribute equals SHA-256 of the two ranges.
        let mut hasher = Sha256::new();
        hasher.update(&signed[0..lt]);
        hasher.update(&signed[gt + 1..signed.len()]);
        let expected = hasher.finalize();
        let md_oid: der::asn1::ObjectIdentifier = "1.2.840.113549.1.9.4".parse().unwrap();
        let attrs = si.signed_attrs.as_ref().expect("signed attrs present");
        let md_attr = attrs.iter().find(|a| a.oid == md_oid).expect("messageDigest attr");
        let md_octets = md_attr.values.get(0).unwrap().decode_as::<OctetString>().unwrap();
        assert_eq!(md_octets.as_bytes(), &expected[..], "messageDigest must cover the signed bytes");

        // 4. The RSA signature over the DER-encoded signed attributes verifies.
        use rsa::pkcs1v15::{Signature, VerifyingKey};
        use signature::Verifier;
        let store = KeyStore::from_pkcs12(&pfx, "zuga").unwrap();
        let (_, chain) = store.private_key_chain().unwrap();
        let key = load_rsa_key(chain.key()).unwrap();
        let _ = key.as_ref().n(); // touch to ensure the key parsed
        let pubkey = rsa::RsaPublicKey::from(key.as_ref());
        let vk = VerifyingKey::<Sha256>::new(pubkey);
        let signed_attrs_der = attrs.to_der().unwrap();
        let sig = Signature::try_from(si.signature.as_bytes()).unwrap();
        vk.verify(&signed_attrs_der, &sig).expect("PKCS#7 signature must verify");
    }

    #[test]
    fn byte_range_placeholder_is_patched_in_place() {
        let mut bytes = b"xx/ByteRange [0 9999999999 9999999999 9999999999] yy".to_vec();
        let before = bytes.len();
        patch_byte_range(&mut bytes, [0, 10, 5000, 42]).unwrap();
        assert_eq!(bytes.len(), before, "patch must not change length");
        let br = parse_byte_range(&bytes);
        assert_eq!(br, [0, 10, 5000, 42]);
        assert!(bytes.ends_with(b"]  yy") || bytes.windows(2).any(|w| w == b"] "));
    }
}


//! RFC 3161 trusted timestamping — upgrades an existing PAdES B-B signature to
//! B-T by fetching a TimeStampToken from a TSA and embedding it as an
//! *unsigned* CMS attribute on the signature already in the PDF. Unsigned
//! attributes sit outside what the RSA signature covers (RFC 5652 §5.3), so
//! adding one never invalidates the signature — this is exactly what makes a
//! post-hoc upgrade possible: sign offline today, timestamp whenever you're
//! next online, no re-signing.
//!
//! User-initiated only, gated by `settings::require(OnlineFeature::TrustedTimestamp)`
//! at the call site. Nothing in this module runs unless the user pressed the
//! button — see the network policy in `settings.rs`.

use cms::content_info::ContentInfo;
use cms::signed_data::{SignedData, SignerInfo, SignerInfos};
use der::asn1::{Any, AnyRef, GeneralizedTime, Int, ObjectIdentifier, OctetString, SetOfVec};
use der::{Decode, Encode, Sequence, SliceReader};
use sha2::{Digest, Sha256};
use spki::AlgorithmIdentifierOwned;
use x509_cert::attr::{Attribute, Attributes};
use x509_cert::ext::Extensions;

/// id-aa-signatureTimeStampToken (RFC 5126 / ETSI CAdES, reused by PAdES).
const OID_SIGNATURE_TIMESTAMP_TOKEN: &str = "1.2.840.113549.1.9.16.2.14";
/// id-signedData (RFC 5652).
const OID_ID_SIGNED_DATA: &str = "1.2.840.113549.1.7.2";
/// id-sha256.
const OID_SHA256: &str = "2.16.840.1.101.3.4.2.1";

// ---- RFC 3161 wire types (only what we need; unknown optional fields are
// captured as opaque `Any` rather than modeled, since we don't act on them) ----

#[derive(Clone, Debug, Sequence)]
struct MessageImprint {
    hash_algorithm: AlgorithmIdentifierOwned,
    hashed_message: OctetString,
}

#[derive(Clone, Debug, Sequence)]
struct TimeStampReq {
    version: Int,
    message_imprint: MessageImprint,
    req_policy: Option<ObjectIdentifier>,
    nonce: Option<Int>,
    #[asn1(default = "Default::default")]
    cert_req: bool,
}

#[derive(Clone, Debug, Sequence)]
struct PkiStatusInfo {
    status: Int,
    status_string: Option<Any>,
    fail_info: Option<Any>,
}

#[derive(Clone, Debug, Sequence)]
struct TimeStampResp {
    status: PkiStatusInfo,
    time_stamp_token: Option<ContentInfo>,
}

#[derive(Clone, Debug, Sequence)]
struct TstInfo {
    version: Int,
    policy: ObjectIdentifier,
    message_imprint: MessageImprint,
    serial_number: Int,
    gen_time: GeneralizedTime,
    accuracy: Option<Any>,
    #[asn1(default = "Default::default")]
    ordering: bool,
    nonce: Option<Int>,
    #[asn1(context_specific = "0", tag_mode = "EXPLICIT", optional = "true")]
    tsa: Option<Any>,
    #[asn1(context_specific = "1", tag_mode = "IMPLICIT", constructed = "true", optional = "true")]
    extensions: Option<Extensions>,
}

/// Ask `tsa_url` to timestamp `signature_value` (the exact bytes of the
/// existing CMS `SignerInfo.signature` — i.e. the RSA signature octets, per
/// the signature-time-stamp attribute definition, NOT the whole document).
/// Returns the raw DER of the TimeStampToken (a `ContentInfo`) ready to embed.
fn request_timestamp(tsa_url: &str, signature_value: &[u8]) -> Result<Vec<u8>, String> {
    let imprint = Sha256::digest(signature_value);
    let sha256 = AlgorithmIdentifierOwned {
        oid: OID_SHA256.parse().map_err(|_| "bad sha256 oid")?,
        parameters: None,
    };
    let nonce_bytes = {
        let mut b = [0u8; 8];
        for (i, x) in b.iter_mut().enumerate() {
            *x = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
                >> (i * 8)) as u8;
        }
        b
    };
    let req = TimeStampReq {
        version: Int::new(&[1]).map_err(|_| "bad version int")?,
        message_imprint: MessageImprint {
            hash_algorithm: sha256,
            hashed_message: OctetString::new(imprint.to_vec()).map_err(|_| "bad digest")?,
        },
        req_policy: None,
        nonce: Some(Int::new(&nonce_bytes).map_err(|_| "bad nonce")?),
        cert_req: true,
    };
    let req_der = req.to_der().map_err(|e| format!("could not encode timestamp request: {e}"))?;

    let mut response = ureq::post(tsa_url)
        .content_type("application/timestamp-query")
        .send(req_der.as_slice())
        .map_err(|e| format!("could not reach timestamp authority: {e}"))?;
    if response.status() != 200 {
        return Err(format!("timestamp authority returned HTTP {}", response.status()));
    }
    let body = response
        .body_mut()
        .read_to_vec()
        .map_err(|e| format!("could not read timestamp response: {e}"))?;

    let mut reader = SliceReader::new(&body).map_err(|e| format!("bad TSA response: {e}"))?;
    let resp = TimeStampResp::decode(&mut reader).map_err(|e| format!("bad TSA response: {e}"))?;

    let status: i64 = resp.status.status.as_bytes().iter().fold(0i64, |acc, &b| (acc << 8) | b as i64);
    if status != 0 && status != 1 {
        return Err(format!("timestamp authority declined the request (status {status})"));
    }
    let token = resp.time_stamp_token.ok_or("timestamp authority granted but sent no token")?;
    if token.content_type.to_string() != OID_ID_SIGNED_DATA {
        return Err("timestamp token is not a CMS SignedData".to_string());
    }

    // Sanity-check the token actually covers OUR digest before trusting it —
    // a mismatch here means something is very wrong upstream; refuse to embed
    // it rather than silently attaching a timestamp for the wrong signature.
    let signed_data: SignedData =
        token.content.decode_as().map_err(|e| format!("could not parse timestamp token: {e}"))?;
    let econtent = signed_data
        .encap_content_info
        .econtent
        .ok_or("timestamp token has no TSTInfo content")?;
    let tst_octets: OctetString =
        econtent.decode_as().map_err(|e| format!("could not read TSTInfo: {e}"))?;
    let tst_info = TstInfo::from_der(tst_octets.as_bytes())
        .map_err(|e| format!("could not parse TSTInfo: {e}"))?;
    if tst_info.message_imprint.hashed_message.as_bytes() != imprint.as_slice() {
        return Err("timestamp token does not match this signature — refusing to embed".to_string());
    }

    token.to_der().map_err(|e| format!("could not re-encode timestamp token: {e}"))
}

/// Positions of the `<` and `>` bounding an ALREADY-SIGNED `/Contents` hex
/// string. Unlike `sign::find_contents_gap` (which looks for a long run of
/// zero bytes in the still-blank placeholder), the gap here already holds the
/// real signature bytes followed by zero padding, so we locate it by the
/// `/Contents<` literal instead (lopdf writes no space before the hex string)
/// — unambiguous, since a page's `/Contents` stream reference is never
/// followed by `<`.
fn find_signed_contents(bytes: &[u8]) -> Result<(usize, usize), String> {
    let key = find(bytes, b"/Contents<").ok_or("No signature /Contents found in this PDF")?;
    let lt = key + b"/Contents".len();
    let gt = lt + bytes[lt..].iter().position(|&b| b == b'>').ok_or("Unclosed /Contents value")?;
    Ok((lt, gt))
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

/// Fetch a trusted timestamp from `tsa_url` for the signature already present
/// in `pdf`, and embed it as an unsigned CMS attribute — upgrading PAdES B-B
/// to B-T. Requires the PDF to already carry exactly the signature written by
/// `sign::sign_pdf`; the file's total length and `/ByteRange` are unchanged,
/// only the (already fixed-width) `/Contents` gap is rewritten.
pub fn add_trusted_timestamp(pdf: Vec<u8>, tsa_url: &str) -> Result<Vec<u8>, String> {
    let mut bytes = pdf;
    let (lt, gt) = find_signed_contents(&bytes)?;
    let gap_hex = &bytes[lt + 1..gt];
    let cms_der = hex_decode(gap_hex)?;

    // Decode only the leading CMS TLV — the rest of the gap is zero padding
    // reserved at signing time, not real DER, so we must NOT require the
    // reader to consume it (a plain `from_der` would reject it as trailing
    // garbage).
    let mut reader = SliceReader::new(&cms_der).map_err(|e| format!("could not read signature: {e}"))?;
    let content_info =
        ContentInfo::decode(&mut reader).map_err(|e| format!("could not read signature: {e}"))?;
    if content_info.content_type.to_string() != OID_ID_SIGNED_DATA {
        return Err("this signature is not a CMS SignedData".to_string());
    }
    let signed_data: SignedData =
        content_info.content.decode_as().map_err(|e| format!("could not read signature: {e}"))?;
    let signer_info: &SignerInfo =
        signed_data.signer_infos.0.as_slice().first().ok_or("signature has no SignerInfo")?;

    let token_der = request_timestamp(tsa_url, signer_info.signature.as_bytes())?;
    let token_any = Any::from(AnyRef::try_from(token_der.as_slice()).map_err(|e| e.to_string())?);
    let attr = Attribute {
        oid: OID_SIGNATURE_TIMESTAMP_TOKEN.parse().map_err(|_| "bad attribute oid")?,
        values: SetOfVec::try_from(vec![token_any]).map_err(|e| e.to_string())?,
    };
    let mut updated_signer_info = signer_info.clone();
    updated_signer_info.unsigned_attrs =
        Some(Attributes::try_from(vec![attr]).map_err(|e| e.to_string())?);

    let mut updated_signed_data = signed_data;
    updated_signed_data.signer_infos =
        SignerInfos(SetOfVec::try_from(vec![updated_signer_info]).map_err(|e| e.to_string())?);
    let updated_der = updated_signed_data.to_der().map_err(|e| e.to_string())?;
    let updated_content = Any::from(AnyRef::try_from(updated_der.as_slice()).map_err(|e| e.to_string())?);
    let updated_ci = ContentInfo { content_type: content_info.content_type, content: updated_content };
    let final_der = updated_ci.to_der().map_err(|e| e.to_string())?;

    let hex = hex_lower(&final_der);
    let gap = gt - lt - 1;
    if hex.len() > gap {
        return Err("Timestamped signature is larger than the reserved space".to_string());
    }
    bytes[lt + 1..lt + 1 + hex.len()].copy_from_slice(hex.as_bytes());
    // Re-zero the rest of the gap (may hold shorter leftover hex from before).
    for b in &mut bytes[lt + 1 + hex.len()..gt] {
        *b = b'0';
    }
    Ok(bytes)
}

fn hex_decode(hex: &[u8]) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("Malformed signature hex string".to_string());
    }
    hex.chunks(2)
        .map(|c| {
            let hi = (c[0] as char).to_digit(16).ok_or("Malformed signature hex string")?;
            let lo = (c[1] as char).to_digit(16).ok_or("Malformed signature hex string")?;
            Ok((hi * 16 + lo) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sign::test_support::{make_test_pfx, minimal_pdf};
    use crate::sign::{sign_pdf, SignParams};

    #[test]
    fn finds_signed_contents_bracket_span() {
        let doc = b"prefix /Contents<deadbeef00000000> suffix";
        let (lt, gt) = find_signed_contents(doc).unwrap();
        assert_eq!(&doc[lt + 1..gt], b"deadbeef00000000");
    }

    #[test]
    fn rejects_document_with_no_signature() {
        let doc = b"/Contents 5 0 R"; // an ordinary page content stream ref
        assert!(find_signed_contents(doc).is_err());
    }

    /// End-to-end against a REAL public TSA (no mock — the whole point is to
    /// prove interop with an actual RFC 3161 responder's wire format). Not run
    /// by default: needs network. `cargo test add_trusted_timestamp -- --ignored`
    #[test]
    #[ignore]
    fn add_trusted_timestamp_against_real_tsa() {
        let pfx = make_test_pfx("zuga");
        let params = SignParams {
            reason: "Approved".to_string(),
            name: "PD-EZ".to_string(),
            location: "".to_string(),
        };
        let signed = sign_pdf(minimal_pdf(), &pfx, "zuga", &params).unwrap();

        let timestamped =
            add_trusted_timestamp(signed, crate::settings::DEFAULT_TSA_URL).unwrap();

        let (lt, gt) = find_signed_contents(&timestamped).unwrap();
        let cms_der = hex_decode(&timestamped[lt + 1..gt]).unwrap();
        let mut reader = SliceReader::new(&cms_der).unwrap();
        let content_info = ContentInfo::decode(&mut reader).unwrap();
        let signed_data: SignedData = content_info.content.decode_as().unwrap();
        let signer_info = signed_data.signer_infos.0.as_slice().first().unwrap();
        let unsigned = signer_info.unsigned_attrs.as_ref().expect("timestamp attribute missing");
        assert!(unsigned
            .as_slice()
            .iter()
            .any(|a| a.oid.to_string() == OID_SIGNATURE_TIMESTAMP_TOKEN));
    }
}

//! Dev tool: mint a signed license lease with the local dev key.
//! Not shipped — gated behind the `dev-tools` cargo feature.
//!
//!   cargo run --features dev-tools --bin mint_lease -- buyer@example.com [order_id]
//!
//! Writes pdez-<order_id>.lease next to the cwd; import it via the app's
//! activation screen. The same signing logic moves server-side (Polar.sh
//! webhook handler) for production.

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};

fn main() {
    let mut args = std::env::args().skip(1);
    let email = args.next().unwrap_or_else(|| {
        eprintln!("usage: mint_lease <email> [order_id]");
        std::process::exit(1);
    });
    let order_id = args.next().unwrap_or_else(|| format!("dev-{}", std::process::id()));

    let key_b64 = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/keys/license.key"))
        .expect("keys/license.key missing — generate the dev keypair first");
    let seed: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(key_b64.trim())
        .expect("bad key encoding")
        .try_into()
        .expect("key must be 32 bytes");
    let signing = SigningKey::from_bytes(&seed);

    let issued_at = chrono_free_now();
    let payload = format!("pdez.v1|{email}|{order_id}|{issued_at}");
    let sig = base64::engine::general_purpose::STANDARD.encode(signing.sign(payload.as_bytes()).to_bytes());

    let lease = serde_json::json!({
        "v": 1,
        "product": "pdez",
        "email": email,
        "order_id": order_id,
        "issued_at": issued_at,
        "sig": sig,
    });

    let out = format!("pdez-{order_id}.lease");
    std::fs::write(&out, serde_json::to_string_pretty(&lease).unwrap()).unwrap();
    println!("minted {out}");
}

/// RFC 3339 UTC date without pulling in chrono.
fn chrono_free_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Days-to-civil algorithm (Howard Hinnant), valid for the Unix era.
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

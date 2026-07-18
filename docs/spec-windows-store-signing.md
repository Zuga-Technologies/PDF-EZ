# Spec: Windows Certificate Store / TPM signing (V1.X)

**One-liner:** Sign with the certificate already in Windows. The private key
never leaves the machine — or, on TPM/smart-card certs, never leaves the chip.
No free tool (incl. Stirling PDF, whose signing means uploading your .pfx to a
server) can say this.

## What changes for the user

The Sign panel gets a second identity source above the existing .pfx picker:

```
Sign this document
  ( ) Windows certificate store        <- default when the store has any
        [Antonio Delgado  ·  RSA  ·  TPM-backed  ·  expires 2027-03-01  v]
  ( ) Digital ID file (.pfx / .p12)
        [Choose digital ID...]  [password]
```

- Store certs are listed newest-expiry first: subject CN, key algorithm,
  provider badge (`TPM` for Microsoft Platform Crypto Provider, `smart card`
  for card readers, nothing for software keys), expiry date.
- Expired certs and certs without a private key are not listed.
- Picking a store cert needs no password — Windows prompts natively (PIN
  dialog) if the key requires it. Everything else (reason/name/location,
  "Sign & save as", timestamps) is unchanged.

## Architecture

The PDF layout path (placeholder gap, /ByteRange patch, digest, splice) and
`build_cms` are already signer-generic via RustCrypto traits. CNG becomes a
second `Signer` implementation — nothing else moves.

```
src-tauri/src/sign.rs      build_cms made generic over S: Signer<pkcs1v15::Signature>
                           + Keypair + DynSignatureAlgorithmIdentifier (it already
                           is in effect; the pfx path keeps rsa::SigningKey)
src-tauri/src/winsign.rs   NEW:
  list_certs()          -> Vec<StoreCert{thumbprint, subject, issuer, not_after,
                           algorithm, provider, tpm_backed}>   [CurrentUser\MY]
  CngSigner             holds NCRYPT_KEY_HANDLE + leaf DER + chain DER
    - Keypair                    verifying key from the cert SPKI
    - DynSignatureAlgorithmIdentifier -> sha256WithRSAEncryption
    - Signer<pkcs1v15::Signature>     -> SHA-256(msg), NCryptSignHash(PKCS1)
  acquire(thumbprint)   -> CngSigner  (CertFindCertificateInStore by hash,
                           CryptAcquireCertificatePrivateKey ONLY_NCRYPT,
                           CertGetCertificateChain for the embedded chain)
```

Commands: `list_windows_certs()` and `sign_document_store(id, thumbprint,
reason, name, location, out)` alongside the existing `sign_document`.

## Scope guards

- **RSA certs only in this pass.** ECDSA needs a different CMS signature
  algorithm + raw-r||s-to-DER conversion; certs with EC keys are listed
  greyed-out ("ECDSA — coming soon") so the user sees why. (Most consumer
  signing certs are RSA.)
- CurrentUser\MY store only (no LocalMachine — avoids admin prompts).
- The PIN/consent dialog is Windows' own; we never touch the PIN.
- No key export, ever: only NCryptSignHash on a handle.

## Verification (definition of done)

1. `cargo test` — existing sign tests still green.
2. Live: mint a **software** self-signed RSA cert (New-SelfSignedCertificate),
   sign a testdata PDF through the store path, verify: byte range tiles the
   file, CMS parses, messageDigest matches, RSA signature over signedAttrs
   verifies against the cert public key. Delete the cert after.
3. Live: repeat with a **TPM-backed** cert (`-Provider "Microsoft Platform
   Crypto Provider"`) to prove hardware-key signing works end to end.
4. UI: cert list renders, picking + signing produces a -signed.pdf that the
   app reopens.

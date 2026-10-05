# printcraft-sign

Layer L4: digital signatures (execution plan M9; ISO 32000-2 §12.8; PAdES, ETSI EN 319 142).

```rust
let id = pkcs12::open(&std::fs::read("me.p12")?, "password")?;          // a digital ID
let signed = sign(&doc, &id, &SignOptions { page: 0, rect: Some(r), date, ..Default::default() })?;
for s in signatures(&doc, &bytes, &trust) {                                // list + validate
    println!("{}: {} — {:?}", s.field, s.summary(), s.details);
}
```

- **Formats, on RustCrypto primitives:** a small DER reader and writer (`der`), X.509
  certificates (`x509`), CMS SignedData (`cms`), and PKCS #12 digital ID files (`pkcs12`):
  PBES2 (PBKDF2 + AES/3DES), the legacy SHA-1 3DES/RC2 schemes, and MAC checks. Writing uses
  OpenSSL 3's defaults (AES-256-CBC, HMAC-SHA-256).
- **Keys:** RSA, ECDSA P-256 and P-384. Verification uses RustCrypto everywhere. RSA
  private-key operations (signing, key generation) use `aws-lc-rs` on native targets and are
  refused in the browser (ADR-0009: the `rsa` crate's Marvin advisory, RUSTSEC-2023-0071, has
  no fix, so `rsa` is only used to verify). ECDSA signs with deterministic nonces (RFC 6979).
- **Signing:** PAdES B-B (`ETSI.CAdES.detached`, signing-certificate-v2, SHA-256/384, no SHA-1).
  An existing unsigned field or a new one (visible with Acrobat's name-and-details appearance,
  or invisible); certification with DocMDP P=1/2/3. The document is written incrementally
  with a zero-filled `/Contents` and fixed-width `/ByteRange`, which are then patched in
  place. Encrypted documents are refused for now.
- **Validation:** `/ByteRange` and the CMS are read from the file's own bytes; the digest,
  the signature value and the signer's chain (against a `TrustStore`) are checked. Later
  revisions are diffed against the signed one, and the changes are classified (signing, form
  fill, comments, metadata, page content, document structure) under the DocMDP permissions.
  The verdict follows Acrobat: valid, unknown (intact but the identity isn't trusted) or invalid.

- **Keys held elsewhere** (`ExternalKey`: the private key never enters PrintCraft):
  - `pkcs11` (native targets): smart cards such as a US DoD Common Access Card (CAC) or a PIV
    card, and USB tokens, through the vendor's PKCS #11 library loaded at run time with
    `cryptoki` (OpenSC's `opensc-pkcs11`, ActivClient's `acpkcs211.dll`, p11-kit's proxy;
    `default_modules()` finds the usual install locations). `identities(module)` lists the
    certificates without a PIN (a CAC's certificates are public objects); encryption-only
    certificates (a CAC's Key Management certificate) are left out and the rest are labelled
    "Digital Signature" (nonRepudiation) or "Authentication", so the right one can be picked.
    `TokenIdentity::digital_id(pin)` signs on the card: log in (or use the reader's PIN pad),
    `CKM_RSA_PKCS` over a `DigestInfo` built in software, or `CKM_ECDSA` over the digest with
    the raw `r || s` re-encoded as DER. A wrong PIN is `SignError::WrongPin`. References are
    `pkcs11:<SHA-256 of the certificate>`.
  - `certstore` (Windows): certificates with a private key in CurrentUser\MY, smart cards
    included, signing through CNG (Windows asks for the PIN). `certstore:<SHA-256>`.
    Compile-checked only so far.
  - `keychain` (macOS): Keychain identities through the Security framework.

  `tests/pkcs11.rs` runs against a SoftHSM2 token (`apt-get install softhsm2`); without
  SoftHSM2 it prints a note and passes. `tests/softhsm/mod.rs` builds the token and is shared
  with the automation and UI tests.

Not yet: RFC 3161 timestamps, LTV (DSS/VRI, OCSP, CRL), FieldMDP locks, certificate security.

Oracles: poppler's `pdfsig` reports our signatures valid; OpenSSL reads our `.p12` files and
verifies our CMS; `tests/data/openssl-signed.pdf` is a signature OpenSSL made, which we validate.

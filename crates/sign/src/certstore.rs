//! Digital IDs from the Windows certificate store (CurrentUser\MY), through CNG. A CAC or PIV
//! card's certificates appear there through Windows' built-in smart card driver (or the card's
//! minidriver), and Windows itself asks for the PIN when the card signs. The private key never
//! leaves the store or the card.

use std::sync::Arc;

use rustls_cng::cert::CertContext;
use rustls_cng::error::CngError;
use rustls_cng::key::SignaturePadding;
use rustls_cng::store::{CertStore, CertStoreType};

use crate::keys::{DigestAlg, ExternalKey, PrivateKey, PublicKey};
use crate::pkcs11::{can_sign, ecdsa_der};
use crate::{Certificate, DigitalId, SignError};

/// `NTE_SILENT_CONTEXT`: the key exists but needs to show UI (a smart card PIN prompt).
const NTE_SILENT_CONTEXT: u32 = 0x8009_0022;
/// `SCARD_W_CANCELLED_BY_USER`, `ERROR_CANCELLED` (as an HRESULT and as a Win32 code).
const CANCELLED: [u32; 3] = [0x8010_006E, 0x8007_04C7, 1223];
/// `SCARD_W_WRONG_CHV`, `SCARD_W_CHV_BLOCKED`.
const WRONG_PIN: u32 = 0x8010_006B;
const PIN_BLOCKED: u32 = 0x8010_006C;
/// `SCARD_E_NO_SMARTCARD`, `SCARD_W_REMOVED_CARD`.
const NO_CARD: [u32; 2] = [0x8010_000C, 0x8010_0069];

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The `certstore:<SHA-256 of the certificate>` reference PrintCraft keeps for an identity.
pub fn reference(c: &Certificate) -> String {
    format!("certstore:{}", hex(&DigestAlg::Sha256.digest(&[&c.raw])))
}

fn cng_error(e: CngError, what: &str) -> SignError {
    match e {
        CngError::WindowsError(WRONG_PIN) => SignError::WrongPin,
        CngError::WindowsError(PIN_BLOCKED) => {
            SignError::Crypto("the card's PIN is locked after too many wrong tries; ask your card issuer to unlock it".into())
        }
        CngError::WindowsError(c) if CANCELLED.contains(&c) => SignError::Crypto("PIN entry was cancelled".into()),
        CngError::WindowsError(c) if NO_CARD.contains(&c) => {
            SignError::Crypto("the smart card is not in the reader (insert it and try again)".into())
        }
        e => SignError::Crypto(format!("Windows couldn't finish {what}: {e}")),
    }
}

fn open_store() -> Result<CertStore, SignError> {
    CertStore::open(CertStoreType::CurrentUser, "MY").map_err(|e| cng_error(e, "opening the certificate store"))
}

/// Whether the certificate has a private key PrintCraft can ask to sign (acquiring it silently
/// works, or would only need to show the PIN prompt).
fn has_key(ctx: &CertContext) -> bool {
    match ctx.acquire_key(true) {
        Ok(_) => true,
        Err(CngError::WindowsError(NTE_SILENT_CONTEXT)) => true,
        Err(_) => false,
    }
}

/// The signing identities in CurrentUser\MY (smart card certificates included), each with a
/// key Windows can use. Encryption-only certificates are left out.
pub fn identities() -> Result<Vec<DigitalId>, SignError> {
    let store = open_store()?;
    let all = store.find_all().map_err(|e| cng_error(e, "listing certificates"))?;
    let mut out: Vec<DigitalId> = Vec::new();
    for ctx in all.into_iter().take(1024) {
        let Ok(certificate) = Certificate::parse(ctx.as_der()) else { continue };
        if !can_sign(&certificate) || !has_key(&ctx) {
            continue;
        }
        if out.iter().any(|o| o.certificate.raw == certificate.raw) {
            continue;
        }
        let fingerprint = DigestAlg::Sha256.digest(&[&certificate.raw]);
        let key = StoreKey { fingerprint, public: certificate.public_key.clone() };
        let key = PrivateKey::external(certificate.public_key.clone(), Arc::new(key));
        let friendly_name = Some(certificate.display_name());
        out.push(DigitalId { key, certificate, chain: Vec::new(), friendly_name });
    }
    Ok(out)
}

/// The identity a reference (or a certificate common name) names.
pub fn find(reference_or_name: &str) -> Result<DigitalId, SignError> {
    let wanted = reference_or_name.strip_prefix("certstore:").unwrap_or(reference_or_name);
    let wanted_ref = format!("certstore:{}", wanted.to_ascii_lowercase().replace([':', ' '], ""));
    identities()?.into_iter().find(|id| reference(&id.certificate) == wanted_ref || id.certificate.subject.common_name() == Some(wanted)).ok_or_else(
        || {
            SignError::Crypto(format!(
                "no certificate {reference_or_name} with a private key in the Windows certificate store (is the card in the reader?)"
            ))
        },
    )
}

/// A key in the Windows store, found again by its certificate's SHA-256 at each signature.
struct StoreKey {
    fingerprint: Vec<u8>,
    public: PublicKey,
}

impl ExternalKey for StoreKey {
    fn sign(&self, alg: DigestAlg, msg: &[u8]) -> Result<Vec<u8>, SignError> {
        let store = open_store()?;
        let found = store.find_by_sha256(&self.fingerprint).map_err(|e| cng_error(e, "finding the certificate"))?;
        let ctx = found.first().ok_or_else(|| SignError::Crypto("the certificate is no longer in the Windows certificate store".into()))?;
        // Not silent: Windows shows its own PIN prompt for a smart card.
        let key = ctx.acquire_key(false).map_err(|e| cng_error(e, "opening the private key"))?;
        // CNG picks the hash algorithm for PKCS #1 padding from the digest length (32, 48 or
        // 64 bytes: SHA-256, SHA-384, SHA-512), which is what `alg` makes.
        let digest = alg.digest(&[msg]);
        match self.public {
            PublicKey::Rsa { .. } => key.sign(&digest, SignaturePadding::Pkcs1).map_err(|e| cng_error(e, "signing")),
            PublicKey::P256(_) | PublicKey::P384(_) => {
                let raw = key.sign(&digest, SignaturePadding::None).map_err(|e| cng_error(e, "signing"))?;
                ecdsa_der(&raw)
            }
        }
    }
}

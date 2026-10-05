//! Digital IDs on PKCS #11 tokens and smart cards (a US DoD Common Access Card, PIV cards,
//! USB tokens). The vendor's PKCS #11 library (OpenSC's `opensc-pkcs11`, ActivClient's
//! `acpkcs211.dll`, p11-kit's proxy) is loaded at run time through `cryptoki`.
//!
//! Certificates on a card are public objects, so [`identities`] lists them without a PIN. The
//! private key never leaves the card: [`TokenIdentity::digital_id`] makes a [`DigitalId`] whose
//! key logs in with the PIN the caller supplies (or the reader's PIN pad) and asks the card to
//! sign. Digests are computed in software: RSA keys sign a PKCS #1 `DigestInfo` with
//! `CKM_RSA_PKCS` (the mechanism every CAC supports), EC keys sign the digest with `CKM_ECDSA`
//! and the raw `r || s` result is DER-encoded for CMS.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use cryptoki::context::{CInitializeArgs, CInitializeFlags, Pkcs11};
use cryptoki::error::{Error as CkError, RvError};
use cryptoki::mechanism::Mechanism;
use cryptoki::object::{Attribute, AttributeType, CertificateType, KeyType, ObjectClass, ObjectHandle};
use cryptoki::session::{Session, UserType};
use cryptoki::slot::Slot;
use cryptoki::types::AuthPin;

use crate::der;
use crate::keys::{DigestAlg, ExternalKey, PrivateKey, PublicKey};
use crate::{Certificate, DigitalId, SignError};

/// At most this many objects are read from one token (a CAC holds a handful).
const MAX_OBJECTS: usize = 256;

/// The well-known places of PKCS #11 libraries for smart cards, those that exist on this
/// computer (OpenSC on every platform, ActivClient on Windows, p11-kit's proxy on Linux, which
/// forwards to every module the system has registered). Links to the same file are listed once.
pub fn default_modules() -> Vec<PathBuf> {
    let candidates: &[&str] = if cfg!(windows) {
        &[
            r"C:\Program Files\OpenSC Project\OpenSC\pkcs11\opensc-pkcs11.dll",
            r"C:\Windows\System32\opensc-pkcs11.dll",
            r"C:\Program Files\HID Global\ActivClient\acpkcs211.dll",
            r"C:\Program Files\ActivIdentity\ActivClient\acpkcs211.dll",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "/Library/OpenSC/lib/opensc-pkcs11.so",
            "/opt/homebrew/lib/opensc-pkcs11.so",
            "/opt/homebrew/lib/pkcs11/opensc-pkcs11.so",
            "/usr/local/lib/opensc-pkcs11.so",
            "/usr/local/lib/pkcs11/opensc-pkcs11.so",
        ]
    } else {
        &[
            "/usr/lib/x86_64-linux-gnu/opensc-pkcs11.so",
            "/usr/lib/aarch64-linux-gnu/opensc-pkcs11.so",
            "/usr/lib/x86_64-linux-gnu/pkcs11/opensc-pkcs11.so",
            "/usr/lib/aarch64-linux-gnu/pkcs11/opensc-pkcs11.so",
            "/usr/lib64/opensc-pkcs11.so",
            "/usr/lib64/pkcs11/opensc-pkcs11.so",
            "/usr/lib/opensc-pkcs11.so",
            "/usr/lib/pkcs11/opensc-pkcs11.so",
            "/usr/local/lib/opensc-pkcs11.so",
            "/usr/lib/x86_64-linux-gnu/p11-kit-proxy.so",
            "/usr/lib/aarch64-linux-gnu/p11-kit-proxy.so",
            "/usr/lib64/p11-kit-proxy.so",
            "/usr/lib/p11-kit-proxy.so",
        ]
    };
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for c in candidates {
        let p = Path::new(c);
        if !p.is_file() {
            continue;
        }
        let real = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        if !seen.contains(&real) {
            seen.push(real);
            out.push(p.to_path_buf());
        }
    }
    out
}

/// The loaded, initialized libraries. A PKCS #11 library is initialized once per process and
/// stays loaded (unloading one other code may still use is unsafe for some vendors).
fn context(module: &Path) -> Result<Pkcs11, SignError> {
    static LOADED: OnceLock<Mutex<HashMap<PathBuf, Pkcs11>>> = OnceLock::new();
    let mut loaded = LOADED.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|p| p.into_inner());
    if let Some(c) = loaded.get(module) {
        return Ok(c.clone());
    }
    if !module.is_file() {
        return Err(SignError::Crypto(format!(
            "the smart card library {} does not exist (install OpenSC or your card's middleware, or choose its PKCS #11 library)",
            module.display()
        )));
    }
    let ctx = Pkcs11::new(module).map_err(|e| SignError::Crypto(format!("the smart card library {} couldn't be loaded: {e}", module.display())))?;
    match ctx.initialize(CInitializeArgs::new(CInitializeFlags::OS_LOCKING_OK)) {
        Ok(()) | Err(CkError::Pkcs11(RvError::CryptokiAlreadyInitialized, _)) => {}
        Err(e) => return Err(SignError::Crypto(format!("the smart card library {} didn't start: {e}", module.display()))),
    }
    loaded.insert(module.to_path_buf(), ctx.clone());
    Ok(ctx)
}

/// A PKCS #11 failure as a message a person can act on.
fn ck_error(e: CkError, what: &str) -> SignError {
    match e {
        CkError::Pkcs11(rv, _) => match rv {
            RvError::PinIncorrect => SignError::WrongPin,
            RvError::PinLocked => SignError::Crypto(
                "the card's PIN is locked after too many wrong tries; ask your card issuer (for a CAC, a RAPIDS/ID card office) to unlock it".into(),
            ),
            RvError::PinExpired => SignError::Crypto("the card's PIN has expired; change it with your card's software first".into()),
            RvError::PinLenRange | RvError::PinInvalid => SignError::Crypto("the PIN has the wrong length or characters for this card".into()),
            RvError::TokenNotPresent | RvError::DeviceRemoved | RvError::SessionHandleInvalid | RvError::SessionClosed => {
                SignError::Crypto("the smart card is not in the reader (insert it and try again)".into())
            }
            RvError::TokenNotRecognized => SignError::Crypto("the card in the reader is not recognized by the smart card library".into()),
            RvError::FunctionCanceled => SignError::Crypto("PIN entry was cancelled".into()),
            RvError::DeviceError => SignError::Crypto(format!("the card reader reported an error while {what}; reinsert the card and try again")),
            rv => SignError::Crypto(format!("the smart card couldn't finish {what}: {rv}")),
        },
        e => SignError::Crypto(format!("the smart card couldn't finish {what}: {e}")),
    }
}

/// Lower-case hex.
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The `pkcs11:<SHA-256 of the certificate>` reference PrintCraft keeps for a token identity.
pub fn reference(c: &Certificate) -> String {
    format!("pkcs11:{}", hex(&DigestAlg::Sha256.digest(&[&c.raw])))
}

/// Whether a certificate's key may sign documents: no key usage extension, or digitalSignature
/// (bit 0) or nonRepudiation (bit 1). Encryption-only certificates (a CAC's Key Management
/// certificate: keyEncipherment alone) are left out.
pub fn can_sign(c: &Certificate) -> bool {
    !c.is_ca && c.key_usage.is_none_or(|u| u & 0b11 != 0)
}

/// What a certificate is for, in the words a CAC holder knows.
pub fn usage(c: &Certificate) -> &'static str {
    match c.key_usage {
        Some(u) if u & 0b10 != 0 => "Digital Signature",
        Some(u) if u & 0b01 != 0 => "Authentication",
        _ => "Signing",
    }
}

/// A certificate on a token whose private key can sign.
#[derive(Clone, Debug)]
pub struct TokenIdentity {
    pub certificate: Certificate,
    /// `pkcs11:<SHA-256 fingerprint>`.
    pub reference: String,
    /// The PKCS #11 library that sees the token.
    pub module: PathBuf,
    pub slot: u64,
    pub token_label: String,
    pub token_serial: String,
    /// The certificate object's `CKA_LABEL` ("Certificate for Digital Signature").
    pub label: String,
    /// `CKA_ID`, shared by the certificate and its private key.
    pub key_id: Vec<u8>,
    /// The reader has a PIN pad (`CKF_PROTECTED_AUTHENTICATION_PATH`): no PIN is typed here.
    pub pin_pad: bool,
}

impl TokenIdentity {
    /// "Digital Signature · Certificate for Digital Signature · PIV_II (token)".
    pub fn description(&self) -> String {
        let mut parts = vec![usage(&self.certificate).to_string()];
        if !self.label.is_empty() && self.label != usage(&self.certificate) {
            parts.push(self.label.clone());
        }
        if !self.token_label.is_empty() {
            parts.push(self.token_label.clone());
        }
        parts.join(" · ")
    }

    /// A digital ID that signs on the card, logging in with `pin` (`None` for a reader with a
    /// PIN pad, or a token that is already logged in). The PIN is kept only as long as the
    /// returned ID, which callers drop after signing.
    pub fn digital_id(&self, pin: Option<&str>) -> DigitalId {
        let key = TokenKey {
            module: self.module.clone(),
            slot: self.slot,
            token_serial: self.token_serial.clone(),
            key_id: self.key_id.clone(),
            public: self.certificate.public_key.clone(),
            pin: pin.map(|p| AuthPin::from(p.to_string())),
        };
        let key = PrivateKey::external(self.certificate.public_key.clone(), Arc::new(key));
        let name = self.certificate.display_name();
        DigitalId { key, certificate: self.certificate.clone(), chain: Vec::new(), friendly_name: Some(name) }
    }
}

/// The signing identities on every token `module` sees. Needs no PIN: a token whose private
/// keys are hidden until login contributes each certificate that may sign.
pub fn identities(module: &Path) -> Result<Vec<TokenIdentity>, SignError> {
    let ctx = context(module)?;
    let slots = ctx.get_slots_with_token().map_err(|e| ck_error(e, "listing the card readers"))?;
    let mut out: Vec<TokenIdentity> = Vec::new();
    for slot in slots {
        let Ok(info) = ctx.get_token_info(slot) else { continue };
        let Ok(session) = ctx.open_ro_session(slot) else { continue };
        for id in token_identities(&session, module, slot, &info) {
            if !out.iter().any(|o| o.reference == id.reference) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

/// The identities of every module in `modules`, without duplicates, and the errors of modules
/// that couldn't be read (a missing reader service is not worth stopping for).
pub fn identities_in(modules: &[PathBuf]) -> (Vec<TokenIdentity>, Vec<String>) {
    let mut out: Vec<TokenIdentity> = Vec::new();
    let mut errors = Vec::new();
    for m in modules {
        match identities(m) {
            Ok(ids) => {
                for id in ids {
                    if !out.iter().any(|o| o.reference == id.reference) {
                        out.push(id);
                    }
                }
            }
            Err(e) => errors.push(e.to_string()),
        }
    }
    (out, errors)
}

/// The identity a reference, certificate common name or object label names, among the tokens
/// of `modules`.
pub fn find(reference_or_name: &str, modules: &[PathBuf]) -> Result<TokenIdentity, SignError> {
    let (ids, errors) = identities_in(modules);
    let wanted = reference_or_name.strip_prefix("pkcs11:").unwrap_or(reference_or_name);
    let wanted_ref = format!("pkcs11:{}", wanted.to_ascii_lowercase().replace([':', ' '], ""));
    ids.into_iter()
        .find(|id| id.reference == wanted_ref || id.certificate.subject.common_name() == Some(wanted) || (!id.label.is_empty() && id.label == wanted))
        .ok_or_else(|| {
            let mut m = if modules.is_empty() {
                "no smart card library was found (install OpenSC or your card's middleware, or give the PKCS #11 library's path)".to_string()
            } else {
                format!("no smart card identity {reference_or_name} (is the card in the reader?)")
            };
            if !errors.is_empty() {
                m.push_str(&format!(": {}", errors.join("; ")));
            }
            SignError::Crypto(m)
        })
}

fn bytes_of(attrs: &[Attribute], kind: AttributeType) -> Option<Vec<u8>> {
    attrs.iter().find_map(|a| match (a, kind) {
        (Attribute::Value(v), AttributeType::Value) | (Attribute::Id(v), AttributeType::Id) | (Attribute::Label(v), AttributeType::Label) => {
            Some(v.clone())
        }
        (Attribute::Modulus(v), AttributeType::Modulus) => Some(v.clone()),
        _ => None,
    })
}

fn token_identities(session: &Session, module: &Path, slot: Slot, info: &cryptoki::slot::TokenInfo) -> Vec<TokenIdentity> {
    let Ok(certs) = session.find_objects(&[Attribute::Class(ObjectClass::CERTIFICATE), Attribute::CertificateType(CertificateType::X_509)]) else {
        return Vec::new();
    };
    // Private keys visible without logging in (many tokens hide them until login).
    let key_ids: Vec<Vec<u8>> = session
        .find_objects(&[Attribute::Class(ObjectClass::PRIVATE_KEY)])
        .unwrap_or_default()
        .into_iter()
        .take(MAX_OBJECTS)
        .filter_map(|k| session.get_attributes(k, &[AttributeType::Id]).ok().and_then(|a| bytes_of(&a, AttributeType::Id)))
        .collect();
    let mut out = Vec::new();
    for handle in certs.into_iter().take(MAX_OBJECTS) {
        let Ok(attrs) = session.get_attributes(handle, &[AttributeType::Value, AttributeType::Id, AttributeType::Label]) else { continue };
        let Some(Ok(certificate)) = bytes_of(&attrs, AttributeType::Value).map(|v| Certificate::parse(&v)) else { continue };
        let key_id = bytes_of(&attrs, AttributeType::Id).unwrap_or_default();
        // With visible keys, a certificate counts only if its key is there; otherwise any
        // certificate that may sign (its key is presumably among the hidden ones).
        let has_key = key_ids.is_empty() || key_ids.contains(&key_id);
        if !has_key || !can_sign(&certificate) {
            continue;
        }
        let label = bytes_of(&attrs, AttributeType::Label).map(|l| String::from_utf8_lossy(&l).trim().to_string()).unwrap_or_default();
        out.push(TokenIdentity {
            reference: reference(&certificate),
            certificate,
            module: module.to_path_buf(),
            slot: slot.id(),
            token_label: info.label().trim().to_string(),
            token_serial: info.serial_number().trim().to_string(),
            label,
            key_id,
            pin_pad: info.protected_authentication_path(),
        });
    }
    out
}

/// A private key on a token. Each signature opens a session, logs in, signs and closes it.
struct TokenKey {
    module: PathBuf,
    slot: u64,
    token_serial: String,
    key_id: Vec<u8>,
    public: PublicKey,
    pin: Option<AuthPin>,
}

impl TokenKey {
    /// The slot holding the token: found by serial number (slot numbers change when readers
    /// come and go), else the slot it was listed in.
    fn slot(&self, ctx: &Pkcs11) -> Result<(Slot, bool), SignError> {
        let slots = ctx.get_slots_with_token().map_err(|e| ck_error(e, "finding the card"))?;
        let mut fallback = None;
        for s in slots {
            let Ok(info) = ctx.get_token_info(s) else { continue };
            if !self.token_serial.is_empty() && info.serial_number().trim() == self.token_serial {
                return Ok((s, info.protected_authentication_path()));
            }
            if s.id() == self.slot {
                fallback = Some((s, info.protected_authentication_path()));
            }
        }
        if self.token_serial.is_empty()
            && let Some(f) = fallback
        {
            return Ok(f);
        }
        Err(SignError::Crypto("the smart card is not in the reader (insert it and try again)".into()))
    }

    /// The private key: by `CKA_ID`, else (RSA) the key whose modulus matches the certificate's.
    fn find_key(&self, session: &Session) -> Result<ObjectHandle, SignError> {
        if !self.key_id.is_empty() {
            let found = session
                .find_objects(&[Attribute::Class(ObjectClass::PRIVATE_KEY), Attribute::Id(self.key_id.clone())])
                .map_err(|e| ck_error(e, "finding the key"))?;
            if let Some(k) = found.first() {
                return Ok(*k);
            }
        }
        if let PublicKey::Rsa { n, .. } = &self.public {
            let keys = session.find_objects(&[Attribute::Class(ObjectClass::PRIVATE_KEY), Attribute::KeyType(KeyType::RSA)]).unwrap_or_default();
            for k in keys.into_iter().take(MAX_OBJECTS) {
                let m = session.get_attributes(k, &[AttributeType::Modulus]).ok().and_then(|a| bytes_of(&a, AttributeType::Modulus));
                if m.as_deref().map(strip_zeros) == Some(strip_zeros(n)) {
                    return Ok(k);
                }
            }
        }
        Err(SignError::Crypto("the card has no private key for this certificate".into()))
    }

    fn sign_in(&self, session: &Session, alg: DigestAlg, msg: &[u8]) -> Result<Vec<u8>, SignError> {
        let key = self.find_key(session)?;
        let digest = alg.digest(&[msg]);
        match &self.public {
            PublicKey::Rsa { .. } => {
                // PKCS #1 v1.5: the card pads a DigestInfo we build.
                let info = der::seq(&[&der::algorithm(alg.oid(), Some(&der::null())), &der::octets(&digest)]);
                session.sign(&Mechanism::RsaPkcs, key, &info).map_err(|e| ck_error(e, "signing"))
            }
            PublicKey::P256(_) | PublicKey::P384(_) => {
                let raw = session.sign(&Mechanism::Ecdsa, key, &digest).map_err(|e| ck_error(e, "signing"))?;
                ecdsa_der(&raw)
            }
        }
    }
}

fn strip_zeros(b: &[u8]) -> &[u8] {
    let i = b.iter().position(|x| *x != 0).unwrap_or(b.len());
    b.get(i..).unwrap_or_default()
}

/// A raw `r || s` ECDSA signature (PKCS #11, Windows CNG) as a DER `ECDSA-Sig-Value`.
pub fn ecdsa_der(raw: &[u8]) -> Result<Vec<u8>, SignError> {
    if raw.is_empty() || !raw.len().is_multiple_of(2) {
        return Err(SignError::Crypto(format!("the card returned a malformed ECDSA signature ({} bytes)", raw.len())));
    }
    let (r, s) = raw.split_at(raw.len() / 2);
    Ok(der::seq(&[&der::uint(r), &der::uint(s)]))
}

impl ExternalKey for TokenKey {
    fn sign(&self, alg: DigestAlg, msg: &[u8]) -> Result<Vec<u8>, SignError> {
        let ctx = context(&self.module)?;
        let (slot, pin_pad) = self.slot(&ctx)?;
        let session = ctx.open_ro_session(slot).map_err(|e| ck_error(e, "opening a session"))?;
        if self.pin.is_none() && !pin_pad {
            return Err(SignError::Crypto("enter the smart card's PIN".into()));
        }
        match session.login(UserType::User, self.pin.as_ref()) {
            Ok(()) | Err(CkError::Pkcs11(RvError::UserAlreadyLoggedIn, _)) => {}
            Err(e) => return Err(ck_error(e, "logging in")),
        }
        let result = self.sign_in(&session, alg, msg);
        // Closing the last session logs out as well; logging out first is tidy either way.
        let _ = session.logout();
        let _ = session.close();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_ecdsa_becomes_der() {
        let raw = [[0x80u8; 32], [0x01u8; 32]].concat();
        let d = ecdsa_der(&raw).unwrap();
        let t = der::Tlv::parse_all(&d).unwrap().children().unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].value.len(), 33, "a leading zero keeps r positive");
        assert!(ecdsa_der(&[1, 2, 3]).is_err());
        assert!(ecdsa_der(&[]).is_err());
    }

    #[test]
    fn encryption_only_certificates_are_left_out() {
        let key = PrivateKey::generate_p256().unwrap();
        let name = crate::Name::build("Card Holder", "", "", "", "US");
        let mut c = Certificate::self_signed(&name, &key, crate::Time::from_unix(1_767_225_600), 1, &[1]).unwrap();
        assert!(can_sign(&c));
        assert_eq!(usage(&c), "Digital Signature");
        c.key_usage = Some(0b1); // digitalSignature: a PIV Authentication certificate
        assert!(can_sign(&c));
        assert_eq!(usage(&c), "Authentication");
        c.key_usage = Some(0b100); // keyEncipherment alone: Key Management (encryption)
        assert!(!can_sign(&c));
        c.key_usage = None;
        assert!(can_sign(&c));
        c.is_ca = true;
        assert!(!can_sign(&c));
    }

    #[test]
    fn missing_module_is_a_clear_error() {
        let e = identities(Path::new("/nonexistent/opensc-pkcs11.so")).unwrap_err().to_string();
        assert!(e.contains("does not exist"), "{e}");
        let e = find("pkcs11:00", &[]).unwrap_err().to_string();
        assert!(e.contains("no smart card library"), "{e}");
    }
}

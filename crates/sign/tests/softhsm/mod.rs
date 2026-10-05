//! A SoftHSM2 token standing in for a smart card in tests (shared with the automation tests).
//!
//! SoftHSM reads `SOFTHSM2_CONF` when it is loaded, and a PKCS #11 library is initialized once
//! per process, so each test runs its body in a child process of the test binary with its own
//! token folder (setting the variable in-process would need `unsafe`).

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use cryptoki::context::{CInitializeArgs, CInitializeFlags, Pkcs11};
use cryptoki::object::{Attribute, CertificateType, KeyType, ObjectClass};
use cryptoki::session::UserType;
use cryptoki::types::AuthPin;
use printcraft_sign::der::Tlv;
use printcraft_sign::{Certificate, DigitalId, Name, PrivateKey, Time};

pub const PIN: &str = "123456";
const SO_PIN: &str = "87654321";
const CHILD: &str = "PRINTCRAFT_SOFTHSM_CHILD";

/// The SoftHSM2 library, if installed.
pub fn module() -> Option<PathBuf> {
    [
        "/usr/lib/softhsm/libsofthsm2.so",
        "/usr/lib/x86_64-linux-gnu/softhsm/libsofthsm2.so",
        "/usr/lib64/pkcs11/libsofthsm2.so",
        "/usr/lib64/softhsm/libsofthsm2.so",
        "/usr/local/lib/softhsm/libsofthsm2.so",
        "/opt/homebrew/lib/softhsm/libsofthsm2.so",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

/// Run `body` with a fresh SoftHSM token (the module path is passed in). In the parent test
/// process this re-runs the test `name` in a child with `SOFTHSM2_CONF` set and checks that it
/// passed; without SoftHSM it prints a note and returns.
pub fn run(name: &str, body: impl FnOnce(&Path)) {
    let Some(module) = module() else {
        eprintln!("note: SoftHSM2 is not installed (apt-get install softhsm2); skipping the PKCS #11 test {name}");
        return;
    };
    if std::env::var_os(CHILD).is_some() {
        body(&module);
        return;
    }
    let dir = std::env::temp_dir().join(format!("printcraft-softhsm-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("tokens")).unwrap();
    let conf = dir.join("softhsm2.conf");
    std::fs::write(&conf, format!("directories.tokendir = {}\nobjectstore.backend = file\nlog.level = ERROR\n", dir.join("tokens").display()))
        .unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env("SOFTHSM2_CONF", &conf)
        .env(CHILD, "1")
        .current_dir(&dir)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    println!("{text}");
    assert!(out.status.success(), "the SoftHSM child test failed:\n{text}");
    assert!(text.contains("1 passed"), "the child ran no test:\n{text}");
}

/// Initialize a token labelled "PrintCraft CAC" with user PIN [`PIN`] and import a 2048-bit RSA
/// and a P-256 digital ID onto it (private keys hidden until login, as on a CAC), each with
/// its certificate under the same `CKA_ID`. Returns the IDs (software copies, for comparison).
pub fn make_token(module: &Path) -> Vec<DigitalId> {
    let ctx = Pkcs11::new(module).unwrap();
    let _ = ctx.initialize(CInitializeArgs::new(CInitializeFlags::OS_LOCKING_OK));
    let slot = ctx.get_all_slots().unwrap()[0];
    let so = AuthPin::from(SO_PIN.to_string());
    ctx.init_token(slot, &so, "PrintCraft CAC").unwrap();
    // SoftHSM gives an initialized token a new slot number.
    let slot = ctx.get_slots_with_initialized_token().unwrap()[0];
    let session = ctx.open_rw_session(slot).unwrap();
    session.login(UserType::So, Some(&so)).unwrap();
    session.init_pin(&AuthPin::from(PIN.to_string())).unwrap();
    session.logout().unwrap();
    session.login(UserType::User, Some(&AuthPin::from(PIN.to_string()))).unwrap();
    let mut ids = Vec::new();
    for (i, (cn, key)) in [("Token Signer RSA", PrivateKey::generate_rsa(2048).unwrap()), ("Token Signer EC", PrivateKey::generate_p256().unwrap())]
        .into_iter()
        .enumerate()
    {
        let ckid = vec![1, i as u8 + 1];
        let name = Name::build(cn, "", "PrintCraft Test", "", "US");
        let cert = Certificate::self_signed(&name, &key, Time::from_unix(1_767_225_600), 5, &[0x10, i as u8 + 1]).unwrap();
        let info = Tlv::parse_all(key.pkcs8()).unwrap().children().unwrap();
        let inner = Tlv::parse_all(info[2].value).unwrap().children().unwrap();
        let common = vec![
            Attribute::Class(ObjectClass::PRIVATE_KEY),
            Attribute::Token(true),
            Attribute::Private(true),
            Attribute::Sensitive(true),
            Attribute::Sign(true),
            Attribute::Id(ckid.clone()),
            Attribute::Label(format!("Private key {cn}").into_bytes()),
        ];
        let mut template = common;
        if i == 0 {
            let n = |k: usize| inner[k].uint_bytes().to_vec();
            template.extend([
                Attribute::KeyType(KeyType::RSA),
                Attribute::Modulus(n(1)),
                Attribute::PublicExponent(n(2)),
                Attribute::PrivateExponent(n(3)),
                Attribute::Prime1(n(4)),
                Attribute::Prime2(n(5)),
                Attribute::Exponent1(n(6)),
                Attribute::Exponent2(n(7)),
                Attribute::Coefficient(n(8)),
            ]);
        } else {
            template.extend([
                Attribute::KeyType(KeyType::EC),
                Attribute::EcParams(printcraft_sign::der::oid(printcraft_sign::keys::oid::P256)),
                Attribute::Value(inner[1].value.to_vec()),
            ]);
        }
        session.create_object(&template).unwrap();
        session
            .create_object(&[
                Attribute::Class(ObjectClass::CERTIFICATE),
                Attribute::CertificateType(CertificateType::X_509),
                Attribute::Token(true),
                Attribute::Private(false),
                Attribute::Value(cert.raw.clone()),
                Attribute::Subject(cert.subject.raw.clone()),
                Attribute::Id(ckid),
                Attribute::Label(b"Certificate for Digital Signature".to_vec()),
            ])
            .unwrap();
        ids.push(DigitalId { key, certificate: cert, chain: Vec::new(), friendly_name: Some(cn.into()) });
    }
    session.logout().unwrap();
    session.close().unwrap();
    // The context stays loaded with the library (never finalized: PrintCraft shares it).
    std::mem::forget(ctx);
    ids
}

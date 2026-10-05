//! Signing with a PKCS #11 token (a SoftHSM2 token standing in for a CAC): listing identities
//! without a PIN, signing with the PIN, a wrong PIN, and validation.
#![cfg(not(target_arch = "wasm32"))]

mod softhsm;

use std::sync::Arc;

use printcraft_cos::Document;
use printcraft_sign::{SignError, SignOptions, Status, TrustStore, pkcs11, validate};

/// One page.
fn fixture() -> Vec<u8> {
    let objs: Vec<&[u8]> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] >>",
    ];
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

fn open(b: &[u8]) -> Document {
    Document::open(Arc::new(b.to_vec())).unwrap()
}

#[test]
fn signing_with_a_pkcs11_token() {
    softhsm::run("signing_with_a_pkcs11_token", |module| {
        let made = softhsm::make_token(module);
        // Listed without a PIN; the private keys are hidden until login, as on a CAC.
        let ids = pkcs11::identities(module).unwrap();
        assert_eq!(ids.len(), 2, "{ids:?}");
        for (id, m) in ids.iter().zip(&made) {
            assert_eq!(id.certificate, m.certificate);
            assert_eq!(id.reference, pkcs11::reference(&m.certificate));
            assert!(id.reference.starts_with("pkcs11:") && id.reference.len() == 7 + 64);
            assert_eq!((id.token_label.as_str(), id.label.as_str(), id.pin_pad), ("PrintCraft CAC", "Certificate for Digital Signature", false));
            assert!(id.description().starts_with("Digital Signature · "), "{}", id.description());
        }
        // Found by reference, by common name, and through a module list.
        let modules = vec![module.to_path_buf()];
        assert_eq!(pkcs11::find(&ids[1].reference, &modules).unwrap().reference, ids[1].reference);
        assert_eq!(pkcs11::find("pkcs11:Token Signer RSA", &modules).unwrap().reference, ids[0].reference);
        assert!(pkcs11::find("pkcs11:nobody", &modules).is_err());

        let opts = SignOptions { page: 0, rect: Some([20.0, 20.0, 220.0, 70.0]), date: "D:20261005120000Z".into(), ..SignOptions::default() };
        for id in &ids {
            let digital_id = id.digital_id(Some(softhsm::PIN));
            assert!(digital_id.key.is_external());
            let signed = printcraft_sign::sign(&open(&fixture()), &digital_id, &opts).unwrap();
            let trust = TrustStore { certs: vec![id.certificate.clone()] };
            let s = validate(&open(&signed), &signed, &trust, "Signature1").expect("the new signature");
            assert_eq!(s.status, Status::Valid, "{}: {:?}", id.certificate.display_name(), s.details);
            assert_eq!(s.signer.as_deref(), id.certificate.subject.common_name());
        }
        // A wrong PIN says so; no PIN (and no PIN pad) asks for one.
        let wrong = printcraft_sign::sign(&open(&fixture()), &ids[0].digital_id(Some("000000")), &opts).unwrap_err();
        assert!(matches!(wrong, SignError::WrongPin), "{wrong:?}");
        assert!(wrong.to_string().contains("PIN is incorrect") && wrong.to_string().contains("locks"));
        let none = printcraft_sign::sign(&open(&fixture()), &ids[0].digital_id(None), &opts).unwrap_err();
        assert!(none.to_string().contains("PIN"), "{none}");
        // The right PIN still works after a wrong one.
        assert!(printcraft_sign::sign(&open(&fixture()), &ids[1].digital_id(Some(softhsm::PIN)), &opts).is_ok());
    });
}

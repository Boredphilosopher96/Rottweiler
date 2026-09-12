use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use ed25519_dalek::{Signer as _, SigningKey};
use std::path::Path;
pub fn install(root: &Path, bytes: &[u8]) {
    let key = SigningKey::from_bytes(&[17; 32]);
    let public = key.verifying_key().to_bytes();
    let manifest = serde_json::from_value(serde_json::json!({
        "name":"mixed-policy","version":"1.0.0","protocol":3,
        "capabilities":{"hooks":[{"name":"pre_tool","class":"policy","failure_policy":"fail-closed"}]}
    })).expect("WASM manifest");
    let mut release = rw_ext::RegistryRelease {
        name: "mixed-policy".into(),
        version: "1.0.0".into(),
        manifest,
        component: rw_ext::RegistryArtifact {
            url: "https://fixture.invalid/policy.wasm".into(),
            blake3: blake3::hash(bytes).to_hex().to_string(),
            size: bytes.len() as u64,
        },
        publisher_key: STANDARD_NO_PAD.encode(public),
        signature: String::new(),
    };
    release.signature = STANDARD_NO_PAD.encode(
        key.sign(&release.signing_bytes().expect("signed bytes"))
            .to_bytes(),
    );
    rw_ext::install_verified_component(root, &release, &public, bytes)
        .expect("verified fixture installation");
    rw_ext::activate_installed_wasm_extension(root, "mixed-policy", "1.0.0")
        .expect("exact capability activation");
}

//! Signed v2 catalogs for service and worker tests.
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use oink_update::{
    Error, PRODUCT, WINDOWS_EPOCH,
    catalog::{self, Expected, Platform, VerifiedCatalog},
    feed::{Channel, Trust, Watermark},
};
use serde_json::{Value, json};
pub const NOW: i64 = 1794268800; // 2026-11-10 UTC
// TEST ONLY, derived in memory; never compiled into production trust.
fn key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}
pub fn public() -> [u8; 32] {
    key().verifying_key().to_bytes()
}
pub fn trust(public: &[u8; 32]) -> Trust<'_> {
    Trust {
        epoch: WINDOWS_EPOCH,
        key: public,
    }
}
pub fn release(build: u64, channel: &str) -> Value {
    json!({"build":build,"version":format!("0.3.{}",build / 1000 % 100),"tag":format!("v{build}"),"channel":channel,
        "sourceTreeSha256":"a".repeat(64),"minimumOs":"10.0.17763","protocolBaseline":1,"minimumUpdaterSchema":3,
        "notes":{"en":"Fixes","zh-Hans":"修复"},
        "package":{"url":format!("https://github.com/oinkvm/oink/releases/download/v{build}/Oink.msi"),"length":3,
            "sha256":"b".repeat(64),"keyId":WINDOWS_EPOCH,"ed25519":STANDARD.encode([0;64])}})
}
/// A beta catalog with 0.3.5, 0.3.6 (stable) and 0.3.7 (beta) for everyone.
pub fn payload() -> Value {
    json!({"schema":2,"product":PRODUCT,"platform":"windows","keyEpoch":WINDOWS_EPOCH,"architecture":"x64",
        "channel":"beta","sequence":5,"generatedAt":"2026-11-09T00:00:00Z","expiresAt":"2026-12-09T00:00:00Z",
        "releases":[release(305001,"stable"),release(306002,"stable"),release(307001,"beta")],
        "rules":[{"installed":[0,2147483647],"kind":"normal","target":"latest"}]})
}
pub fn envelope(payload: &Value) -> Vec<u8> {
    let bytes = serde_json::to_vec(payload).unwrap();
    serde_json::to_vec(&json!({"envelopeSchema":1,"payload":STANDARD.encode(&bytes),
        "signatures":[{"keyId":WINDOWS_EPOCH,"ed25519":STANDARD.encode(key().sign(&bytes).to_bytes())}]}))
    .unwrap()
}
pub fn verified(payload: &Value, channel: Channel) -> Result<VerifiedCatalog, Error> {
    let public = public();
    let expected = Expected {
        platform: Platform::Windows,
        architecture: "x64",
        channel,
    };
    catalog::verify(
        &envelope(payload),
        &trust(&public),
        &expected,
        &Watermark::default(),
        NOW,
    )
}

//! Display-only receipts, never installation or ownership authority.
use super::MicrophonePluginStep;
use serde::Deserialize;
#[derive(Deserialize)]
pub(super) struct Receipt {
    token: String,
    pub step: MicrophonePluginStep,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub detail: String,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub audio_defaults: Option<Vec<AudioDefaultChange>>,
}
#[derive(Debug, Deserialize)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) struct AudioDefaultChange {
    pub flow: String,
    pub role: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub outcome: String,
    pub detail: String,
}
pub(super) fn token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub(super) fn parse(bytes: &[u8], token: &str) -> Option<Receipt> {
    if bytes.len() > 8192
        || token.len() != 32
        || !token.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return None;
    }
    let receipt: Receipt = serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes)).ok()?;
    (receipt.token == token).then_some(receipt)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn progress_requires_random_current_token_known_step_and_bounded_complete_receipt() {
        let token = token().unwrap();
        let receipt = serde_json::json!({"token":token,"step":"copy_plugin","detail":""}).to_string();
        assert_eq!(
            parse(receipt.as_bytes(), &token).unwrap().step,
            MicrophonePluginStep::CopyPlugin
        );
        assert!(parse(receipt.as_bytes(), &"0".repeat(32)).is_none());
        assert!(parse(receipt.as_bytes(), "invalid").is_none());
        assert!(parse(receipt.replace("copy_plugin", "execute").as_bytes(), &token).is_none());
        assert!(parse(b"{partial", &token).is_none());
        assert!(parse(&[b' '; 8193], &token).is_none());
        let bom = [b"\xef\xbb\xbf".as_slice(), receipt.as_bytes()].concat();
        assert!(parse(&bom, &token).is_some());
    }
}

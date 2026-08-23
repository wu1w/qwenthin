use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::family::Family;

pub fn vendor_root() -> PathBuf {
    if let Ok(p) = std::env::var("Q38_VENDOR_DIR") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../third_party/qwen-family")
}

pub fn family_dir(family: Family) -> PathBuf {
    vendor_root().join(family.vendor_dir())
}

pub fn chat_template_path(family: Family) -> PathBuf {
    family_dir(family).join("chat_template.jinja")
}

pub fn tokenizer_path(family: Family) -> PathBuf {
    family_dir(family).join("tokenizer.json")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn verify_file(path: &Path, expected_hex: &str) -> Result<()> {
    let bytes = fs::read(path)?;
    let got = sha256_hex(&bytes);
    if got != expected_hex {
        return Err(Error::Vendor(format!(
            "{} hash mismatch: expected {expected_hex}, got {got}",
            path.display()
        )));
    }
    Ok(())
}

/// Locked hashes for the Qwen3.8-27B vendor set (primary).
pub mod qwen38 {
    pub const CHAT_TEMPLATE: &str =
        "c3cf9e34abf4f9e36c2d72165aa9c132d3e2a725b6c2586aaa3a8af9d7a81041";
    pub const TOKENIZER_JSON: &str =
        "0997f410c57a1f4e53b09e4be8f4a172d90edd9564368fb0847030937229b9f3";
    pub const TOKENIZER_CONFIG: &str =
        "b11349aafa7cdc6a320767cf7ceb29ed82f7eda5d65e8e0819e76f0ce947bf27";
}

pub fn verify_qwen38() -> Result<()> {
    let dir = family_dir(Family::Qwen38);
    verify_file(&dir.join("chat_template.jinja"), qwen38::CHAT_TEMPLATE)?;
    verify_file(&dir.join("tokenizer.json"), qwen38::TOKENIZER_JSON)?;
    verify_file(&dir.join("tokenizer_config.json"), qwen38::TOKENIZER_CONFIG)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen38_vendor_hashes() {
        verify_qwen38().expect("qwen38 vendor set");
    }
}

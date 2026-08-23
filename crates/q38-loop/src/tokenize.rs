use std::sync::OnceLock;

use tokenizers::Tokenizer;

use crate::error::{Error, Result};
use crate::family::Family;
use crate::vendor;

static QWEN38: OnceLock<Tokenizer> = OnceLock::new();

pub fn load_tokenizer(family: Family) -> Result<&'static Tokenizer> {
    match family {
        Family::Qwen38 | Family::Auto => qwen38_tokenizer(),
        Family::Qwen35 | Family::Qwen36 => Err(Error::Tokenizer(format!(
            "{} tokenizer.json is not vendored yet; prefix accounting is Qwen3.8-27B only",
            family.as_str()
        ))),
    }
}

pub fn qwen38_tokenizer() -> Result<&'static Tokenizer> {
    if let Some(t) = QWEN38.get() {
        return Ok(t);
    }
    vendor::verify_qwen38()?;
    let path = vendor::tokenizer_path(Family::Qwen38);
    let tok = Tokenizer::from_file(&path).map_err(|e| Error::Tokenizer(e.to_string()))?;
    Ok(QWEN38.get_or_init(|| tok))
}

pub fn count_tokens(family: Family, text: &str) -> Result<u32> {
    let tok = load_tokenizer(family)?;
    let enc = tok
        .encode(text, false)
        .map_err(|e| Error::Tokenizer(e.to_string()))?;
    Ok(enc.get_ids().len() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen38_tokenizer_loads() {
        let n = count_tokens(Family::Qwen38, "hello").unwrap();
        assert!(n > 0 && n < 10, "hello => {n} tokens");
    }

    #[test]
    fn cousin_tokenizer_refused() {
        let err = count_tokens(Family::Qwen35, "hello").unwrap_err();
        assert!(err.to_string().contains("not vendored"));
    }
}

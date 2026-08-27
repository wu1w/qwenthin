use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Qwen3.5-family generation. v1 quality gate is **Qwen3.8-27B**.
/// `Qwen38Next` is the Qwen4-preview Flash-Next line (sparse MoE, own Jinja).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    Auto,
    Qwen35,
    Qwen36,
    #[default]
    Qwen38,
    #[serde(rename = "q38next", alias = "qwen38next", alias = "flash-next")]
    Qwen38Next,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Qwen35 => "qwen35",
            Self::Qwen36 => "qwen36",
            Self::Qwen38 => "qwen38",
            Self::Qwen38Next => "q38next",
        }
    }

    pub fn vendor_dir(self) -> &'static str {
        match self {
            Self::Auto => "qwen38",
            Self::Qwen35 => "qwen35",
            Self::Qwen36 => "qwen36",
            Self::Qwen38 => "qwen38",
            Self::Qwen38Next => "q38next",
        }
    }

    /// Official `reasoning_effort` values this generation's Jinja accepts.
    /// Empty means the key must be omitted (3.5 / 3.6 templates have no effort sentence).
    pub fn effort_values(self) -> &'static [&'static str] {
        match self {
            Self::Qwen38 | Self::Qwen38Next => &["low", "medium", "xhigh"],
            Self::Auto | Self::Qwen35 | Self::Qwen36 => &[],
        }
    }

    pub fn preserve_thinking_kwarg(self) -> bool {
        matches!(
            self,
            Self::Qwen36 | Self::Qwen38 | Self::Qwen38Next | Self::Auto
        )
    }

    /// Follow-up compact after this many live tool messages. Flash-Next on a
    /// 1-slot llama.cpp box pays a full cold prefill; archive sooner than 27B.
    pub fn follow_up_compact_tools(self) -> usize {
        match self {
            Self::Qwen38Next => 6,
            _ => 8,
        }
    }

    /// Best-effort parse of a `/v1/models` id. Unknown Qwen-like ids return `None`
    /// so the builder will not inject 3.8 `xhigh` into a 3.5 template.
    pub fn detect(model_id: &str) -> Option<Self> {
        let s = model_id.to_ascii_lowercase();
        if s.contains("flash-next")
            || s.contains("flash_next")
            || s.contains("q38next")
            || s.contains("qwen3.8-flash")
            || s.contains("qwen38-flash")
        {
            return Some(Self::Qwen38Next);
        }
        if s.contains("qwen3.8") || s.contains("qwen3_8") {
            return Some(Self::Qwen38);
        }
        if s.contains("qwen3.6") || s.contains("qwen3_6") {
            return Some(Self::Qwen36);
        }
        if s.contains("qwen3.5") || s.contains("qwen3_5") {
            return Some(Self::Qwen35);
        }
        None
    }

    pub fn resolve(self, model_id: &str) -> Self {
        if self != Self::Auto {
            return self;
        }
        Self::detect(model_id).unwrap_or(Self::Auto)
    }
}

impl fmt::Display for Family {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Family {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "qwen35" | "qwen3.5" | "3.5" => Ok(Self::Qwen35),
            "qwen36" | "qwen3.6" | "3.6" => Ok(Self::Qwen36),
            "qwen38" | "qwen3.8" | "3.8" => Ok(Self::Qwen38),
            "q38next" | "qwen38next" | "qwen3.8-flash-next" | "flash-next" => Ok(Self::Qwen38Next),
            other => Err(format!("unknown family {other:?}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineProfile {
    #[default]
    Auto,
    LlamaCpp,
    Vllm,
    Sglang,
    Generic,
}

impl EngineProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::LlamaCpp => "llamacpp",
            Self::Vllm => "vllm",
            Self::Sglang => "sglang",
            Self::Generic => "generic",
        }
    }

    pub fn detect(base_url: &str, model_id: &str, owned_by: Option<&str>) -> Self {
        let blob = format!(
            "{} {} {}",
            base_url.to_ascii_lowercase(),
            model_id.to_ascii_lowercase(),
            owned_by.unwrap_or("").to_ascii_lowercase()
        );
        if blob.contains("sglang") {
            return Self::Sglang;
        }
        if blob.contains("vllm") {
            return Self::Vllm;
        }
        if blob.contains("llamacpp")
            || blob.contains("llama.cpp")
            || blob.contains("llama-cpp")
            || owned_by.is_some_and(|o| o.eq_ignore_ascii_case("llamacpp"))
        {
            return Self::LlamaCpp;
        }
        // llama.cpp default listen is :8080; only use this if nothing else matched.
        if let Ok(url) = reqwest::Url::parse(base_url) {
            if url.port() == Some(8080) {
                return Self::LlamaCpp;
            }
        }
        Self::Generic
    }

    pub fn resolve(self, base_url: &str, model_id: &str, owned_by: Option<&str>) -> Self {
        if self != Self::Auto {
            return self;
        }
        Self::detect(base_url, model_id, owned_by)
    }
}

impl fmt::Display for EngineProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EngineProfile {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "llamacpp" | "llama.cpp" | "llama-cpp" => Ok(Self::LlamaCpp),
            "vllm" => Ok(Self::Vllm),
            "sglang" => Ok(Self::Sglang),
            "generic" => Ok(Self::Generic),
            other => Err(format!("unknown profile {other:?}")),
        }
    }
}

/// Probe-filled capability bits. Unknown keys must not be sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointCaps {
    pub family: Family,
    pub profile: EngineProfile,
    pub enable_thinking: bool,
    pub effort_values: Vec<String>,
    pub preserve_thinking: bool,
    pub mtp: bool,
    pub cached_tokens_field: bool,
    pub quant_label: String,
    /// Native image input. `None` = not probed; agent will not attach pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_image: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_video: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_audio: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_transcription: Option<bool>,
}

impl EndpointCaps {
    /// Assumed 3.8 + llama.cpp caps used by unit tests and as a probe starting point.
    pub fn qwen38_llamacpp() -> Self {
        Self {
            family: Family::Qwen38,
            profile: EngineProfile::LlamaCpp,
            enable_thinking: true,
            effort_values: Family::Qwen38
                .effort_values()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            preserve_thinking: true,
            mtp: false,
            cached_tokens_field: false,
            quant_label: String::new(),
            supports_image: None,
            supports_video: None,
            supports_audio: None,
            supports_transcription: None,
        }
    }

    pub fn for_family(family: Family, profile: EngineProfile) -> Self {
        Self {
            family,
            profile,
            enable_thinking: true,
            effort_values: family
                .effort_values()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            preserve_thinking: family.preserve_thinking_kwarg(),
            mtp: false,
            cached_tokens_field: false,
            quant_label: String::new(),
            supports_image: None,
            supports_video: None,
            supports_audio: None,
            supports_transcription: None,
        }
    }

    pub fn media_caps(&self, origin: Option<(String, String)>) -> crate::media::MediaCaps {
        crate::media::MediaCaps {
            image: self.supports_image,
            video: self.supports_video,
            audio: self.supports_audio,
            transcription: self.supports_transcription,
            origin,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_qwen38_primary() {
        assert_eq!(Family::detect("Qwen3.8-27B-UD-Q8"), Some(Family::Qwen38));
        assert_eq!(
            Family::detect("qwen3.8-flash-next"),
            Some(Family::Qwen38Next)
        );
        assert_eq!(
            Family::detect("Qwen3.8-Flash-Next-UD-IQ3_XXS"),
            Some(Family::Qwen38Next)
        );
        assert_eq!(Family::detect("qwen3.6-27b"), Some(Family::Qwen36));
        assert_eq!(Family::detect("totally-other"), None);
    }

    #[test]
    fn qwen38_has_xhigh_others_do_not() {
        assert!(Family::Qwen38.effort_values().contains(&"xhigh"));
        assert!(Family::Qwen38Next.effort_values().contains(&"xhigh"));
        assert_eq!(Family::Qwen38Next.follow_up_compact_tools(), 6);
        assert!(Family::Qwen35.effort_values().is_empty());
        assert!(Family::Qwen36.effort_values().is_empty());
    }
}

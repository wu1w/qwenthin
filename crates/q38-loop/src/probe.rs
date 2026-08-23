use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapter::{build_chat_body, find_key, ChatRequestSpec};
use crate::config::{Config, CODING_CTX_TOKENS};
use crate::error::{Error, Result};
use crate::family::{EndpointCaps, EngineProfile, Family};
use crate::media::{
    image_probe_hit, silence_wav, video_probe_hit, MediaPart, IMAGE_PROBE_PROMPT, PROBE_IMAGE_B64,
    PROBE_VIDEO_URL, VIDEO_PROBE_PROMPT,
};
use crate::policy::ThinkPolicy;
use crate::prefix_cache::{self, classify, hit_pct, CrossTurnClass};
use crate::template::ChatMessage;
use crate::tokenize::count_tokens;

const OFF_THINK_MAX: u32 = 4;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeReport {
    pub family: String,
    pub profile: String,
    pub model: String,
    pub base_url: String,
    pub quant_label: String,
    pub enable_thinking: bool,
    pub effort_values: Vec<String>,
    pub preserve_thinking: Option<bool>,
    pub mtp: Option<bool>,
    pub cached_tokens_field: bool,
    pub prefill_tok_s: Option<f64>,
    pub decode_tok_s: Option<f64>,
    pub think_tokens_off: Option<u32>,
    pub think_tokens_low: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross_turn_off_hit_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross_turn_on_hit_pct: Option<f64>,
    pub red: Vec<String>,
    pub yellow: Vec<String>,
    pub notes: Vec<String>,
    pub probed_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_image: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_video: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_audio: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_transcription: Option<bool>,
}

impl ProbeReport {
    pub fn ok(&self) -> bool {
        self.red.is_empty()
    }

    pub fn to_caps(&self) -> crate::family::EndpointCaps {
        use crate::family::{EndpointCaps, EngineProfile, Family};
        let family = self.family.parse::<Family>().unwrap_or(Family::Qwen38);
        let profile = self
            .profile
            .parse::<EngineProfile>()
            .unwrap_or(EngineProfile::Generic);
        EndpointCaps {
            family,
            profile,
            enable_thinking: self.enable_thinking,
            effort_values: self.effort_values.clone(),
            preserve_thinking: self.preserve_thinking.unwrap_or(false),
            mtp: self.mtp.unwrap_or(false),
            cached_tokens_field: self.cached_tokens_field,
            quant_label: self.quant_label.clone(),
            supports_image: self.supports_image,
            supports_video: self.supports_video,
            supports_audio: self.supports_audio,
            supports_transcription: self.supports_transcription,
        }
    }
}

pub async fn run_probe(cfg: &Config) -> Result<ProbeReport> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(cfg.server.connect_timeout_s.max(1)))
        .timeout(Duration::from_secs(cfg.server.read_timeout_s.max(5)))
        .build()
        .map_err(|e| Error::Http(e.to_string()))?;

    let mut notes = Vec::new();
    let mut red = Vec::new();
    let mut yellow = Vec::new();
    let mut total_slots: Option<u32> = None;

    let (model, owned_by) = fetch_model(&client, cfg).await?;
    let family = cfg.server.family.resolve(&model);
    let profile = cfg
        .server
        .profile
        .resolve(&cfg.server.base_url, &model, owned_by.as_deref());
    notes.push(format!(
        "resolved family={family} profile={profile} model={model} (Qwen3.8-27B is the v1 quality gate)"
    ));

    let mut caps = EndpointCaps::for_family(family, profile);
    caps.quant_label = infer_quant(&model);

    let origin = engine_origin(&cfg.server.base_url);
    inspect_llamacpp(
        &client,
        &origin,
        profile,
        &mut caps,
        &mut notes,
        &mut yellow,
        &mut total_slots,
    )
    .await;

    let off_tokens = match complete_think(
        &client,
        cfg,
        &caps,
        &model,
        ThinkPolicy::off(),
        "Reply with the single word: ok",
    )
    .await
    {
        Ok(r) => {
            caps.cached_tokens_field = r.cached_tokens.is_some();
            Some(r)
        }
        Err(e) => {
            red.push(format!("thinking-off request failed: {e}"));
            None
        }
    };

    let low_tokens = match complete_think(
        &client,
        cfg,
        &caps,
        &model,
        ThinkPolicy::agent_default(),
        "Reply with the single word: ok",
    )
    .await
    {
        Ok(r) => Some(r),
        Err(e) => {
            red.push(format!("thinking-low request failed: {e}"));
            None
        }
    };

    let enable_thinking = match (&off_tokens, &low_tokens) {
        (Some(off), Some(low)) => {
            let off_n = off.think_tokens;
            let ok = off_n <= OFF_THINK_MAX;
            if !ok {
                red.push(format!(
                    "enable_thinking=false still produced {off_n} think tokens (need ≤ {OFF_THINK_MAX})"
                ));
            } else {
                notes.push(format!(
                    "per-request thinking off works (think_tokens={off_n})"
                ));
            }
            if low.think_tokens + 8 < off_n {
                yellow.push("low thinking was shorter than off; check parser".into());
            }
            ok
        }
        _ => false,
    };
    caps.enable_thinking = enable_thinking;

    let mut effort_values = Vec::new();
    if family == Family::Qwen38 {
        for effort in ["low", "medium", "xhigh"] {
            let mut policy = ThinkPolicy::agent_default();
            policy.effort = crate::policy::Effort::from_config(effort);
            match complete_think(
                &client,
                cfg,
                &caps,
                &model,
                policy,
                "Reply with the single word: ok",
            )
            .await
            {
                Ok(r) => {
                    notes.push(format!(
                        "effort={effort} think_tokens={} prompt_tokens={:?}",
                        r.think_tokens, r.prompt_tokens
                    ));
                    effort_values.push(effort.to_string());
                }
                Err(e) => yellow.push(format!("effort={effort} rejected: {e}")),
            }
        }
        if effort_values.is_empty() {
            yellow.push("no reasoning_effort value was accepted".into());
        }
    } else {
        notes.push(format!(
            "skipping effort sweep; {} template has no reasoning_effort sentence",
            family
        ));
    }
    caps.effort_values = effort_values.clone();

    let preserve_thinking = if family.preserve_thinking_kwarg() {
        Some(true)
    } else {
        None
    };
    if preserve_thinking.is_none() {
        notes.push("preserve_thinking=n/a for this family".into());
    }
    caps.preserve_thinking = preserve_thinking.unwrap_or(false);

    let cache = probe_cross_turn(
        &client,
        cfg,
        &caps,
        &model,
        profile,
        total_slots,
        &mut notes,
        &mut yellow,
    )
    .await;

    let (prefill_tok_s, decode_tok_s) = match &low_tokens {
        Some(r) => (r.prefill_tok_s, r.decode_tok_s),
        None => (None, None),
    };
    if prefill_tok_s.is_none() {
        yellow.push("no prefill tok/s (engine did not return timings)".into());
    }
    if decode_tok_s.is_none() {
        yellow.push("no decode tok/s (engine did not return timings)".into());
    }

    if family != Family::Qwen38 {
        yellow.push(format!(
            "resolved family is {family}; v1 quality gate is Qwen3.8-27B"
        ));
    }

    let mm = probe_multimodal(&client, cfg, &caps, &model, &mut notes, &mut yellow).await;

    let report = ProbeReport {
        family: family.as_str().to_string(),
        profile: profile.as_str().to_string(),
        model,
        base_url: cfg.server.base_url.clone(),
        quant_label: caps.quant_label,
        enable_thinking,
        effort_values,
        preserve_thinking,
        mtp: if profile == EngineProfile::LlamaCpp {
            Some(caps.mtp)
        } else {
            None
        },
        cached_tokens_field: caps.cached_tokens_field,
        prefill_tok_s,
        decode_tok_s,
        think_tokens_off: off_tokens.as_ref().map(|r| r.think_tokens),
        think_tokens_low: low_tokens.as_ref().map(|r| r.think_tokens),
        cross_turn_off_hit_pct: cache.off_hit_pct,
        cross_turn_on_hit_pct: cache.on_hit_pct,
        red,
        yellow,
        notes,
        probed_at_unix: unix_now(),
        supports_image: mm.image,
        supports_video: mm.video,
        supports_audio: mm.audio,
        supports_transcription: mm.transcription,
    };
    Ok(report)
}

struct CompleteResult {
    think_tokens: u32,
    prompt_tokens: Option<u32>,
    cached_tokens: Option<u32>,
    prefill_tok_s: Option<f64>,
    decode_tok_s: Option<f64>,
    content: String,
    reasoning: String,
}

struct CrossTurnHits {
    off_hit_pct: Option<f64>,
    on_hit_pct: Option<f64>,
}

pub async fn fetch_model(client: &Client, cfg: &Config) -> Result<(String, Option<String>)> {
    let url = format!("{}/models", cfg.server.base_url.trim_end_matches('/'));
    let mut req = client.get(&url);
    if !cfg.server.api_key.is_empty() {
        req = req.bearer_auth(&cfg.server.api_key);
    }
    let v: Value = req.send().await?.error_for_status()?.json().await?;
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| Error::Http("GET /v1/models: missing data[]".into()))?;
    if data.is_empty() {
        return Err(Error::Http("GET /v1/models: empty data[]".into()));
    }
    if !cfg.server.model.is_empty() {
        let found = data
            .iter()
            .find(|m| m["id"].as_str() == Some(cfg.server.model.as_str()));
        let row = found.unwrap_or(&data[0]);
        let id = row["id"].as_str().unwrap_or(&cfg.server.model).to_string();
        let owned = row["owned_by"].as_str().map(|s| s.to_string());
        if found.is_none() {
            return Ok((cfg.server.model.clone(), owned));
        }
        return Ok((id, owned));
    }
    let id = data[0]["id"]
        .as_str()
        .ok_or_else(|| Error::Http("model id missing".into()))?
        .to_string();
    let owned = data[0]["owned_by"].as_str().map(|s| s.to_string());
    Ok((id, owned))
}

/// Cheap GET `/models` for the console titlebar. Does not run the full probe.
pub async fn ping_models(cfg: &Config) -> Result<String> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| Error::Http(e.to_string()))?;
    let (id, _) = fetch_model(&client, cfg).await?;
    if cfg.server.model.trim().is_empty() {
        Ok(id)
    } else {
        Ok(cfg.server.model.clone())
    }
}

async fn complete_think(
    client: &Client,
    cfg: &Config,
    caps: &EndpointCaps,
    model: &str,
    policy: ThinkPolicy,
    user: &str,
) -> Result<CompleteResult> {
    complete_chat(
        client,
        cfg,
        caps,
        model,
        policy,
        &[ChatMessage::user(user)],
        false,
        None,
    )
    .await
}

async fn complete_chat(
    client: &Client,
    cfg: &Config,
    caps: &EndpointCaps,
    model: &str,
    mut policy: ThinkPolicy,
    messages: &[ChatMessage],
    cache_prompt: bool,
    id_slot: Option<i64>,
) -> Result<CompleteResult> {
    policy.max_tokens = 64;
    let body = build_chat_body(&ChatRequestSpec {
        model,
        messages,
        tools: None,
        stream: false,
        policy: &policy,
        caps,
        id_slot,
        cache_prompt,
        lossy_repeat: false,
    });
    let url = format!(
        "{}/chat/completions",
        cfg.server.base_url.trim_end_matches('/')
    );
    let mut req = client.post(&url).json(&body);
    if !cfg.server.api_key.is_empty() {
        req = req.bearer_auth(&cfg.server.api_key);
    }
    let v: Value = req.send().await?.error_for_status()?.json().await?;
    if let Some(err) = v.get("error") {
        return Err(Error::Http(err.to_string()));
    }
    let msg = &v["choices"][0]["message"];
    let reasoning = msg["reasoning_content"].as_str().unwrap_or("").to_string();
    let content = msg["content"].as_str().unwrap_or("").to_string();
    let think_text = if !reasoning.is_empty() {
        reasoning.clone()
    } else {
        extract_think(&content).unwrap_or_default()
    };
    let think_tokens = if think_text.is_empty() {
        0
    } else {
        count_tokens(Family::Qwen38, &think_text)
            .unwrap_or(think_text.split_whitespace().count() as u32)
    };

    let usage = &v["usage"];
    let prompt_tokens = usage["prompt_tokens"].as_u64().map(|n| n as u32);
    let cached_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|n| n.as_u64())
        .or_else(|| find_key(&v, "cached_tokens").and_then(|n| n.as_u64()))
        .or_else(|| v.pointer("/timings/cache_n").and_then(|n| n.as_u64()))
        .map(|n| n as u32);
    let cached_tokens = match (cached_tokens, prompt_tokens) {
        (Some(c), Some(p)) if p > 0 => Some(c.min(p)),
        (c, _) => c,
    };

    let timings = &v["timings"];
    let prefill_tok_s = timings["prompt_per_second"].as_f64();
    let decode_tok_s = timings["predicted_per_second"].as_f64();

    Ok(CompleteResult {
        think_tokens,
        prompt_tokens,
        cached_tokens,
        prefill_tok_s,
        decode_tok_s,
        content,
        reasoning,
    })
}

fn llama_probe_pin(profile: EngineProfile, total_slots: Option<u32>) -> (bool, Option<i64>) {
    match profile {
        EngineProfile::LlamaCpp | EngineProfile::Auto => {
            let slot = match total_slots {
                Some(n) if n > 1 => Some(i64::from(n) - 1),
                _ => Some(0),
            };
            (true, slot)
        }
        _ => (false, None),
    }
}

async fn probe_cross_turn(
    client: &Client,
    cfg: &Config,
    caps: &EndpointCaps,
    model: &str,
    profile: EngineProfile,
    total_slots: Option<u32>,
    notes: &mut Vec<String>,
    yellow: &mut Vec<String>,
) -> CrossTurnHits {
    let mut hits = CrossTurnHits {
        off_hit_pct: None,
        on_hit_pct: None,
    };
    if !family_has_preserve(caps) {
        notes.push("cross-turn cache probe skipped (preserve_thinking n/a)".into());
        return hits;
    }
    let (cache_prompt, id_slot) = llama_probe_pin(profile, total_slots);
    hits.off_hit_pct = probe_one_cross_turn(
        client,
        cfg,
        caps,
        model,
        ThinkPolicy::off(),
        "Reply with the single word: ping",
        "Reply with the single word: pong",
        cache_prompt,
        id_slot,
        notes,
        yellow,
        true,
    )
    .await;
    hits.on_hit_pct = probe_one_cross_turn(
        client,
        cfg,
        caps,
        model,
        ThinkPolicy::agent_default(),
        "Reply with the single word: alpha",
        "Reply with the single word: beta",
        cache_prompt,
        id_slot,
        notes,
        yellow,
        false,
    )
    .await;
    hits
}

fn family_has_preserve(caps: &EndpointCaps) -> bool {
    caps.family.preserve_thinking_kwarg()
}

#[allow(clippy::too_many_arguments)]
async fn probe_one_cross_turn(
    client: &Client,
    cfg: &Config,
    caps: &EndpointCaps,
    model: &str,
    policy: ThinkPolicy,
    user1: &str,
    user2: &str,
    cache_prompt: bool,
    id_slot: Option<i64>,
    notes: &mut Vec<String>,
    yellow: &mut Vec<String>,
    thinking_off: bool,
) -> Option<f64> {
    let label = if thinking_off {
        "thinking-off"
    } else {
        "thinking-on"
    };
    let t1 = match complete_chat(
        client,
        cfg,
        caps,
        model,
        policy.clone(),
        &[ChatMessage::user(user1)],
        cache_prompt,
        id_slot,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            yellow.push(format!("{label} cross-turn t1 failed: {e}"));
            return None;
        }
    };
    let reasoning = if t1.reasoning.is_empty() {
        None
    } else {
        Some(t1.reasoning.clone())
    };
    let hist = vec![
        ChatMessage::user(user1),
        ChatMessage::assistant_reply(Some(t1.content.clone()), reasoning, None),
    ];
    let mut t2_msgs = hist.clone();
    t2_msgs.push(ChatMessage::user(user2));
    let t2 = match complete_chat(
        client,
        cfg,
        caps,
        model,
        policy.clone(),
        &t2_msgs,
        cache_prompt,
        id_slot,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            yellow.push(format!("{label} cross-turn t2 failed: {e}"));
            return None;
        }
    };
    let Some(prompt) = t2.prompt_tokens.map(u64::from) else {
        notes.push(format!("{label} cross-turn: t2 omitted prompt_tokens"));
        return None;
    };
    let Some(cached) = t2.cached_tokens.map(u64::from) else {
        notes.push(format!(
            "{label} cross-turn: t2 omitted cached_tokens (prefix cache n/a)"
        ));
        return None;
    };
    let kwargs = policy.template_kwargs(caps);
    let hist_n = prefix_cache::hist_tokens(caps.family, &hist, None, kwargs.clone()).unwrap_or(0);
    let stripped_n =
        prefix_cache::stripped_tokens(caps.family, &t2_msgs, None, kwargs).unwrap_or(0);
    let class = classify(cached, prompt, hist_n, stripped_n);
    let pct = hit_pct(cached, prompt);
    let pct_s = pct
        .map(|p| format!("{p:.1}%"))
        .unwrap_or_else(|| "n/a".into());
    notes.push(format!(
        "{label} cross-turn t2 cached={cached}/{prompt} ({pct_s}) class={class:?} hist={hist_n} stripped={stripped_n}"
    ));
    match (thinking_off, class) {
        (true, CrossTurnClass::Warm) => {}
        (true, CrossTurnClass::Stripped) => yellow.push(
            "thinking-off cross-turn stuck at pre-assistant prefix (empty think wrapper mismatch; this engine's template may omit the generation wrapper)"
                .into(),
        ),
        (true, CrossTurnClass::Cold) => yellow.push(
            "thinking-off cross-turn cache miss (prefix cache off, or another request replaced the slot)"
                .into(),
        ),
        (true, CrossTurnClass::Partial) => yellow.push(format!(
            "thinking-off cross-turn partial hit {pct_s} (cached={cached} hist={hist_n} stripped={stripped_n})"
        )),
        (false, CrossTurnClass::Stripped) => notes.push(
            "thinking-on cross-turn first hop at pre-assistant prefix is expected with preserve=false"
                .into(),
        ),
        (false, CrossTurnClass::Warm) => notes.push(
            "thinking-on cross-turn stayed warm (server likely kept historical think)"
                .into(),
        ),
        (false, CrossTurnClass::Cold) => notes.push(
            "thinking-on cross-turn cache miss (same engine as thinking-off, or slot replaced between turns)"
                .into(),
        ),
        (false, CrossTurnClass::Partial) => notes.push(format!(
            "thinking-on cross-turn partial hit {pct_s}"
        )),
    }
    pct
}

fn extract_think(content: &str) -> Option<String> {
    let start = content.find("<think>")?;
    let rest = &content[start + 7..];
    let end = rest.find("</think>")?;
    Some(rest[..end].trim().to_string())
}

struct MmFlags {
    image: Option<bool>,
    video: Option<bool>,
    audio: Option<bool>,
    transcription: Option<bool>,
}

async fn probe_multimodal(
    client: &Client,
    cfg: &Config,
    caps: &EndpointCaps,
    model: &str,
    notes: &mut Vec<String>,
    yellow: &mut Vec<String>,
) -> MmFlags {
    let mut flags = MmFlags {
        image: Some(false),
        video: Some(false),
        audio: Some(false),
        transcription: Some(false),
    };

    match probe_image(client, cfg, caps, model).await {
        Ok((hit, answer)) => {
            flags.image = Some(hit);
            notes.push(format!(
                "image probe {} (answer={:?})",
                if hit { "yes" } else { "no" },
                clip_probe(&answer)
            ));
        }
        Err(e) => {
            flags.image = Some(false);
            yellow.push(format!("image probe failed: {e}"));
        }
    }

    if flags.image == Some(true) {
        match probe_video(client, cfg, caps, model).await {
            Ok((hit, answer)) => {
                flags.video = Some(hit);
                notes.push(format!(
                    "video probe {} (answer={:?})",
                    if hit { "yes" } else { "no" },
                    clip_probe(&answer)
                ));
            }
            Err(e) => {
                flags.video = Some(false);
                yellow.push(format!("video probe failed: {e}"));
            }
        }
    } else {
        notes.push("video probe skipped (image not supported)".into());
    }

    match probe_audio_native(client, cfg, caps, model).await {
        Ok((hit, answer)) => {
            flags.audio = Some(hit);
            notes.push(format!(
                "audio native {} (answer={:?})",
                if hit { "yes" } else { "no" },
                clip_probe(&answer)
            ));
        }
        Err(e) => {
            flags.audio = Some(false);
            yellow.push(format!("audio probe failed: {e}"));
        }
    }

    match probe_transcriptions(client, cfg).await {
        Ok(hit) => {
            flags.transcription = Some(hit);
            notes.push(format!(
                "audio/transcriptions {}",
                if hit { "yes" } else { "no" }
            ));
        }
        Err(e) => {
            flags.transcription = Some(false);
            yellow.push(format!("transcriptions probe failed: {e}"));
        }
    }

    flags
}

async fn probe_image(
    client: &Client,
    cfg: &Config,
    caps: &EndpointCaps,
    model: &str,
) -> Result<(bool, String)> {
    let mut user = ChatMessage::user(IMAGE_PROBE_PROMPT);
    user.parts = vec![MediaPart::image_url(format!(
        "data:image/png;base64,{PROBE_IMAGE_B64}"
    ))];
    let (content, reasoning) = complete_messages(client, cfg, caps, model, &[user]).await?;
    Ok((
        image_probe_hit(&content, &reasoning),
        format!("{content} | {reasoning}"),
    ))
}

async fn probe_video(
    client: &Client,
    cfg: &Config,
    caps: &EndpointCaps,
    model: &str,
) -> Result<(bool, String)> {
    let mut user = ChatMessage::user(VIDEO_PROBE_PROMPT);
    user.parts = vec![MediaPart::video_url(PROBE_VIDEO_URL)];
    match complete_messages(client, cfg, caps, model, &[user]).await {
        Ok((content, reasoning)) => Ok((
            video_probe_hit(&content, &reasoning, true),
            format!("{content} | {reasoning}"),
        )),
        Err(e) if media_rejected(&e.to_string()) => Ok((false, e.to_string())),
        Err(e) => Err(e),
    }
}

fn media_rejected(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("unsupported")
        || l.contains("not support")
        || l.contains("mmproj")
        || l.contains("invalid_request")
        || l.contains("501")
}

async fn probe_audio_native(
    client: &Client,
    cfg: &Config,
    caps: &EndpointCaps,
    model: &str,
) -> Result<(bool, String)> {
    let wav = silence_wav();
    let mut user = ChatMessage::user("Reply with the single word: ok");
    user.parts = vec![MediaPart::data_uri(
        crate::media::MediaKind::Audio,
        "audio/wav",
        &wav,
    )];
    match complete_messages(client, cfg, caps, model, &[user]).await {
        Ok((content, reasoning)) => {
            let blob = format!("{content} {reasoning}");
            // HTTP 200 is not enough — official 3.8 Jinja raises on audio parts
            // at the server if the template is stock. A non-empty reply without
            // a media-keyword error counts as native audio.
            let lower = blob.to_ascii_lowercase();
            let rejected = [
                "image",
                "video",
                "audio",
                "vision",
                "multimodal",
                "unexpected",
            ]
            .iter()
            .any(|k| {
                lower.contains(k) && (lower.contains("not support") || lower.contains("unexpected"))
            });
            Ok((!content.trim().is_empty() && !rejected, blob))
        }
        Err(e) => {
            let s = e.to_string();
            if media_rejected(&s) || s.to_ascii_lowercase().contains("audio") {
                Ok((false, s))
            } else {
                Err(e)
            }
        }
    }
}

async fn probe_transcriptions(client: &Client, cfg: &Config) -> Result<bool> {
    let url = format!(
        "{}/audio/transcriptions",
        cfg.server.base_url.trim_end_matches('/')
    );
    let wav = silence_wav();
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("probe.wav")
        .mime_str("audio/wav")
        .map_err(|e| Error::Http(e.to_string()))?;
    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);
    let mut req = client.post(&url).multipart(form);
    if !cfg.server.api_key.is_empty() {
        req = req.bearer_auth(&cfg.server.api_key);
    }
    match req.send().await {
        Ok(resp) => Ok(resp.status().is_success()),
        Err(e) => {
            let s = e.to_string();
            if s.contains("404") || s.contains("error sending") {
                Ok(false)
            } else {
                Err(Error::Http(s))
            }
        }
    }
}

async fn complete_messages(
    client: &Client,
    cfg: &Config,
    caps: &EndpointCaps,
    model: &str,
    messages: &[ChatMessage],
) -> Result<(String, String)> {
    let mut policy = ThinkPolicy::off();
    policy.max_tokens = 64;
    let body = build_chat_body(&ChatRequestSpec {
        model,
        messages,
        tools: None,
        stream: false,
        policy: &policy,
        caps,
        id_slot: None,
        cache_prompt: false,
        lossy_repeat: false,
    });
    let url = format!(
        "{}/chat/completions",
        cfg.server.base_url.trim_end_matches('/')
    );
    let mut req = client.post(&url).json(&body);
    if !cfg.server.api_key.is_empty() {
        req = req.bearer_auth(&cfg.server.api_key);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let v: Value = resp.json().await?;
    if !status.is_success() {
        return Err(Error::Http(format!("{status}: {v}")));
    }
    if let Some(err) = v.get("error") {
        return Err(Error::Http(err.to_string()));
    }
    let msg = &v["choices"][0]["message"];
    let content = msg["content"].as_str().unwrap_or("").to_string();
    let reasoning = msg["reasoning_content"].as_str().unwrap_or("").to_string();
    Ok((content, reasoning))
}

fn clip_probe(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = 80usize;
    if flat.chars().count() <= n {
        return flat;
    }
    format!(
        "{}…",
        flat.chars().take(n.saturating_sub(1)).collect::<String>()
    )
}

async fn inspect_llamacpp(
    client: &Client,
    origin: &str,
    profile: EngineProfile,
    caps: &mut crate::family::EndpointCaps,
    notes: &mut Vec<String>,
    yellow: &mut Vec<String>,
    total_slots: &mut Option<u32>,
) {
    if profile != EngineProfile::LlamaCpp {
        return;
    }
    let url = format!("{}/props", origin.trim_end_matches('/'));
    let Ok(v) = client.get(&url).send().await else {
        yellow.push("MTP presence unknown (GET /props failed)".into());
        return;
    };
    let Ok(v) = v.json::<Value>().await else {
        yellow.push("MTP presence unknown (GET /props not JSON)".into());
        return;
    };

    if let Some(n_ctx) = v
        .pointer("/default_generation_settings/n_ctx")
        .and_then(|x| x.as_u64())
    {
        notes.push(format!("server n_ctx={n_ctx}"));
        if n_ctx < 131_072 {
            yellow.push(format!(
                "server n_ctx={n_ctx}; coding baseline is {CODING_CTX_TOKENS} (short windows force compact/re-read)"
            ));
        } else if n_ctx >= u64::from(CODING_CTX_TOKENS) {
            notes.push(format!("n_ctx={n_ctx} meets the 262k coding baseline"));
        }
    }
    if let Some(n) = v.get("total_slots").and_then(|x| x.as_u64()) {
        notes.push(format!("total_slots={n}"));
        *total_slots = n.try_into().ok();
        if n <= 1 {
            yellow.push("total_slots=1: another chat completion replaces this slot's KV".into());
        }
    }

    if let Some(ftype) = v.get("model_ftype").and_then(|x| x.as_str()) {
        notes.push(format!("model_ftype={ftype}"));
        if caps.quant_label.contains("q8") && ftype.to_ascii_lowercase().contains("q4") {
            yellow.push(format!(
                "model id looks like {} but llama.cpp reports ftype={ftype}",
                caps.quant_label
            ));
        }
    }
    if let Some(path) = v.get("model_path").and_then(|x| x.as_str()) {
        notes.push(format!("model_path={path}"));
    }

    let blob = v.to_string().to_ascii_lowercase();
    let mtp = blob.contains("draft") || blob.contains("speculat") || blob.contains("mtp");
    caps.mtp = mtp;
    notes.push(format!("mtp={}", if mtp { "yes" } else { "no" }));
}

fn engine_origin(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

fn infer_quant(model: &str) -> String {
    let s = model.to_ascii_lowercase();
    for tag in [
        "ud-q8", "q8_0", "q6_k", "q5_k", "q4_k", "q4_0", "fp8", "awq", "gptq", "nvfp4", "bf16",
        "fp16",
    ] {
        if s.contains(tag) {
            return tag.to_string();
        }
    }
    model.to_string()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn write_report(report: &ProbeReport) -> Result<std::path::PathBuf> {
    let path = Config::probe_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(report).map_err(|e| Error::msg(e))?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Convenience constructor used by tests that don't hit the network.
pub fn report_from_caps(caps: &EndpointCaps) -> ProbeReport {
    ProbeReport {
        family: caps.family.as_str().into(),
        profile: caps.profile.as_str().into(),
        model: String::new(),
        base_url: String::new(),
        quant_label: caps.quant_label.clone(),
        enable_thinking: caps.enable_thinking,
        effort_values: caps.effort_values.clone(),
        preserve_thinking: caps.preserve_thinking.then_some(true),
        mtp: if caps.mtp { Some(true) } else { None },
        cached_tokens_field: caps.cached_tokens_field,
        prefill_tok_s: None,
        decode_tok_s: None,
        think_tokens_off: None,
        think_tokens_low: None,
        cross_turn_off_hit_pct: None,
        cross_turn_on_hit_pct: None,
        red: Vec::new(),
        yellow: Vec::new(),
        notes: Vec::new(),
        probed_at_unix: 0,
        supports_image: caps.supports_image,
        supports_video: caps.supports_video,
        supports_audio: caps.supports_audio,
        supports_transcription: caps.supports_transcription,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_strips_v1() {
        assert_eq!(
            engine_origin("http://127.0.0.1:8080/v1"),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn extract_think_block() {
        assert_eq!(
            extract_think("pre<think>\nabc\n</think>\npost").as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn llama_pin_uses_last_slot_when_parallel() {
        assert_eq!(
            llama_probe_pin(EngineProfile::LlamaCpp, Some(4)),
            (true, Some(3))
        );
        assert_eq!(
            llama_probe_pin(EngineProfile::LlamaCpp, Some(1)),
            (true, Some(0))
        );
        assert_eq!(llama_probe_pin(EngineProfile::Vllm, Some(8)), (false, None));
        assert_eq!(llama_probe_pin(EngineProfile::Sglang, None), (false, None));
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_qwen38_reference_box() {
        let (cfg, _) = Config::load_or_init().unwrap();
        let report = run_probe(&cfg).await.expect("probe");
        assert!(report.ok(), "probe red lights: {:?}", report.red);
        assert_eq!(report.family, "qwen38");
        assert!(report.enable_thinking);
        assert!(report.effort_values.iter().any(|v| v == "low"));
    }
}

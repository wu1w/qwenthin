//! `view` — load image / audio / video into the next model turn.
//!
//! Shape from QwenPaw `view_media.py`: return a media block + short text, never
//! a pixel dump. This llama.cpp box has vision and no native video/audio, so
//! video is 3 JPEG stills via `ffmpeg` and audio is a `whisper-cli` transcript.
//! HTTP image URLs pass through. Helpers are resolved on PATH (Windows / Linux /
//! macOS); the model must not `bash` to install them.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use super::media_exec::{
    extract_frames, no_hear_msg, no_watch_msg, transcribe_file, AUDIO_FETCH_MAX_BYTES,
    VIDEO_FETCH_MAX_BYTES,
};
use super::{arg_path, arg_str, Workspace};
use crate::media::{
    fallback_hint, is_http_url, kind_from_ext, kind_from_magic, mime_for, native_image_mime,
    path_ext, MediaBins, MediaCaps, MediaKind, MediaPart, MAX_INLINE_MEDIA_BYTES,
};
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};

pub async fn view(
    workspace: &Workspace,
    call: &ToolCall,
    caps: &MediaCaps,
    bins: &MediaBins,
    max_bytes: usize,
) -> ToolResponse {
    let Some(raw) = arg_path(&call.arguments).or_else(|| arg_str(&call.arguments, "image_path"))
    else {
        return ToolResponse::text(&call.id, "Error: No `path` provided.", ToolState::Error);
    };
    let raw = raw.trim().to_string();
    if raw.is_empty() {
        return ToolResponse::text(&call.id, "Error: No `path` provided.", ToolState::Error);
    }
    let hint = arg_str(&call.arguments, "kind")
        .as_deref()
        .and_then(MediaKind::parse);

    if is_http_url(&raw) {
        return view_url(&call.id, &raw, hint, caps, bins).await;
    }

    let path = match workspace.resolve(&raw) {
        Ok(p) => p,
        Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
    };
    if !path.is_file() {
        return ToolResponse::text(
            &call.id,
            format!("Error: {} does not exist or is not a file.", raw),
            ToolState::Error,
        );
    }

    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            return ToolResponse::text(
                &call.id,
                format!("Error: {} is not readable: {e}", raw),
                ToolState::Error,
            )
        }
    };
    let head = read_head(&path, 32);
    let kind = sniff_kind(&path, hint, head.as_deref()).unwrap_or(MediaKind::Image);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let ext = path_ext(&path.to_string_lossy());

    // Images stay under the inline cap. Video/audio helpers seek/decode from the
    // path — do not load a whole movie into memory.
    if kind == MediaKind::Image && meta.len() as usize > max_bytes {
        return ToolResponse::text(
            &call.id,
            format!(
                "Error: {name} is {} bytes and exceeds the {max_bytes}-byte media limit.",
                meta.len()
            ),
            ToolState::Error,
        );
    }

    match kind {
        MediaKind::Image => {
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    return ToolResponse::text(
                        &call.id,
                        format!("Error: failed to read {}: {e}", raw),
                        ToolState::Error,
                    )
                }
            };
            view_local_image(&call.id, &name, &ext, &bytes, caps)
        }
        MediaKind::Video => {
            view_local_video(&call.id, &path, &name, &ext, meta.len(), caps, bins).await
        }
        MediaKind::Audio => {
            view_local_audio(&call.id, &path, &name, &ext, meta.len(), caps, bins).await
        }
    }
}

fn sniff_kind(path: &Path, hint: Option<MediaKind>, head: Option<&[u8]>) -> Option<MediaKind> {
    if let Some(k) = hint {
        return Some(k);
    }
    kind_from_ext(&path_ext(&path.to_string_lossy())).or_else(|| head.and_then(kind_from_magic))
}

fn read_head(path: &Path, n: usize) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; n];
    let got = f.read(&mut buf).ok()?;
    buf.truncate(got);
    Some(buf)
}

async fn view_url(
    id: &str,
    url: &str,
    hint: Option<MediaKind>,
    caps: &MediaCaps,
    bins: &MediaBins,
) -> ToolResponse {
    let ext = path_ext(url);
    let kind = hint
        .or_else(|| kind_from_ext(&ext))
        .unwrap_or(MediaKind::Image);
    match kind {
        MediaKind::Image if caps.attach_image() => ok_media(
            id,
            format!("Image loaded from URL: {url}"),
            vec![MediaPart::image_url(url)],
        ),
        MediaKind::Video if caps.attach_video() => ok_media(
            id,
            format!("Video loaded from URL: {url}"),
            vec![MediaPart::video_url(url)],
        ),
        MediaKind::Video if caps.attach_image() => {
            match fetch_capped(url, VIDEO_FETCH_MAX_BYTES, caps).await {
                Ok((bytes, _)) => video_from_bytes(id, url, &ext, &bytes, bins).await,
                Err(e) => ToolResponse::text(
                    id,
                    no_watch_msg(url, &e.trim_start_matches("Error: ")),
                    ToolState::Success,
                ),
            }
        }
        MediaKind::Audio if caps.attach_audio() => {
            match fetch_capped(url, MAX_INLINE_MEDIA_BYTES, caps).await {
                Ok((bytes, mime)) => {
                    let ext = ext_from_mime(&mime, &ext);
                    ok_media(
                        id,
                        format!("Audio loaded from URL: {url}"),
                        vec![MediaPart::data_uri(
                            MediaKind::Audio,
                            mime_for(MediaKind::Audio, &ext, Some(&bytes)),
                            &bytes,
                        )],
                    )
                }
                Err(_) => transcribe_url(id, url, caps, bins).await,
            }
        }
        MediaKind::Audio => transcribe_url(id, url, caps, bins).await,
        other => ToolResponse::text(
            id,
            fallback_hint(other, url, missing_reason(other, caps)),
            ToolState::Success,
        ),
    }
}

fn view_local_image(
    id: &str,
    name: &str,
    ext: &str,
    bytes: &[u8],
    caps: &MediaCaps,
) -> ToolResponse {
    let mime = mime_for(MediaKind::Image, ext, Some(bytes));
    if !native_image_mime(mime) {
        return ToolResponse::text(
            id,
            format!(
                "Error: {name} uses unsupported image format {mime}. Convert to PNG/JPEG/WebP/GIF."
            ),
            ToolState::Error,
        );
    }
    if !caps.attach_image() {
        return ToolResponse::text(
            id,
            fallback_hint(
                MediaKind::Image,
                name,
                missing_reason(MediaKind::Image, caps),
            ),
            ToolState::Success,
        );
    }
    ok_media(
        id,
        format!("Image loaded: {name}"),
        vec![MediaPart::data_uri(MediaKind::Image, mime, bytes)],
    )
}

async fn view_local_video(
    id: &str,
    path: &Path,
    name: &str,
    ext: &str,
    size: u64,
    caps: &MediaCaps,
    bins: &MediaBins,
) -> ToolResponse {
    if caps.attach_video() {
        if size as usize > MAX_INLINE_MEDIA_BYTES {
            return ToolResponse::text(
                id,
                format!(
                    "Error: {name} exceeds the {}-byte inline video limit.",
                    MAX_INLINE_MEDIA_BYTES
                ),
                ToolState::Error,
            );
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                return ToolResponse::text(
                    id,
                    format!("Error: failed to read {name}: {e}"),
                    ToolState::Error,
                )
            }
        };
        let mime = mime_for(MediaKind::Video, ext, Some(&bytes));
        return ok_media(
            id,
            format!("Video loaded: {name}"),
            vec![MediaPart::data_uri(MediaKind::Video, mime, &bytes)],
        );
    }
    if caps.attach_image() {
        return video_stills(id, path, name, bins).await;
    }
    ToolResponse::text(
        id,
        fallback_hint(
            MediaKind::Video,
            name,
            missing_reason(MediaKind::Video, caps),
        ),
        ToolState::Success,
    )
}

async fn video_from_bytes(
    id: &str,
    name: &str,
    ext: &str,
    bytes: &[u8],
    bins: &MediaBins,
) -> ToolResponse {
    let tmp = std::env::temp_dir().join(format!(
        "q38-vid-{}.{}",
        uuid::Uuid::new_v4().simple(),
        if ext.is_empty() { "mp4" } else { ext }
    ));
    if let Err(e) = std::fs::write(&tmp, bytes) {
        return ToolResponse::text(
            id,
            no_watch_msg(name, &format!("could not buffer download ({e})")),
            ToolState::Success,
        );
    }
    let r = video_stills(id, &tmp, name, bins).await;
    let _ = std::fs::remove_file(&tmp);
    r
}

async fn video_stills(id: &str, path: &Path, name: &str, bins: &MediaBins) -> ToolResponse {
    match extract_frames(bins, path).await {
        Ok(sampled) => {
            let n = sampled.parts.len();
            ok_media(
                id,
                format!("Video {name}: {n} stills at {}.", sampled.label),
                sampled.parts,
            )
        }
        Err(e) => ToolResponse::text(id, no_watch_msg(name, &e), ToolState::Success),
    }
}

async fn view_local_audio(
    id: &str,
    path: &Path,
    name: &str,
    ext: &str,
    size: u64,
    caps: &MediaCaps,
    bins: &MediaBins,
) -> ToolResponse {
    if caps.attach_audio() && (size as usize) <= MAX_INLINE_MEDIA_BYTES {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                return ToolResponse::text(
                    id,
                    format!("Error: failed to read {name}: {e}"),
                    ToolState::Error,
                )
            }
        };
        let mime = mime_for(MediaKind::Audio, ext, Some(&bytes));
        return ok_media(
            id,
            format!("Audio loaded: {name}"),
            vec![MediaPart::data_uri(MediaKind::Audio, mime, &bytes)],
        );
    }
    transcribe_path(id, path, name, caps, bins).await
}

async fn transcribe_url(id: &str, url: &str, caps: &MediaCaps, bins: &MediaBins) -> ToolResponse {
    match fetch_capped(url, AUDIO_FETCH_MAX_BYTES, caps).await {
        Ok((bytes, mime)) => {
            let ext = ext_from_mime(&mime, &path_ext(url));
            let tmp = std::env::temp_dir().join(format!(
                "q38-aud-{}.{}",
                uuid::Uuid::new_v4().simple(),
                if ext.is_empty() { "wav" } else { ext.as_str() }
            ));
            if let Err(e) = std::fs::write(&tmp, bytes) {
                return ToolResponse::text(
                    id,
                    no_hear_msg(url, &format!("could not buffer download ({e})")),
                    ToolState::Success,
                );
            }
            let r = transcribe_path(id, &tmp, url, caps, bins).await;
            let _ = std::fs::remove_file(&tmp);
            r
        }
        Err(e) => ToolResponse::text(
            id,
            no_hear_msg(url, &e.trim_start_matches("Error: ")),
            ToolState::Success,
        ),
    }
}

async fn transcribe_path(
    id: &str,
    path: &Path,
    name: &str,
    caps: &MediaCaps,
    bins: &MediaBins,
) -> ToolResponse {
    if bins.whisper.is_some() {
        match transcribe_file(bins, path).await {
            Ok(text) => {
                let body = if text.is_empty() {
                    "(empty)".to_string()
                } else {
                    text
                };
                return ToolResponse::text(
                    id,
                    format!("Transcript of {name}:\n{body}"),
                    ToolState::Success,
                );
            }
            Err(e) => {
                if caps.try_transcribe() {
                    if let Some(r) = transcribe_http(id, name, path, caps).await {
                        return r;
                    }
                }
                return ToolResponse::text(id, no_hear_msg(name, &e), ToolState::Success);
            }
        }
    }
    if caps.try_transcribe() {
        if let Some(r) = transcribe_http(id, name, path, caps).await {
            return r;
        }
    }
    let why = if bins.whisper.is_none() {
        "whisper-cli is not on PATH (and /audio/transcriptions is unavailable)"
    } else {
        "transcription failed"
    };
    ToolResponse::text(id, no_hear_msg(name, why), ToolState::Success)
}

async fn transcribe_http(
    id: &str,
    name: &str,
    path: &Path,
    caps: &MediaCaps,
) -> Option<ToolResponse> {
    let (base, key) = caps.origin.clone()?;
    let bytes = std::fs::read(path).ok()?;
    let ext = path_ext(&path.to_string_lossy());
    let url = format!("{}/audio/transcriptions", base.trim_end_matches('/'));
    let mime = mime_for(MediaKind::Audio, &ext, Some(&bytes));
    let filename = if name.contains('/') || name.contains('\\') {
        format!("audio.{ext}")
    } else {
        name.to_string()
    };
    let part = match reqwest::multipart::Part::bytes(bytes.clone())
        .file_name(filename)
        .mime_str(mime)
    {
        Ok(p) => p,
        Err(_) => reqwest::multipart::Part::bytes(bytes).file_name("audio.wav"),
    };
    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .ok()?;
    let mut req = client.post(&url).multipart(form);
    if !key.is_empty() && key != "local" {
        req = req.bearer_auth(key);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return None;
            }
            let text = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or(body);
            Some(ToolResponse::text(
                id,
                format!("Transcript of {name}:\n{}", text.trim()),
                ToolState::Success,
            ))
        }
        Err(_) => None,
    }
}

async fn fetch_capped(
    url: &str,
    max_bytes: usize,
    caps: &MediaCaps,
) -> Result<(Vec<u8>, String), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Error: http client: {e}"))?;
    let mut req = client.get(url);
    if let Some((base, key)) = &caps.origin {
        if !key.is_empty() && key != "local" && url_is_same_host(url, base) {
            req = req.bearer_auth(key);
        }
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("Error: download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Error: download HTTP {}", resp.status()));
    }
    if let Some(len) = resp.content_length() {
        if len as usize > max_bytes {
            return Err(format!(
                "Error: remote media is {len} bytes and exceeds the {max_bytes}-byte limit."
            ));
        }
    }
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("Error: download body: {e}"))?;
    if bytes.len() > max_bytes {
        return Err(format!(
            "Error: remote media is {} bytes and exceeds the {max_bytes}-byte limit.",
            bytes.len()
        ));
    }
    Ok((bytes.to_vec(), mime))
}

fn url_is_same_host(url: &str, base: &str) -> bool {
    fn host(s: &str) -> Option<String> {
        reqwest::Url::parse(s)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
    }
    match (host(url), host(base)) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
        _ => false,
    }
}

fn ext_from_mime(mime: &str, fallback: &str) -> String {
    if mime.contains("wav") {
        "wav".into()
    } else if mime.contains("mpeg") || mime.contains("mp3") {
        "mp3".into()
    } else if mime.contains("mp4") || mime.contains("m4a") {
        "m4a".into()
    } else if !fallback.is_empty() {
        fallback.to_string()
    } else {
        "wav".into()
    }
}

fn missing_reason(kind: MediaKind, caps: &MediaCaps) -> &'static str {
    let flag = match kind {
        MediaKind::Image => caps.image,
        MediaKind::Video => caps.video,
        MediaKind::Audio => caps.audio,
    };
    match flag {
        Some(true) => "attach failed",
        Some(false) => "probe reported unsupported",
        None => "run `q38 probe` (capability unknown)",
    }
}

fn ok_media(id: &str, text: String, media: Vec<MediaPart>) -> ToolResponse {
    let mut r = ToolResponse::text(id, text, ToolState::Success);
    r.media = media;
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Workspace;
    use serde_json::json;
    use std::path::PathBuf;

    fn scratch() -> (Workspace, PathBuf) {
        let dir = std::env::temp_dir().join(format!("q38-view-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        (Workspace::open(&dir, true).unwrap(), dir)
    }

    fn call(path: &str) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: "view".into(),
            arguments: json!({"path": path}),
        }
    }

    async fn run(ws: &Workspace, path: &str, caps: &MediaCaps) -> ToolResponse {
        view(
            ws,
            &call(path),
            caps,
            &MediaBins::none(),
            MAX_INLINE_MEDIA_BYTES,
        )
        .await
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let (ws, dir) = scratch();
        let r = run(&ws, "nope.png", &MediaCaps::default()).await;
        assert_eq!(r.state, ToolState::Error);
        assert!(
            r.joined_text().contains("does not exist"),
            "{}",
            r.joined_text()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn size_cap_rejects() {
        let (ws, dir) = scratch();
        let big: Vec<u8> = vec![0u8; 64];
        std::fs::write(dir.join("a.png"), &big).unwrap();
        let r = view(
            &ws,
            &call("a.png"),
            &MediaCaps::default(),
            &MediaBins::none(),
            8,
        )
        .await;
        assert_eq!(r.state, ToolState::Error);
        assert!(r.joined_text().contains("exceeds"), "{}", r.joined_text());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn attaches_png_when_caps_allow() {
        let (ws, dir) = scratch();
        let png = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            crate::media::PROBE_IMAGE_B64,
        )
        .unwrap();
        std::fs::write(dir.join("red.png"), &png).unwrap();
        let mut caps = MediaCaps::default();
        caps.image = Some(true);
        let r = run(&ws, "red.png", &caps).await;
        assert_eq!(r.state, ToolState::Success);
        assert_eq!(r.media.len(), 1);
        assert!(r.media[0].url.starts_with("data:image/png;base64,"));
        assert!(r.joined_text().contains("Image loaded: red.png"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn no_attach_when_probe_says_no() {
        let (ws, dir) = scratch();
        let png = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            crate::media::PROBE_IMAGE_B64,
        )
        .unwrap();
        std::fs::write(dir.join("red.png"), &png).unwrap();
        let mut caps = MediaCaps::default();
        caps.image = Some(false);
        let r = run(&ws, "red.png", &caps).await;
        assert_eq!(r.state, ToolState::Success);
        assert!(r.media.is_empty());
        assert!(
            r.joined_text().contains("cannot perceive"),
            "{}",
            r.joined_text()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn http_image_passes_url_through() {
        let (ws, dir) = scratch();
        let mut caps = MediaCaps::default();
        caps.image = Some(true);
        let r = run(&ws, "https://example.com/cat.png", &caps).await;
        assert_eq!(r.media.len(), 1);
        assert_eq!(r.media[0].url, "https://example.com/cat.png");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn kind_dispatch_from_extension() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("clip.mp4"), b"not-really-mp4").unwrap();
        let mut caps = MediaCaps::default();
        caps.video = Some(false);
        caps.image = Some(false);
        let r = run(&ws, "clip.mp4", &caps).await;
        assert!(r.joined_text().contains("video"), "{}", r.joined_text());
        assert!(r.media.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn video_without_ffmpeg_does_not_suggest_bash() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("clip.mp4"), b"not-really-mp4").unwrap();
        let mut caps = MediaCaps::default();
        caps.video = Some(false);
        caps.image = Some(true);
        let r = run(&ws, "clip.mp4", &caps).await;
        let t = r.joined_text();
        assert!(t.contains("Cannot watch"), "{t}");
        assert!(!t.contains("Do not install"), "{t}");
        assert!(!t.to_ascii_lowercase().contains("brew"), "{t}");
        assert!(!t.contains("apt"), "{t}");
        assert!(r.media.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn audio_without_whisper_does_not_suggest_bash() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("speak.wav"), crate::media::silence_wav()).unwrap();
        let mut caps = MediaCaps::default();
        caps.audio = Some(false);
        caps.transcription = Some(false);
        let r = run(&ws, "speak.wav", &caps).await;
        let t = r.joined_text();
        assert!(t.contains("Cannot hear"), "{t}");
        assert!(!t.contains("Do not install"), "{t}");
        assert!(!t.to_ascii_lowercase().contains("brew"), "{t}");
        assert!(r.media.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn ffmpeg_samples_three_stills_when_present() {
        let bins = MediaBins::detect();
        let Some(ffmpeg) = bins.ffmpeg.clone() else {
            eprintln!("skip: ffmpeg not on PATH");
            return;
        };
        let (ws, dir) = scratch();
        let clip = dir.join("clip.mp4");
        super::super::media_exec::encode_color_mp4(&ffmpeg, &clip, "red", 1.0)
            .await
            .expect("lavfi red mp4");
        let mut caps = MediaCaps::default();
        caps.video = Some(false);
        caps.image = Some(true);
        let r = view(&ws, &call("clip.mp4"), &caps, &bins, MAX_INLINE_MEDIA_BYTES).await;
        assert_eq!(r.state, ToolState::Success, "{}", r.joined_text());
        assert_eq!(r.media.len(), 3, "{}", r.joined_text());
        assert!(r.media.iter().all(|p| p.kind == MediaKind::Image));
        assert!(r.joined_text().contains("stills"), "{}", r.joined_text());
        let _ = std::fs::remove_dir_all(dir);
    }
}

use serde::{Deserialize, Serialize};

use crate::family::EndpointCaps;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    Xhigh,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::Xhigh => "xhigh",
        }
    }

    pub fn from_config(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            // Official Qwen3.8 `medium` adds no steering sentence, so `auto`
            // is an honest user-facing alias: the model chooses the depth.
            "auto" | "medium" => Some(Self::Medium),
            "xhigh" | "high" => Some(Self::Xhigh),
            _ => None,
        }
    }
}

fn default_preserve() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThinkPolicy {
    pub enabled: bool,
    /// `None` when thinking is off — the request builder omits `reasoning_effort`.
    pub effort: Option<Effort>,
    pub max_think_tokens: u32,
    /// Official Qwen 3.6+ default. Missing JSONL fields must not silently strip.
    #[serde(default = "default_preserve")]
    pub preserve: bool,
    pub max_tokens: u32,
}

/// Token caps shared by CLI `--think`, slash `/think`, and mode defaults.
/// `default_effort` is never xhigh — that depth is explicit-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThinkBudget {
    pub max_tokens: u32,
    pub think_mode_max_tokens: u32,
    pub max_think_low: u32,
    pub max_think_medium: u32,
    pub max_think_xhigh: u32,
    pub default_effort: Effort,
}

impl Default for ThinkBudget {
    fn default() -> Self {
        Self {
            max_tokens: 8192,
            think_mode_max_tokens: 16384,
            max_think_low: 512,
            max_think_medium: 2048,
            max_think_xhigh: 4096,
            default_effort: Effort::Medium,
        }
    }
}

pub const XHIGH_WARN: &str = "q38: think xhigh — this turn may take 200s+";

/// Lossy overlay only. Official tables stay at 1.0 when the switch is off.
pub const LOSSY_REPEAT_PENALTY: f64 = 1.1;
/// Only applied when thinking is still the default low/512 and the user did not lock.
pub const LOSSY_THINK_CAP: u32 = 384;

impl ThinkPolicy {
    /// Template/unit-test fixture. Production paths use [`Self::effort_with`].
    pub fn agent_default() -> Self {
        Self {
            enabled: true,
            effort: Some(Effort::Low),
            max_think_tokens: 512,
            preserve: true,
            max_tokens: 2048,
        }
    }

    pub fn off() -> Self {
        Self {
            enabled: false,
            effort: None,
            max_think_tokens: 0,
            // Official Jinja still emits `<think>\n\n</think>` at the generation
            // prompt when thinking is off. Keep that wrapper on historical
            // assistants or the next turn is not a token prefix (llama.cpp,
            // vLLM APC, SGLang radix).
            preserve: true,
            max_tokens: 2048,
        }
    }

    pub fn think_mode() -> Self {
        Self {
            enabled: true,
            effort: Some(Effort::Medium),
            max_think_tokens: 2048,
            preserve: true,
            max_tokens: 4096,
        }
    }

    pub fn off_with(b: &ThinkBudget) -> Self {
        let mut p = Self::off();
        p.max_tokens = b.max_tokens;
        p
    }

    /// Thinking on, `medium` so official Jinja injects no effort sentence.
    /// The model picks depth. `max_think_tokens` is a runaway cap only.
    /// Honor `default_effort` for that cap: Low → `max_think_low`; Medium/Xhigh
    /// keep `max_think_xhigh`. Never put Low/Xhigh in `effort` here.
    pub fn native_with(b: &ThinkBudget) -> Self {
        let max_think_tokens = match b.default_effort {
            Effort::Low => b.max_think_low,
            Effort::Medium | Effort::Xhigh => b.max_think_xhigh,
        };
        Self {
            enabled: true,
            effort: Some(Effort::Medium),
            max_think_tokens,
            preserve: true,
            max_tokens: b.max_tokens,
        }
    }

    /// `/think LEVEL` and `--think LEVEL`. Historical thinking is preserved.
    /// xhigh uses `think_mode_max_tokens` so generation is not clipped at 8k.
    pub fn effort_with(b: &ThinkBudget, effort: Effort) -> Self {
        let (max_think_tokens, max_tokens) = match effort {
            Effort::Low => (b.max_think_low, b.max_tokens),
            Effort::Medium => (b.max_think_medium, b.max_tokens),
            Effort::Xhigh => (b.max_think_xhigh, b.think_mode_max_tokens),
        };
        Self {
            enabled: true,
            effort: Some(effort),
            max_think_tokens,
            preserve: true,
            max_tokens,
        }
    }

    /// `/mode think` / `--mode think`: medium, preserve=true, long generation.
    pub fn think_mode_with(b: &ThinkBudget) -> Self {
        let mut p = Self::effort_with(b, Effort::Medium);
        p.preserve = true;
        p.max_tokens = b.think_mode_max_tokens;
        p
    }

    /// CLI `--fast` / `--think` / `--mode`. `--fast` wins.
    /// `--think` overlays effort even when `--mode think` (preserve stays true).
    /// Config `default_effort=xhigh` is ignored; xhigh is explicit-only.
    pub fn from_cli(b: &ThinkBudget, fast: bool, think: Option<&str>, mode: Option<&str>) -> Self {
        if fast {
            return Self::off_with(b);
        }
        let think_mode = mode.is_some_and(|m| m.eq_ignore_ascii_case("think"));
        let user_effort = think.and_then(Effort::from_config);
        if think_mode && user_effort.is_none() {
            return Self::think_mode_with(b);
        }
        let Some(effort) = user_effort else {
            // No `--think` / `/think`: do not inject a low/xhigh lecture.
            return Self::native_with(b);
        };
        let mut p = Self::effort_with(b, effort);
        if think_mode {
            p.preserve = true;
            p.max_tokens = b.think_mode_max_tokens;
        }
        p
    }

    pub fn sampling(&self) -> Sampling {
        if self.enabled {
            Sampling::thinking()
        } else {
            Sampling::instruct()
        }
    }

    /// Low-precision overlay. `/think` / `--fast` (user lock) and think-mode are untouched.
    pub fn apply_lossy_think_cap(mut self, locked: bool) -> Self {
        if locked || !self.enabled {
            return self;
        }
        if self.effort == Some(Effort::Low) && self.max_think_tokens == 512 {
            self.max_think_tokens = LOSSY_THINK_CAP;
        }
        self
    }

    /// Official agent path: keep historical `<think>` so later hops continue
    /// instead of restating the user turn. Compact clips old think; do not
    /// strip via this flag.
    pub fn with_preserved_thinking(mut self) -> Self {
        self.preserve = true;
        self
    }

    /// Kwargs that this endpoint's template actually understands. Unknown keys stay omitted.
    pub fn template_kwargs(&self, caps: &EndpointCaps) -> TemplateKwargs {
        let enable_thinking = Some(self.enabled);
        let reasoning_effort = if !self.enabled {
            None
        } else {
            self.effort.and_then(|e| {
                let s = e.as_str();
                caps.effort_values
                    .iter()
                    .any(|v| v == s)
                    .then(|| s.to_string())
            })
        };
        // Always `true` on the wire when the family supports the kwarg.
        // `preserve: false` only exists for local Jinja tests; sending it on
        // an agent loop makes every new user turn restate the task.
        let preserve_thinking = if caps.preserve_thinking {
            Some(true)
        } else {
            None
        };
        TemplateKwargs {
            enable_thinking,
            reasoning_effort,
            preserve_thinking,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TemplateKwargs {
    pub enable_thinking: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub preserve_thinking: Option<bool>,
}

impl TemplateKwargs {
    pub fn is_empty(&self) -> bool {
        self.enable_thinking.is_none()
            && self.reasoning_effort.is_none()
            && self.preserve_thinking.is_none()
    }

    pub fn to_json_object(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        if let Some(v) = self.enable_thinking {
            m.insert("enable_thinking".into(), serde_json::Value::Bool(v));
        }
        if let Some(ref v) = self.reasoning_effort {
            m.insert(
                "reasoning_effort".into(),
                serde_json::Value::String(v.clone()),
            );
        }
        if let Some(v) = self.preserve_thinking {
            m.insert("preserve_thinking".into(), serde_json::Value::Bool(v));
        }
        m
    }
}

/// Official Qwen sampling tables. Missing fields fall through to server defaults — send them all.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sampling {
    pub temperature: f64,
    pub top_p: f64,
    pub top_k: i64,
    pub min_p: f64,
    pub presence_penalty: f64,
    pub repetition_penalty: f64,
}

/// Same-session depth controller. Never auto-selects xhigh. A user lock
/// (`--think` / `--mode think` / `--fast` / slash) blocks auto upgrade.
/// After an auto-upgrade, the next clean step drops back to the turn
/// baseline (one prefix miss up, one miss down — matching the design table).
#[derive(Clone, Debug)]
pub struct EffortController {
    pub user_locked: bool,
    policy: ThinkPolicy,
    baseline: ThinkPolicy,
    auto_upgraded: bool,
    parse_fails: u32,
    harness_fails: u32,
    test_fails: u32,
    parse_upgrade_after: u32,
}

impl EffortController {
    pub fn new(policy: ThinkPolicy, user_locked: bool) -> Self {
        Self {
            user_locked,
            policy: policy.clone(),
            baseline: policy,
            auto_upgraded: false,
            parse_fails: 0,
            harness_fails: 0,
            test_fails: 0,
            parse_upgrade_after: 2,
        }
    }

    pub fn with_parse_upgrade_after(mut self, n: u32) -> Self {
        self.parse_upgrade_after = n.max(1);
        self
    }

    pub fn policy(&self) -> &ThinkPolicy {
        &self.policy
    }

    pub fn auto_upgraded(&self) -> bool {
        self.auto_upgraded
    }

    /// Default: first parse fail stays put; second upgrades to medium/2048.
    /// Low-precision overlay upgrades on the first fail.
    pub fn note_parse_fail(&mut self) {
        self.parse_fails = self.parse_fails.saturating_add(1);
        self.harness_fails = 0;
        if self.parse_fails >= self.parse_upgrade_after {
            self.upgrade_medium();
        }
    }

    /// Two consecutive coordinator/internal failures → medium. Tool-level
    /// errors (bash nonzero, unknown tool, test-red) are not harness fails.
    pub fn note_harness_fail(&mut self) -> bool {
        self.harness_fails = self.harness_fails.saturating_add(1);
        if self.harness_fails >= 2 {
            let before = self.auto_upgraded;
            self.upgrade_medium();
            return self.auto_upgraded && !before;
        }
        false
    }

    /// Two consecutive test-runner failures (model or harness oracle) bump
    /// Low→Medium. Never auto-xhigh. Already-Medium stays put.
    pub fn note_test_fail(&mut self) -> bool {
        self.test_fails = self.test_fails.saturating_add(1);
        if self.test_fails < 2 {
            return false;
        }
        let before = self.auto_upgraded;
        self.bump_effort();
        self.auto_upgraded && !before
    }

    /// Edit thrash (mutually reverting edits on one path) is a judgment
    /// failure, not an execution failure: upgrade immediately so the very
    /// next turn plans at medium. Same decay as the other upgrades — one
    /// clean step drops back to baseline.
    pub fn note_thrash(&mut self) -> bool {
        let before = self.auto_upgraded;
        self.upgrade_medium();
        self.auto_upgraded && !before
    }

    /// Reset parse/harness streaks. Test-fail streak survives a clean edit
    /// so red→edit→red still counts. If this turn auto-upgraded, drop back
    /// to the baseline policy. Returns true when kwargs must be synced.
    pub fn note_clean_step(&mut self) -> bool {
        self.parse_fails = 0;
        self.harness_fails = 0;
        if self.user_locked || !self.auto_upgraded {
            return false;
        }
        self.auto_upgraded = false;
        if self.policy == self.baseline {
            return false;
        }
        self.policy = self.baseline.clone();
        true
    }

    /// Green tests reset the consecutive-red streak. Does not change effort.
    pub fn note_tests_green(&mut self) {
        self.test_fails = 0;
    }

    fn bump_effort(&mut self) {
        if self.user_locked {
            return;
        }
        match self.policy.effort {
            Some(Effort::Xhigh) | Some(Effort::Medium) => {}
            Some(Effort::Low) | None => self.upgrade_medium(),
        }
    }

    fn upgrade_medium(&mut self) {
        if self.user_locked {
            return;
        }
        if self.policy.effort == Some(Effort::Xhigh) {
            return;
        }
        let already_medium = self.policy.enabled
            && self.policy.effort == Some(Effort::Medium)
            && self.policy.max_think_tokens >= 2048;
        if already_medium {
            return;
        }
        let preserve = self.policy.preserve;
        let max_tokens = if self.policy.max_tokens >= 8192 {
            self.policy.max_tokens
        } else {
            self.policy.max_tokens.max(4096)
        };
        self.policy.enabled = true;
        self.policy.effort = Some(Effort::Medium);
        self.policy.max_think_tokens = 2048;
        self.policy.preserve = preserve;
        self.policy.max_tokens = max_tokens;
        self.auto_upgraded = true;
    }
}

impl Sampling {
    pub fn thinking() -> Self {
        Self {
            temperature: 1.0,
            top_p: 0.95,
            top_k: 20,
            min_p: 0.0,
            presence_penalty: 0.0,
            repetition_penalty: 1.0,
        }
    }

    pub fn instruct() -> Self {
        Self {
            temperature: 0.7,
            top_p: 0.80,
            top_k: 20,
            min_p: 0.0,
            presence_penalty: 1.5,
            repetition_penalty: 1.0,
        }
    }

    pub fn with_lossy_repeat(mut self) -> Self {
        self.repetition_penalty = self.repetition_penalty.max(LOSSY_REPEAT_PENALTY);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::family::EndpointCaps;

    #[test]
    fn off_omits_effort() {
        let caps = EndpointCaps::qwen38_llamacpp();
        let kw = ThinkPolicy::off().template_kwargs(&caps);
        assert_eq!(kw.enable_thinking, Some(false));
        assert!(kw.reasoning_effort.is_none());
        assert_eq!(kw.preserve_thinking, Some(true));
    }

    #[test]
    fn wire_kwargs_keep_think_even_if_policy_field_is_false() {
        let caps = EndpointCaps::qwen38_llamacpp();
        let mut p = ThinkPolicy::native_with(&ThinkBudget::default());
        p.preserve = false;
        let kw = p.template_kwargs(&caps);
        assert_eq!(kw.preserve_thinking, Some(true));
        assert_eq!(kw.reasoning_effort.as_deref(), Some("medium"));
        let missing: ThinkPolicy = serde_json::from_str(
            r#"{"enabled":true,"effort":"medium","max_think_tokens":4096,"max_tokens":8192}"#,
        )
        .unwrap();
        assert!(missing.preserve);
    }

    #[test]
    fn qwen35_omits_effort_even_when_low() {
        let caps = EndpointCaps::for_family(
            crate::family::Family::Qwen35,
            crate::family::EngineProfile::Generic,
        );
        let kw = ThinkPolicy::agent_default().template_kwargs(&caps);
        assert_eq!(kw.enable_thinking, Some(true));
        assert!(kw.reasoning_effort.is_none());
        assert!(kw.preserve_thinking.is_none());
    }

    #[test]
    fn first_parse_stays_low() {
        let mut c = EffortController::new(ThinkPolicy::agent_default(), false);
        c.note_parse_fail();
        assert_eq!(c.policy().effort, Some(Effort::Low));
        assert_eq!(c.policy().max_think_tokens, 512);
        assert!(!c.auto_upgraded());
    }

    #[test]
    fn lossy_first_parse_upgrades() {
        let mut c =
            EffortController::new(ThinkPolicy::agent_default(), false).with_parse_upgrade_after(1);
        c.note_parse_fail();
        assert_eq!(c.policy().effort, Some(Effort::Medium));
        assert_eq!(c.policy().max_think_tokens, 2048);
        assert!(c.auto_upgraded());
    }

    #[test]
    fn lossy_think_cap_only_default_low() {
        let p = ThinkPolicy::agent_default().apply_lossy_think_cap(false);
        assert_eq!(p.max_think_tokens, LOSSY_THINK_CAP);
        let locked = ThinkPolicy::agent_default().apply_lossy_think_cap(true);
        assert_eq!(locked.max_think_tokens, 512);
        let native = ThinkPolicy::native_with(&ThinkBudget::default()).apply_lossy_think_cap(false);
        assert_eq!(native.max_think_tokens, 4096);
        let off = ThinkPolicy::off().apply_lossy_think_cap(false);
        assert_eq!(off.max_think_tokens, 0);
    }

    #[test]
    fn second_parse_upgrades_medium() {
        let mut c = EffortController::new(ThinkPolicy::agent_default(), false);
        c.note_parse_fail();
        c.note_parse_fail();
        assert_eq!(c.policy().effort, Some(Effort::Medium));
        assert_eq!(c.policy().max_think_tokens, 2048);
        assert!(c.policy().enabled);
        assert!(c.policy().preserve);
        assert_eq!(c.policy().max_tokens, 4096);
        assert_ne!(c.policy().effort, Some(Effort::Xhigh));
        assert!(c.auto_upgraded());
    }

    #[test]
    fn upgrade_keeps_8192_max_tokens() {
        let mut p = ThinkPolicy::agent_default();
        p.max_tokens = 8192;
        let mut c = EffortController::new(p, false);
        c.note_parse_fail();
        c.note_parse_fail();
        assert_eq!(c.policy().max_tokens, 8192);
        assert!(c.policy().preserve);
    }

    #[test]
    fn thrash_upgrades_immediately_and_decays_on_clean() {
        let mut c = EffortController::new(ThinkPolicy::agent_default(), false);
        assert!(c.note_thrash());
        assert_eq!(c.policy().effort, Some(Effort::Medium));
        assert_eq!(c.policy().max_think_tokens, 2048);
        assert!(!c.note_thrash(), "already upgraded: no re-sync needed");
        assert!(c.note_clean_step());
        assert_eq!(c.policy().effort, Some(Effort::Low));
    }

    #[test]
    fn thrash_respects_user_lock() {
        let mut c = EffortController::new(ThinkPolicy::agent_default(), true);
        assert!(!c.note_thrash());
        assert_eq!(c.policy().effort, Some(Effort::Low));
        assert_eq!(c.policy().max_think_tokens, 512);
    }

    #[test]
    fn two_harness_fails_upgrade() {
        let mut c = EffortController::new(ThinkPolicy::agent_default(), false);
        assert!(!c.note_harness_fail());
        assert_eq!(c.policy().effort, Some(Effort::Low));
        assert!(c.note_harness_fail());
        assert_eq!(c.policy().effort, Some(Effort::Medium));
        assert_eq!(c.policy().max_think_tokens, 2048);
    }

    #[test]
    fn harness_streak_resets_on_clean() {
        let mut c = EffortController::new(ThinkPolicy::agent_default(), false);
        c.note_harness_fail();
        c.note_clean_step();
        c.note_harness_fail();
        assert_eq!(c.policy().effort, Some(Effort::Low));
    }

    #[test]
    fn clean_step_drops_back_to_baseline() {
        let mut c = EffortController::new(ThinkPolicy::agent_default(), false);
        c.note_parse_fail();
        c.note_parse_fail();
        assert!(c.note_clean_step());
        assert_eq!(c.policy().effort, Some(Effort::Low));
        assert_eq!(c.policy().max_think_tokens, 512);
        assert_eq!(c.policy().max_tokens, 2048);
        assert!(!c.auto_upgraded());
        // A later pair of parse fails can upgrade again.
        c.note_parse_fail();
        c.note_parse_fail();
        assert_eq!(c.policy().effort, Some(Effort::Medium));
        assert!(c.auto_upgraded());
    }

    #[test]
    fn parse_then_clean_then_parse_does_not_upgrade() {
        let mut c = EffortController::new(ThinkPolicy::agent_default(), false);
        c.note_parse_fail();
        c.note_clean_step();
        c.note_parse_fail();
        assert_eq!(c.policy().effort, Some(Effort::Low));
        assert!(!c.auto_upgraded());
    }

    #[test]
    fn from_cli_matches_slash_caps() {
        let b = ThinkBudget::default();
        let xhigh = ThinkPolicy::from_cli(&b, false, Some("xhigh"), None);
        assert_eq!(xhigh, ThinkPolicy::effort_with(&b, Effort::Xhigh));
        assert_eq!(xhigh.max_think_tokens, 4096);
        assert_eq!(xhigh.max_tokens, 16384);
        assert!(xhigh.preserve);

        let fast = ThinkPolicy::from_cli(&b, true, Some("xhigh"), Some("think"));
        assert!(!fast.enabled);
        assert!(fast.effort.is_none());
        assert_eq!(fast.max_tokens, 8192);
        assert!(fast.preserve);

        let mode = ThinkPolicy::from_cli(&b, false, None, Some("think"));
        assert_eq!(mode, ThinkPolicy::think_mode_with(&b));
        assert!(mode.preserve);
        assert_eq!(mode.effort, Some(Effort::Medium));
        assert_eq!(mode.max_tokens, 16384);

        let overlay = ThinkPolicy::from_cli(&b, false, Some("xhigh"), Some("think"));
        assert_eq!(overlay.effort, Some(Effort::Xhigh));
        assert!(overlay.preserve);
        assert_eq!(overlay.max_think_tokens, 4096);
        assert_eq!(overlay.max_tokens, 16384);

        let mut xhigh_cfg = b;
        xhigh_cfg.default_effort = Effort::Xhigh;
        let clamped = ThinkPolicy::from_cli(&xhigh_cfg, false, None, None);
        assert_eq!(clamped.effort, Some(Effort::Medium));
        assert_eq!(clamped.max_think_tokens, 4096);
        assert_eq!(clamped.max_tokens, 8192);
        assert!(clamped.preserve);
    }

    #[test]
    fn native_uses_medium_without_steering_cap() {
        let b = ThinkBudget::default();
        let p = ThinkPolicy::native_with(&b);
        assert!(p.enabled);
        assert_eq!(p.effort, Some(Effort::Medium));
        assert_eq!(p.max_think_tokens, 4096);
        assert_eq!(p.max_tokens, 8192);
        assert!(p.preserve);
    }

    #[test]
    fn native_low_default_caps_think_keeps_medium() {
        let mut b = ThinkBudget::default();
        b.default_effort = Effort::Low;
        let p = ThinkPolicy::native_with(&b);
        assert!(p.enabled);
        assert_eq!(p.effort, Some(Effort::Medium));
        assert_ne!(p.effort, Some(Effort::Low));
        assert_eq!(p.max_think_tokens, b.max_think_low);
        assert!(p.preserve);
    }

    #[test]
    fn user_lock_blocks_upgrade_and_downgrade() {
        let mut low = EffortController::new(ThinkPolicy::agent_default(), true);
        low.note_parse_fail();
        low.note_parse_fail();
        assert_eq!(low.policy().effort, Some(Effort::Low));
        assert_eq!(low.policy().max_think_tokens, 512);

        let mut think = EffortController::new(ThinkPolicy::think_mode(), true);
        think.note_parse_fail();
        think.note_parse_fail();
        think.note_clean_step();
        assert_eq!(think.policy().effort, Some(Effort::Medium));
        assert!(think.policy().preserve);
        assert_eq!(think.policy().max_think_tokens, 2048);

        let mut fast = EffortController::new(ThinkPolicy::off(), true);
        fast.note_parse_fail();
        fast.note_parse_fail();
        assert!(!fast.policy().enabled);
        assert!(fast.policy().effort.is_none());
    }

    #[test]
    fn never_auto_xhigh() {
        let mut c = EffortController::new(ThinkPolicy::agent_default(), false);
        c.note_parse_fail();
        c.note_parse_fail();
        c.note_harness_fail();
        c.note_harness_fail();
        assert_ne!(c.policy().effort, Some(Effort::Xhigh));
        assert_eq!(c.policy().effort, Some(Effort::Medium));
    }

    #[test]
    fn two_test_fails_survive_clean_edit() {
        let mut c = EffortController::new(ThinkPolicy::agent_default(), false);
        assert!(!c.note_test_fail());
        c.note_clean_step();
        assert!(
            c.note_test_fail(),
            "second consecutive red must survive a clean edit"
        );
        assert_eq!(c.policy().effort, Some(Effort::Medium));
        c.note_tests_green();
        assert!(!c.note_test_fail(), "green tests reset the streak");
        assert_eq!(c.policy().effort, Some(Effort::Medium));
    }

    #[test]
    fn two_test_fails_bump_low_to_medium_not_xhigh() {
        let mut c = EffortController::new(ThinkPolicy::agent_default(), false);
        assert!(!c.note_test_fail());
        assert_eq!(c.policy().effort, Some(Effort::Low));
        assert!(c.note_test_fail());
        assert_eq!(c.policy().effort, Some(Effort::Medium));
        c.note_test_fail();
        c.note_test_fail();
        assert_eq!(c.policy().effort, Some(Effort::Medium));
        assert_ne!(c.policy().effort, Some(Effort::Xhigh));
    }
}

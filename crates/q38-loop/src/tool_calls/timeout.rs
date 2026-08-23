use super::types::Deadlines;

/// Foreground offload window as a fraction of the resolved tool timeout.
pub const OFFLOAD_TIMEOUT_RATIO: f64 = 0.5;
/// After offload, ensure at least this much kill budget remains.
pub const MIN_BACKGROUND_WINDOW_SECS: f64 = 30.0;
/// Hard ceiling when the coordinator owns kill (shell-class tools).
pub const COORDINATOR_OWNED_EXEC_TIMEOUT_SECS: f64 = 24.0 * 3600.0;

pub fn arm_kill_deadline(deadlines: &mut Deadlines, secs: f64, only_if_unset: bool) -> bool {
    if only_if_unset && deadlines.kill_at.is_some() {
        return true;
    }
    if secs < 0.0 {
        return false;
    }
    let now = tokio::time::Instant::now();
    let desired_kill = now + secs_to_dur(secs);
    deadlines.kill_at = Some(desired_kill);
    if let Some(offload) = deadlines.offload_at {
        if offload >= desired_kill {
            let mut pulled = now + secs_to_dur((secs * OFFLOAD_TIMEOUT_RATIO).max(0.0));
            if pulled >= desired_kill && secs > 0.0 {
                let pullback = secs_to_dur((secs / 2.0).min(0.001));
                pulled = now
                    + desired_kill
                        .saturating_duration_since(now)
                        .saturating_sub(pullback);
            }
            deadlines.offload_at = Some(pulled);
        }
    }
    true
}

pub fn effective_timeout(
    default_secs: f64,
    remaining_kill: Option<std::time::Duration>,
    max_amplify: f64,
) -> f64 {
    match remaining_kill {
        None => default_secs,
        Some(rem) => rem.as_secs_f64().min(default_secs * max_amplify).max(0.0),
    }
}

pub fn secs_to_dur(secs: f64) -> std::time::Duration {
    std::time::Duration::from_secs_f64(secs.max(0.0))
}

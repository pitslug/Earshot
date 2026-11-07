//! Segmental echo return loss enhancement (ERLE), PLAN.md 4.0.
//!
//! `ERLE = 10 log10(P_mic / P_out)` over consecutive non-overlapping 1 s windows; the
//! reported value is the median over windows that start at t >= 2 s (convergence time
//! excluded). Scored on noise-free far-end-only scenarios, where the mic signal is pure
//! echo and the canceller output is residual echo.

/// Window and warm-up settings. [`Default`] is the PLAN.md 4.0 definition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErleConfig {
    /// Window length in seconds.
    pub window_s: f64,
    /// Windows starting before this time (seconds) are not scored.
    pub skip_s: f64,
}

impl Default for ErleConfig {
    fn default() -> Self {
        ErleConfig {
            window_s: 1.0,
            skip_s: 2.0,
        }
    }
}

/// Per-window values and their median.
#[derive(Debug, Clone, PartialEq)]
pub struct ErleResult {
    /// ERLE in dB of each scored window, in time order. A window whose output is exactly
    /// silent scores `+inf`.
    pub windows_db: Vec<f64>,
    /// Median of `windows_db` (mean of the middle two for an even count).
    pub median_db: f64,
}

/// Segmental ERLE of canceller output `out` against microphone signal `mic`, both at
/// `sample_rate`. A trailing partial window is dropped; windows where the mic is silent
/// are skipped. Returns `None` for mismatched lengths, a zero sample rate, or when no
/// window is scored.
pub fn segmental_erle(
    mic: &[f32],
    out: &[f32],
    sample_rate: u32,
    cfg: &ErleConfig,
) -> Option<ErleResult> {
    if mic.len() != out.len() || sample_rate == 0 {
        return None;
    }
    let win = (cfg.window_s * f64::from(sample_rate)).round() as usize;
    let skip = (cfg.skip_s * f64::from(sample_rate)).round() as usize;
    if win == 0 {
        return None;
    }
    let power = |x: &[f32]| x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>();
    let mut windows_db = Vec::new();
    let mut start = skip;
    while start + win <= mic.len() {
        let p_mic = power(&mic[start..start + win]);
        let p_out = power(&out[start..start + win]);
        if p_mic > 0.0 {
            windows_db.push(if p_out > 0.0 {
                10.0 * (p_mic / p_out).log10()
            } else {
                f64::INFINITY
            });
        }
        start += win;
    }
    if windows_db.is_empty() {
        return None;
    }
    let mut sorted = windows_db.clone();
    sorted.sort_by(f64::total_cmp);
    let k = sorted.len();
    let median_db = if k % 2 == 1 {
        sorted[k / 2]
    } else {
        (sorted[k / 2 - 1] + sorted[k / 2]) / 2.0
    };
    Some(ErleResult {
        windows_db,
        median_db,
    })
}

/// ERLE level that `aec.convergence` and `aec.path_change` wait for (PLAN.md 4.2).
pub const CONVERGED_ERLE_DB: f64 = 15.0;

/// Settings for [`time_to_erle`] (`aec.convergence`, `aec.path_change`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConvergenceConfig {
    /// Short-time ERLE window (s).
    pub window_s: f64,
    /// Hop between windows (s).
    pub hop_s: f64,
    /// After the first window that reaches the target, the median ERLE of the windows
    /// starting within this span (s) must also reach it, so a single lucky window does
    /// not count as converged.
    pub hold_s: f64,
    /// Windows whose mic mean-square power is below this are not scored (no echo).
    pub mic_floor: f64,
}

impl Default for ConvergenceConfig {
    fn default() -> Self {
        Self {
            window_s: 0.5,
            hop_s: 0.1,
            hold_s: 1.0,
            mic_floor: 1e-6,
        }
    }
}

/// Time (s, measured from `start_s`) until the canceller reaches `target_db` ERLE.
///
/// Windows of `window_s` advance by `hop_s` from `start_s`; each scored window's ERLE is
/// `10 log10(P_mic / P_out)`. The result is the end time of the first scored window with
/// ERLE >= `target_db` whose following scored windows (those starting within `hold_s`
/// after it, the window itself excluded) have median ERLE >= `target_db`. `None` if that never happens, on mismatched lengths or a zero rate.
#[must_use]
pub fn time_to_erle(
    mic: &[f32],
    out: &[f32],
    sample_rate: u32,
    start_s: f64,
    target_db: f64,
    cfg: &ConvergenceConfig,
) -> Option<f64> {
    if mic.len() != out.len() || sample_rate == 0 {
        return None;
    }
    let fs = f64::from(sample_rate);
    let win = (cfg.window_s * fs).round() as usize;
    let hop = (cfg.hop_s * fs).round() as usize;
    if win == 0 || hop == 0 {
        return None;
    }
    let mean_sq =
        |x: &[f32]| x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len() as f64;
    // (window start sample, ERLE dB) for scored windows.
    let mut scored = Vec::new();
    let mut s = (start_s * fs).round() as usize;
    while s + win <= mic.len() {
        let pm = mean_sq(&mic[s..s + win]);
        if pm >= cfg.mic_floor {
            let po = mean_sq(&out[s..s + win]);
            let e = if po > 0.0 {
                10.0 * (pm / po).log10()
            } else {
                f64::INFINITY
            };
            scored.push((s, e));
        }
        s += hop;
    }
    let hold = (cfg.hold_s * fs).round() as usize;
    for (i, &(s0, e)) in scored.iter().enumerate() {
        if e < target_db {
            continue;
        }
        // The hold: the windows after the crossing, starting within `hold_s` (the crossing
        // window itself is excluded: it reached the target by definition).
        let mut next: Vec<f64> = scored[i + 1..]
            .iter()
            .take_while(|w| w.0 <= s0 + hold)
            .map(|w| w.1)
            .collect();
        if next.is_empty() {
            continue;
        }
        next.sort_by(f64::total_cmp);
        let k = next.len();
        let median = if k % 2 == 1 {
            next[k / 2]
        } else {
            (next[k / 2 - 1] + next[k / 2]) / 2.0
        };
        if median >= target_db {
            return Some((s0 + win) as f64 / fs - start_s);
        }
    }
    None
}

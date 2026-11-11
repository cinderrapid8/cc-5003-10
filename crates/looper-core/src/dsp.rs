use crate::error::{LooperError, Result};
use crate::model::{AudioBuffer, FadeCurve};

/// Discontinuity measurements at one candidate loop boundary. The boundary
/// joins frame `end - 1` (last played frame) back to frame `start` (first).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SeamMetrics {
    /// Absolute sample jump at the seam, averaged over channels (linear PCM).
    pub amplitude_jump: f32,
    /// Per-channel absolute sample jump at the seam.
    pub amplitude_jump_per_channel: Vec<f32>,
    /// Difference of the short-window spectrum across the seam (0..~1,
    /// lower is better). Captures timbre breaks that level matching misses.
    pub spectral_discontinuity: f32,
    /// True when the joining samples straddle (or touch) zero on every
    /// channel. A candidate flag only — never sufficient on its own.
    pub zero_crossing: bool,
}

/// One candidate end index together with everything measured there.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct LoopCandidate {
    pub end_frame: usize,
    pub metrics: SeamMetrics,
    /// Composite 0..1 ranking score, lower is better.
    pub score: f32,
}

/// Exactly which frames the crossfade occupied and how long the finished
/// loop is. Every number is a frame index into the decoded source so the
/// whole operation can be recomputed and audited.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct FadePlan {
    pub fade_frames: usize,
    /// [head_lo, head_hi): fade-in zone at the beginning of the loop.
    pub head_lo: usize,
    pub head_hi: usize,
    /// [tail_lo, tail_hi): fade-out zone at the end of the loop.
    pub tail_lo: usize,
    pub tail_hi: usize,
    /// Frames between head_hi and tail_lo kept fully unblended.
    pub unblended_frames: usize,
    pub curve: FadeCurve,
    /// Final loop length in frames.
    pub loop_length_frames: usize,
}

/// Mean spectral energy per bin of a short Hann window starting at `offset`.
fn short_spectrum(buffer: &AudioBuffer, offset: usize, bins: usize) -> Vec<f32> {
    let channels = buffer.channels as usize;
    let mut energy = vec![0f32; bins];
    let mut total = 0f32;
    for i in 0..bins {
        let frame = offset + i;
        if frame >= buffer.frames() {
            break;
        }
        let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / bins as f32).cos();
        for ch in 0..channels {
            let v = buffer.sample(frame, ch) * w;
            energy[i] += v * v;
        }
        total += energy[i];
    }
    if total > 0.0 {
        for e in energy.iter_mut() {
            *e /= total;
        }
    }
    energy
}

/// L1 distance of two normalized spectra — a cheap, allocation-light proxy
/// for spectral flux across the seam.
fn spectral_distance(buffer: &AudioBuffer, end: usize, start: usize) -> f32 {
    let bins = 64.min(buffer.frames() / 4).max(8);
    let before_lo = end.saturating_sub(bins);
    let after_lo = start;
    let a = short_spectrum(buffer, before_lo, bins);
    let b = short_spectrum(buffer, after_lo, bins);
    a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum::<f32>() / bins as f32
}

pub fn measure_seam(buffer: &AudioBuffer, start: usize, end: usize) -> SeamMetrics {
    let channels = buffer.channels as usize;
    let mut per_channel = Vec::with_capacity(channels);
    let mut zero = true;
    for ch in 0..channels {
        let last = buffer.sample(end - 1, ch);
        let first = buffer.sample(start, ch);
        per_channel.push((first - last).abs());
        if !(last * first <= 0.0) {
            zero = false;
        }
    }
    let amplitude_jump = per_channel.iter().sum::<f32>() / channels as f32;
    let spectral = spectral_distance(buffer, end, start);
    SeamMetrics {
        amplitude_jump,
        amplitude_jump_per_channel: per_channel,
        spectral_discontinuity: spectral,
        zero_crossing: zero,
    }
}

/// Score every end-frame candidate in `[requested - window, requested +
/// window]` and return them ranked best-first. Zero crossings are flagged but
/// the ranking also requires per-channel and spectral continuity, so a zero
/// crossing with a large L/R or spectrum break will not win.
pub fn rank_candidates(
    buffer: &AudioBuffer,
    start: usize,
    requested_end: usize,
    window: usize,
) -> Vec<LoopCandidate> {
    let frames = buffer.frames();
    let lo = requested_end.saturating_sub(window).max(start + 2);
    let hi = (requested_end + window + 1).min(frames);

    // Physical reference: RMS of the region itself. A seam jump is judged
    // against the signal's own level rather than against the worst candidate,
    // so two acoustically identical seams score identically instead of being
    // smeared together by window-local min/max normalization.
    let region_len = (hi.saturating_sub(lo)).max(1);
    let mut energy = 0f32;
    let channels = buffer.channels as usize;
    for f in lo..hi {
        for ch in 0..channels {
            let v = buffer.sample(f, ch);
            energy += v * v;
        }
    }
    let rms = (energy / (region_len * channels) as f32).sqrt().max(f32::EPSILON);

    let mut candidates: Vec<LoopCandidate> = (lo..hi)
        .map(|end| {
            let m = measure_seam(buffer, start, end);
            let jump_n = m.amplitude_jump / rms;
            // Spectral distance is already a normalized 0..~1 L1 metric.
            let score = 0.6 * jump_n + 0.4 * m.spectral_discontinuity
                - if m.zero_crossing { 0.01 } else { 0.0 };
            LoopCandidate {
                end_frame: end,
                metrics: m,
                score,
            }
        })
        .collect();

    // Deterministic ordering. Scores within an acoustic epsilon are treated
    // as equal (floating-point rounding must not decide a loop boundary);
    // ties then resolve toward the requested end, then the earliest frame.
    const SCORE_EPSILON: f32 = 1e-6;
    candidates.sort_by(|a, b| {
        let diff = a.score - b.score;
        if diff.abs() <= SCORE_EPSILON {
            std::cmp::Ordering::Equal
        } else {
            diff.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal)
        }
        .then_with(|| {
            a.end_frame
                .abs_diff(requested_end)
                .cmp(&b.end_frame.abs_diff(requested_end))
        })
        .then_with(|| a.end_frame.cmp(&b.end_frame))
    });
    candidates
}

pub fn best_candidate(
    buffer: &AudioBuffer,
    start: usize,
    requested_end: usize,
    window: usize,
) -> Result<LoopCandidate> {
    let ranked = rank_candidates(buffer, start, requested_end, window);
    ranked.into_iter().next().ok_or_else(|| LooperError::NoCandidate {
        asset: String::new(),
        requested: requested_end as i64,
        message: "search window contains no valid end frame".to_string(),
    })
}

fn fade_gain(step: usize, len: usize, curve: FadeCurve, fade_in: bool) -> f32 {
    if len == 0 {
        return if fade_in { 1.0 } else { 1.0 };
    }
    let t = step as f32 / len as f32;
    let (g_in, g_out) = match curve {
        FadeCurve::Linear => (t, 1.0 - t),
        // Equal-power: squares sum to 1.
        FadeCurve::EqualPower => {
            let a = (t * std::f32::consts::FRAC_PI_2).sin();
            (a, (t * std::f32::consts::FRAC_PI_2).cos())
        }
    };
    if fade_in { g_in } else { g_out }
}

/// Build the crossfade plan and refuse when the region cannot hold the
/// requested fade. The builder never expands the region or reads samples
/// outside it.
pub fn plan_fade(
    start: usize,
    end: usize,
    fade_frames: usize,
    min_keep: usize,
    curve: FadeCurve,
    asset: &str,
) -> Result<FadePlan> {
    let region = end - start;
    let head_lo = start;
    let head_hi = start + fade_frames;
    // Saturating subtraction: an over-long fade must produce a clean
    // SegmentTooShort rejection, never an arithmetic panic or a widened read.
    let tail_lo = end.saturating_sub(fade_frames);
    let tail_hi = end;
    let unblended = tail_lo.saturating_sub(head_hi);
    if tail_lo < head_hi || unblended < min_keep {
        return Err(LooperError::SegmentTooShort {
            asset: asset.to_string(),
            region_samples: region,
            fade_samples: fade_frames,
            head_samples: fade_frames,
            tail_samples: fade_frames,
            min_keep,
        });
    }
    Ok(FadePlan {
        fade_frames,
        head_lo,
        head_hi,
        tail_lo,
        tail_hi,
        unblended_frames: unblended,
        curve,
        loop_length_frames: region,
    })
}

/// Apply the crossfade and return the finished loop buffer. Both fade zones
/// are written, which is what makes the *wrap* continuous:
///
/// out[head_lo + i] = head(i)·gain_in(i) + tail(i)·gain_out(i)
/// out[tail_lo + i] = head(i)·gain_out(i) + tail(i)·gain_in(i)
///
/// At equal power the two zones sum to the same material, so the last frame
/// (end of the tail zone) and the first frame (start of the head zone) carry
/// the same head/tail mixture at complementary gains. The residual wrap jump
/// therefore equals the head-vs-tail signal difference — near zero for a
/// periodic loop — instead of the original raw boundary jump. Samples outside
/// the two zones are never modified.
pub fn apply_crossfade(source: &AudioBuffer, plan: &FadePlan) -> AudioBuffer {
    let channels = source.channels as usize;
    let mut out = source.clone();
    for i in 0..plan.fade_frames {
        let head = plan.head_lo + i;
        let tail = plan.tail_lo + i;
        let g_in = fade_gain(i, plan.fade_frames, plan.curve, true);
        let g_out = fade_gain(i, plan.fade_frames, plan.curve, false);
        for ch in 0..channels {
            let hv = source.sample(head, ch);
            let tv = source.sample(tail, ch);
            out.samples[head * channels + ch] = hv * g_in + tv * g_out;
            out.samples[tail * channels + ch] = hv * g_out + tv * g_in;
        }
    }
    out
}

/// Verify a finished loop by simulating the wrap: the jump from its last
/// frame back to its first frame, plus spectral flux across the wrap.
pub fn verify_loop(loop_buffer: &AudioBuffer) -> SeamMetrics {
    let end = loop_buffer.frames();
    measure_seam(loop_buffer, 0, end)
}

/// Linear-gain variant, deterministic and allocation-bounded.
pub fn apply_gain(buffer: &AudioBuffer, gain: f32) -> AudioBuffer {
    AudioBuffer {
        sample_rate: buffer.sample_rate,
        channels: buffer.channels,
        samples: buffer.samples.iter().map(|s| s * gain).collect(),
    }
}

/// Fixed-ratio linear resampling (also changes rate/duration). Deterministic
/// so identical inputs rebuild byte-identically.
pub fn resample(buffer: &AudioBuffer, ratio: f32) -> Result<AudioBuffer> {
    if !(ratio.is_finite() && ratio > 0.0) {
        return Err(LooperError::Encode(format!(
            "resample ratio must be a positive finite number, got {ratio}"
        )));
    }
    if (ratio - 1.0).abs() < f32::EPSILON {
        return Ok(buffer.clone());
    }
    let channels = buffer.channels as usize;
    let new_frames = (buffer.frames() as f32 / ratio).round() as usize;
    let mut samples = Vec::with_capacity(new_frames * channels);
    for n in 0..new_frames {
        let pos = n as f32 * ratio;
        let i0 = pos.floor() as usize;
        let frac = pos - i0 as f32;
        let i1 = (i0 + 1).min(buffer.frames() - 1);
        for ch in 0..channels {
            let a = buffer.sample(i0, ch);
            let b = buffer.sample(i1, ch);
            samples.push(a + (b - a) * frac);
        }
    }
    Ok(AudioBuffer {
        sample_rate: buffer.sample_rate,
        channels: buffer.channels,
        samples,
    })
}

/// Remove a measured DC offset per channel. Offsets are reported in the
/// manifest regardless; this is opt-in via the pipeline.
pub fn remove_dc(buffer: &AudioBuffer) -> (AudioBuffer, Vec<f32>) {
    let offsets = buffer.dc_offset();
    let channels = buffer.channels as usize;
    let samples = buffer
        .samples
        .iter()
        .enumerate()
        .map(|(idx, s)| s - offsets[idx % channels])
        .collect();
    (
        AudioBuffer {
            sample_rate: buffer.sample_rate,
            channels: buffer.channels,
            samples,
        },
        offsets,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(rate: u32, v: Vec<f32>) -> AudioBuffer {
        AudioBuffer {
            sample_rate: rate,
            channels: 1,
            samples: v,
        }
    }

    #[test]
    fn candidate_prefers_continuous_over_zero_crossing() {
        // Cosine with a true 16-frame period. The exclusive end index 33
        // means the last played frame is 32, whose value equals the frame-0
        // peak (cos(4π) == 1 in f32): the seam has zero jump and is NOT a
        // zero crossing. This proves zero crossings are only a candidate
        // condition, never the selection rule.
        let mut v = Vec::new();
        for n in 0..160 {
            v.push((2.0 * std::f32::consts::PI * n as f32 / 16.0).cos());
        }
        let buf = mono(48_000, v);
        let ranked = rank_candidates(&buf, 0, 33, 8);
        let best = ranked.first().unwrap();
        assert_eq!(best.end_frame, 33);
        assert!(best.metrics.amplitude_jump < 1e-6);
        assert!(!best.metrics.zero_crossing);
        // Zero-crossing boundaries in the window carry large wrap jumps.
        let worse = ranked
            .iter()
            .find(|c| c.metrics.zero_crossing)
            .unwrap();
        assert!(worse.metrics.amplitude_jump > 0.5);
    }

    #[test]
    fn short_segment_is_refused_not_extended() {
        let plan = plan_fade(10, 20, 8, 1, FadeCurve::EqualPower, "test");
        assert!(matches!(plan, Err(LooperError::SegmentTooShort { .. })));
    }

    #[test]
    fn fade_plan_intervals_are_traceable() {
        let plan = plan_fade(0, 100, 10, 1, FadeCurve::Linear, "test").unwrap();
        assert_eq!(plan.head_lo, 0);
        assert_eq!(plan.head_hi, 10);
        assert_eq!(plan.tail_lo, 90);
        assert_eq!(plan.tail_hi, 100);
        assert_eq!(plan.unblended_frames, 80);
        assert_eq!(plan.loop_length_frames, 100);
    }

    #[test]
    fn crossfade_preserves_length_and_reduces_seam_jump() {
        // Near-periodic loop (non-integer period, like the mechanical hum
        // fixture): the raw wrap jump is small but non-zero and the
        // crossfade removes most of it while keeping the exact frame count.
        let n = 303usize;
        let period = 30.3f32;
        let v: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * i as f32 / period).sin())
            .collect();
        let buf = mono(48_000, v);
        let start = 0;
        let end = n;
        let raw_jump = measure_seam(&buf, start, end).amplitude_jump;
        assert!(raw_jump > 0.0);
        let plan = plan_fade(start, end, 32, 1, FadeCurve::EqualPower, "t").unwrap();
        let looped = apply_crossfade(
            &buf,
            &FadePlan {
                head_lo: 0,
                head_hi: 32,
                tail_lo: end - 32,
                tail_hi: end,
                ..plan
            },
        );
        assert_eq!(looped.frames(), end - start);
        let wrapped = verify_loop(&looped);
        // Head/tail are nearly identical one period apart, so blending the
        // tail into the head pulls the wrap discontinuity toward zero.
        assert!(
            wrapped.amplitude_jump < raw_jump,
            "crossfade seam {wrapped:?} should beat raw {raw_jump}"
        );
    }

    #[test]
    fn crossfade_never_touches_samples_outside_its_zones() {
        let n = 128usize;
        let v: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * i as f32 / 16.0).sin())
            .collect();
        let buf = mono(48_000, v);
        let plan = plan_fade(0, n, 16, 1, FadeCurve::Linear, "t").unwrap();
        let looped = apply_crossfade(&buf, &plan);
        // Unblended middle region must be bit-identical to the source.
        for f in 16..n - 16 {
            assert_eq!(looped.sample(f, 0), buf.sample(f, 0));
        }
        assert_eq!(looped.frames(), n);
    }

    #[test]
    fn resample_changes_length_deterministically() {
        let buf = mono(48_000, vec![0.0; 100]);
        let up = resample(&buf, 2.0).unwrap();
        assert_eq!(up.frames(), 50);
        let again = resample(&buf, 2.0).unwrap();
        assert_eq!(up.samples, again.samples);
    }
}

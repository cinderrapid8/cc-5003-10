use crate::error::Result;
use crate::io;
use crate::model::AudioBuffer;
use std::path::Path;

/// Synthetic test material. Every generator is deterministic so the test
/// suite and the CLI produce byte-identical WAVs.
pub struct SynthSample {
    pub name: &'static str,
    pub description: &'static str,
    pub buffer: AudioBuffer,
}

const RATE: u32 = 48_000;

fn stereo(samples: Vec<f32>) -> AudioBuffer {
    AudioBuffer {
        sample_rate: RATE,
        channels: 2,
        samples,
    }
}

/// 1. Engine idle: a strong constant DC offset on both channels plus a low
/// rumble. A naive zero-crossing search behaves badly here because the
/// offset moves the zero line; the builder must still find a continuous seam.
pub fn engine_dc_offset() -> SynthSample {
    let frames = RATE as usize * 2; // 2 s
    let period = 96.0f32; // 500 Hz rumble, 3 full-ish structures per second
    let dc = 0.12f32;
    let mut samples = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        let tone = 0.5 * (2.0 * std::f32::consts::PI * n as f32 / period).sin();
        let l = dc + tone + 0.02 * (n as f32 * 0.0013).sin();
        let r = dc + tone + 0.02 * (n as f32 * 0.0017 + 1.3).sin();
        samples.push(l);
        samples.push(r);
    }
    SynthSample {
        name: "engine_dc_offset",
        description: "Engine idle with +0.12 DC offset and low rumble",
        buffer: stereo(samples),
    }
}

/// 2. Mechanical hum whose period is deliberately NOT an integer number of
/// frames. Cutting at any arbitrary sample index lands mid-waveform, so the
/// candidate search and crossfade do the real work.
pub fn mechanical_noninteger_period() -> SynthSample {
    let frames = RATE as usize * 2;
    let period = 73.7f32; // non-integer frame period
    let mut samples = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        let phase = 2.0 * std::f32::consts::PI * n as f32 / period;
        let hum = 0.55 * phase.sin() + 0.18 * (2.0 * phase).sin();
        samples.push(hum);
        samples.push(hum * 0.92);
    }
    SynthSample {
        name: "mechanical_noninteger_period",
        buffer: stereo(samples),
        description: "Mechanical hum with a 73.7-frame (non-integer) period",
    }
}

/// 3. Ambient stereo bed: left and right carry the same pitch but different
/// phase. Per-channel seam measurement must detect a break on one side even
/// when the mid signal looks continuous.
pub fn ambient_stereo_phase() -> SynthSample {
    let frames = RATE as usize * 3;
    let period = 240.0f32; // 200 Hz
    let phase_shift = std::f32::consts::FRAC_PI_2;
    let mut samples = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        let phase = 2.0 * std::f32::consts::PI * n as f32 / period;
        let l = 0.4 * phase.sin() + 0.1 * (n as f32 * 0.0009).sin();
        let r = 0.4 * (phase + phase_shift).sin() + 0.1 * (n as f32 * 0.0011).sin();
        samples.push(l);
        samples.push(r);
    }
    SynthSample {
        name: "ambient_stereo_phase",
        buffer: stereo(samples),
        description: "Stereo ambient bed, L/R 90 degrees out of phase",
    }
}

pub fn all_samples() -> Vec<SynthSample> {
    vec![
        engine_dc_offset(),
        mechanical_noninteger_period(),
        ambient_stereo_phase(),
    ]
}

/// Write the standard fixture set into `dir` and return the file paths.
pub fn write_fixture_set(dir: &Path) -> Result<Vec<String>> {
    io::ensure_dir(dir)?;
    let mut names = Vec::new();
    for sample in all_samples() {
        let file = format!("{}.wav", sample.name);
        io::write_wav(&dir.join(&file), &sample.buffer)?;
        names.push(file);
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_are_deterministic() {
        let a = engine_dc_offset();
        let b = engine_dc_offset();
        assert_eq!(a.buffer.samples, b.buffer.samples);
        // DC offset really exists.
        let offset = a.buffer.dc_offset();
        assert!(offset[0] > 0.10 && offset[1] > 0.10);
    }

    #[test]
    fn stereo_phase_channels_differ() {
        let s = ambient_stereo_phase();
        let l0 = s.buffer.sample(0, 0);
        let r0 = s.buffer.sample(0, 1);
        assert!((l0 - r0).abs() > 0.1);
    }
}

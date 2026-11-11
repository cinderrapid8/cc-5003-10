use serde::{Deserialize, Serialize};

/// A stereo-capable audio buffer with sample-accurate length metadata.
/// All indices everywhere in this project refer to *frame* indices of the
/// decoded PCM this buffer holds (one frame = one sample per channel).
#[derive(Clone, Debug)]
pub struct AudioBuffer {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved PCM, length = frames * channels.
    pub samples: Vec<f32>,
}

impl AudioBuffer {
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    pub fn sample(&self, frame: usize, channel: usize) -> f32 {
        self.samples[frame * self.channels as usize + channel]
    }

    /// Mean absolute level per channel — used for DC offset reporting.
    pub fn dc_offset(&self) -> Vec<f32> {
        let n = self.frames().max(1) as f32;
        (0..self.channels as usize)
            .map(|ch| {
                let sum: f32 = (0..self.frames()).map(|f| self.sample(f, ch)).sum();
                sum / n
            })
            .collect()
    }
}

/// How a source region is chosen. Indices are always frame indices into the
/// *decoded* source file. A zero crossing is treated as one candidate signal,
/// never as the definition of the loop boundary.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LoopRegion {
    /// First frame of the loop, inclusive.
    pub start_frame: usize,
    /// Requested end frame (exclusive). The actual loop end is the accepted
    /// candidate within `search_window` and is recorded separately.
    pub end_frame: usize,
    /// Half-width (frames) of the candidate search window around `end_frame`.
    #[serde(default = "default_window")]
    pub search_window: usize,
}

fn default_window() -> usize {
    256
}

/// Curve used for both the head fade-in and tail fade-out of the crossfade.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FadeCurve {
    Linear,
    EqualPower,
}

impl Default for FadeCurve {
    fn default() -> Self {
        Self::EqualPower
    }
}

/// One source asset plus everything the builder needs to turn it into a
/// looping sound. All numbers are explicit; nothing is inferred from a file
/// name or from another asset's settings.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AssetSpec {
    /// Stable logical id, e.g. "engine_rumble".
    pub id: String,
    /// Path to the source audio (WAV or any codec Symphonia can decode).
    pub source: String,
    /// Optional explicit source identity. When set, the loaded file's hash
    /// must match or the build is rejected — same filename never implies
    /// same resource.
    #[serde(default)]
    pub source_sha256: Option<String>,
    #[serde(default)]
    pub region: Option<LoopRegion>,
    /// Crossfade length in milliseconds (converted to frames at the source
    /// sample rate). The occupied interval is always reported back.
    #[serde(default = "default_fade_ms")]
    pub fade_ms: f64,
    #[serde(default)]
    pub fade_curve: FadeCurve,
    /// At least this many frames between the two fade zones must remain
    /// unblended, otherwise generation is refused.
    #[serde(default = "default_min_keep")]
    pub min_keep_frames: usize,
    /// Variants. A variant either carries explicit gain/pitch parameters or
    /// points at an independent source file — never just another name.
    #[serde(default)]
    pub variants: Vec<VariantSpec>,
    /// Emit a quantized "lossy" WAV alongside the master. It is decoded back
    /// through Symphonia for verification; this container carries no
    /// pre-skip, so measured delay/padding must be 0.
    #[serde(default = "default_true")]
    pub encode_lossy: bool,
    /// Emit a second container carrying explicit pre-skip and trailing
    /// padding, emulating the coordinate behavior of a lossy codec so the
    /// runtime demo verifies a non-zero-delay decoded loop.
    #[serde(default = "default_true")]
    pub encode_packed: bool,
    /// Silent guard frames inserted before content in the packed resource.
    #[serde(default = "default_packed_delay")]
    pub packed_delay_frames: usize,
    /// Silent padding frames appended after content in the packed resource.
    #[serde(default = "default_packed_padding")]
    pub packed_padding_frames: usize,
}

fn default_fade_ms() -> f64 {
    20.0
}
fn default_min_keep() -> usize {
    1
}
fn default_true() -> bool {
    true
}
fn default_packed_delay() -> usize {
    1024
}
fn default_packed_padding() -> usize {
    768
}

/// An intensity variant. The two ways to define one are mutually exclusive
/// and validated at load time.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VariantSpec {
    /// Derive from the parent asset with explicit transformation parameters.
    Parametric {
        name: String,
        /// Linear gain multiplier (1.0 = unchanged).
        #[serde(default = "one")]
        gain: f32,
        /// Resampling ratio applied to pitch/rate (1.0 = unchanged).
        #[serde(default = "one")]
        resample_ratio: f32,
    },
    /// Fully independent source with its own region/fade settings.
    Independent {
        name: String,
        source: String,
        #[serde(default)]
        source_sha256: Option<String>,
        region: Option<LoopRegion>,
        #[serde(default = "default_fade_ms")]
        fade_ms: f64,
        #[serde(default)]
        fade_curve: FadeCurve,
        #[serde(default = "default_min_keep")]
        min_keep_frames: usize,
    },
}

fn one() -> f32 {
    1.0
}

impl VariantSpec {
    pub fn name(&self) -> &str {
        match self {
            VariantSpec::Parametric { name, .. } | VariantSpec::Independent { name, .. } => name,
        }
    }
}

/// A saved project — rebuilds from the same inputs produce the same set of
/// resources (content-addressed file names + fixed codec settings).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub name: String,
    #[serde(default)]
    pub assets: Vec<AssetSpec>,
    /// Schema version, so old project files can be migrated or rejected.
    pub schema_version: u32,
}

impl Project {
    pub const SCHEMA_VERSION: u32 = 1;
}

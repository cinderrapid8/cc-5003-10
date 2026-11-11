use crate::dsp::{FadePlan, LoopCandidate, SeamMetrics};
use crate::model::FadeCurve;
use serde::{Deserialize, Serialize};

/// One physical audio file produced by the builder. A variant that emits WAV
/// and OGG produces two of these.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CodecResource {
    pub codec: String,
    /// Path relative to the manifest.
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
    pub sample_rate: u32,
    pub channels: u16,
    /// Frames the *decoder* will actually deliver (measured, not copied from
    /// the WAV header). Runtime players must loop on this coordinate space.
    pub decoded_frames: usize,
    /// Measured encoder pre-skip (0 for the WAV master).
    pub measured_delay_frames: usize,
    /// Measured trailing padding after the true content.
    pub measured_padding_frames: usize,
}

/// The loop definition in both coordinate spaces. The WAV master uses
/// source frames; encoded resources add the measured delay offset. The
/// runtime demo validates against the decoded-resource coordinates.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LoopRecord {
    /// Frame indices into the decoded source WAV.
    pub source_start_frame: usize,
    pub requested_end_frame: usize,
    pub accepted_end_frame: usize,
    pub search_window_frames: usize,
    /// Indices into each emitted resource's own decoded PCM. Keyed by codec.
    pub resource_coordinates: Vec<ResourceCoordinates>,
    pub fade: FadePlan,
    /// Seam measured on the finished WAV master after crossfading.
    pub master_seam: SeamMetricsJson,
    /// Seam measured after a full decode of the encoded resource (OGG…).
    pub encoded_seam: Option<SeamMetricsJson>,
    pub candidate_count: usize,
    /// Full ranking, auditable. First entry is the accepted boundary.
    pub candidates: Vec<LoopCandidate>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ResourceCoordinates {
    pub codec: String,
    pub start_frame: usize,
    pub end_frame: usize,
    pub loop_length_frames: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SeamMetricsJson {
    pub amplitude_jump: f32,
    pub amplitude_jump_per_channel: Vec<f32>,
    pub spectral_discontinuity: f32,
    pub zero_crossing: bool,
}

impl From<SeamMetrics> for SeamMetricsJson {
    fn from(m: SeamMetrics) -> Self {
        Self {
            amplitude_jump: m.amplitude_jump,
            amplitude_jump_per_channel: m.amplitude_jump_per_channel,
            spectral_discontinuity: m.spectral_discontinuity,
            zero_crossing: m.zero_crossing,
        }
    }
}

/// How a variant was produced. Explicit parameters or an independent source
/// — an identical display name never implies identical content.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "provenance", rename_all = "snake_case")]
pub enum VariantProvenance {
    Base,
    Parametric {
        gain: f32,
        resample_ratio: f32,
        derived_from: String,
    },
    Independent {
        source: String,
        source_sha256: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VariantRecord {
    pub name: String,
    pub provenance: VariantProvenance,
    pub loop_record: LoopRecord,
    pub resources: Vec<CodecResource>,
    pub dc_offset_per_channel: Vec<f32>,
    pub peak_per_channel: Vec<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AssetRecord {
    pub id: String,
    pub source_file: String,
    pub source_sha256: String,
    pub source_sample_rate: u32,
    pub source_channels: u16,
    pub source_decoded_frames: usize,
    pub fade_curve: FadeCurve,
    pub variants: Vec<VariantRecord>,
}

/// Top-level sound-library manifest. Rebuilding the same project writes a
/// manifest whose resource hashes and indices can be diffed line by line.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub schema_version: u32,
    pub project_name: String,
    /// Content hash of the project file itself.
    pub project_sha256: String,
    pub builder_version: String,
    pub created_with: String,
    pub assets: Vec<AssetRecord>,
}

/// Progress events emitted while building, surfaced in the UI and CLI.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum ProgressEvent {
    Started { total_assets: usize },
    AssetStarted { asset: String, index: usize },
    SourceLoaded { asset: String, frames: usize },
    Searching { asset: String, candidates: usize },
    Fading { asset: String, variant: String, fade_frames: usize },
    Encoding { asset: String, variant: String, codec: String },
    Verifying { asset: String, variant: String, codec: String },
    AssetDone { asset: String, index: usize },
    Finished { resources: usize, manifest: String },
    Rejected { asset: String, reason: String },
}

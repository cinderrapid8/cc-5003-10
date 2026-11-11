use std::path::PathBuf;

/// All failures the builder can produce. Every variant carries a precise
/// human-readable reason because a rejection must never be silent.
#[derive(Debug, thiserror::Error)]
pub enum LooperError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("wav decode error on {path}: {message}")]
    WavDecode { path: PathBuf, message: String },

    #[error("audio decode error on {path}: {message}")]
    Decode { path: PathBuf, message: String },

    #[error("encode error: {0}")]
    Encode(String),

    #[error("invalid configuration for {asset}: {message}")]
    Config { asset: String, message: String },

    #[error(
        "segment too short for {asset}: region holds {region_samples} samples but the requested \
         fade needs {fade_samples} samples twice over (head {head_samples} + tail {tail_samples} \
         plus {min_keep} unblended); the builder never widens the region to read other sounds"
    )]
    SegmentTooShort {
        asset: String,
        region_samples: usize,
        fade_samples: usize,
        head_samples: usize,
        tail_samples: usize,
        min_keep: usize,
    },

    #[error("no acceptable loop candidate for {asset} near index {requested}: {message}")]
    NoCandidate { asset: String, requested: i64, message: String },

    #[error("variant collision: {message}")]
    VariantCollision { message: String },

    #[error("project file error: {0}")]
    Project(String),
}

pub type Result<T> = std::result::Result<T, LooperError>;

impl LooperError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub fn config(asset: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Config {
            asset: asset.into(),
            message: message.into(),
        }
    }
}

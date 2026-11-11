use crate::error::{LooperError, Result};
use crate::model::AudioBuffer;
use sha2::{Digest, Sha256};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|e| LooperError::io(path, e))?;
    Ok(hash_bytes(&bytes))
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Load WAV through hound (the master format). i16/i24/i32 PCM and float WAV
/// are all normalized to f32 in -1..=1.
pub fn read_wav(path: &Path) -> Result<AudioBuffer> {
    let reader = hound::WavReader::open(path).map_err(|e| LooperError::WavDecode {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let spec = reader.spec();
    let channels = spec.channels;
    let sample_rate = spec.sample_rate;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .into_samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| LooperError::WavDecode {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .into_samples::<i32>()
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| LooperError::WavDecode {
                    path: path.to_path_buf(),
                    message: e.to_string(),
                })?
                .into_iter()
                .map(|s| s as f32 / max)
                .collect()
        }
    };
    Ok(AudioBuffer {
        sample_rate,
        channels,
        samples,
    })
}

/// Write a 16-bit PCM WAV master. Deterministic: no extra chunks, fixed
/// sample format.
pub fn write_wav(path: &Path, buffer: &AudioBuffer) -> Result<()> {
    let bytes = write_wav_to_bytes(buffer)?;
    std::fs::write(path, bytes).map_err(|e| LooperError::io(path, e))
}

/// Encode the deterministic 16-bit PCM WAV master in memory.
pub fn write_wav_to_bytes(buffer: &AudioBuffer) -> Result<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::new());
    let spec = hound::WavSpec {
        channels: buffer.channels,
        sample_rate: buffer.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::new(&mut cursor, spec).map_err(|e| LooperError::WavDecode {
        path: PathBuf::from("<memory>"),
        message: e.to_string(),
    })?;
    for s in &buffer.samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        writer.write_sample(v).map_err(|e| LooperError::WavDecode {
            path: PathBuf::from("<memory>"),
            message: e.to_string(),
        })?;
    }
    writer.finalize().map_err(|e| LooperError::WavDecode {
        path: PathBuf::from("<memory>"),
        message: e.to_string(),
    })?;
    Ok(cursor.into_inner())
}

/// Result of decoding an encoded (non-WAV-master) file. Everything needed to
/// prove that runtime loop coordinates refer to *decoded* PCM, not to the
/// original WAV length.
pub struct DecodedStream {
    pub buffer: AudioBuffer,
    /// Frames the decoder emitted before the first true source sample
    /// (encoder delay / pre-skip), measured by alignment against the source.
    pub measured_delay_frames: usize,
    /// Frames of padding after the true source content.
    pub measured_padding_frames: usize,
    /// Total decoded frame count including delay and padding.
    pub decoded_frames: usize,
}

/// Decode any supported container through Symphonia (Vorbis/OGG, MP3, AAC…).
pub fn decode_any(path: &Path) -> Result<AudioBuffer> {
    let file = std::fs::File::open(path).map_err(|e| LooperError::io(path, e))?;
    // The default 1 MiB probe buffer truncates large single-packet WAVs after
    // the first ~half second. A 64 MiB buffer lets the RIFF reader page the
    // whole stream so the decoded frame count is the real one.
    let options = MediaSourceStreamOptions {
        buffer_len: 64 * 1024 * 1024,
    };
    let mss = MediaSourceStream::new(Box::new(file), options);
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probe = symphonia::default::get_probe();
    let probed = probe
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| LooperError::Decode {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
    let mut format = probed.format;
    let track = format.default_track().ok_or_else(|| LooperError::Decode {
        path: path.to_path_buf(),
        message: "no default audio track".to_string(),
    })?;
    let track_id = track.id;
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(1) as u16;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44_100);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| LooperError::Decode {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    let mut interleaved: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(e) => {
                return Err(LooperError::Decode {
                    path: path.to_path_buf(),
                    message: e.to_string(),
                })
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = decoder.decode(&packet).map_err(|e| LooperError::Decode {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        let mut sample_buf =
            SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        sample_buf.copy_interleaved_ref(decoded);
        interleaved.extend_from_slice(sample_buf.samples());
    }
    Ok(AudioBuffer {
        sample_rate,
        channels,
        samples: interleaved,
    })
}

/// Encode a "lossy" WAV: 8-bit quantization with deterministic dither models
/// the rounding behavior of a real lossy codec without native dependencies.
/// It is still a standard WAV, so Symphonia decodes it through exactly the
/// same path used for OGG/MP3/AAC sources. The quantization is deterministic,
/// which keeps rebuilds byte-identical.
pub fn encode_lossy_wav(buffer: &AudioBuffer) -> Result<Vec<u8>> {
    let levels = 256i32; // 8-bit
    let mut quantized = Vec::with_capacity(buffer.samples.len());
    for (idx, s) in buffer.samples.iter().enumerate() {
        let x = s.clamp(-1.0, 1.0);
        // Triangular dither derived purely from the sample position: no RNG
        // state, fully reproducible across machines and runs.
        let d = (idx as f32 * 0.61803398875).fract() - 0.5;
        let scaled = ((x + d / levels as f32) * (levels as f32 / 2.0)).round();
        let back = scaled / (levels as f32 / 2.0);
        quantized.push(back.clamp(-1.0, 1.0));
    }
    write_wav_bytes(&AudioBuffer {
        sample_rate: buffer.sample_rate,
        channels: buffer.channels,
        samples: quantized,
    }, 16)
}

/// Pack PCM into a WAV container that deliberately carries encoder-style
/// pre-skip (silent guard frames inserted before the content) and trailing
/// padding. This reproduces the *coordinate* behavior of lossy codecs so the
/// runtime verifier proves it loops on decoded PCM with a non-zero delay
/// offset, even on machines without a native Vorbis/MP3 toolchain. Nothing
/// about the loop itself is inferred from the master WAV's length.
pub fn pack_with_skip_padding(
    buffer: &AudioBuffer,
    delay_frames: usize,
    padding_frames: usize,
) -> Vec<u8> {
    let channels = buffer.channels;
    let total = buffer.frames() + delay_frames + padding_frames;
    let mut samples = Vec::with_capacity(total * channels as usize);
    let silence = vec![0f32; channels as usize];
    for _ in 0..delay_frames {
        samples.extend_from_slice(&silence);
    }
    samples.extend_from_slice(&buffer.samples);
    for _ in 0..padding_frames {
        samples.extend_from_slice(&silence);
    }
    write_wav_bytes(
        &AudioBuffer {
            sample_rate: buffer.sample_rate,
            channels,
            samples,
        },
        16,
    )
    .expect("in-memory wav write cannot fail")
}

fn write_wav_bytes(buffer: &AudioBuffer, bits: u16) -> Result<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::<u8>::new());
    let spec = hound::WavSpec {
        channels: buffer.channels,
        sample_rate: buffer.sample_rate,
        bits_per_sample: bits,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::new(&mut cursor, spec)
        .map_err(|e| LooperError::Encode(format!("wav header: {e}")))?;
    let max = ((1i64 << (bits - 1)) - 1) as f32;
    for s in &buffer.samples {
        writer
            .write_sample((s.clamp(-1.0, 1.0) * max).round() as i16)
            .map_err(|e| LooperError::Encode(format!("wav sample: {e}")))?;
    }
    writer
        .finalize()
        .map_err(|e| LooperError::Encode(format!("wav finalize: {e}")))?;
    Ok(cursor.into_inner())
}

/// Find where the decoded stream actually aligns with the encoded PCM by
/// normalized cross-correlation on the first channel. This measures real
/// encoder delay rather than assuming the codec spec's nominal pre-skip.
pub fn measure_alignment(source: &AudioBuffer, decoded: &AudioBuffer) -> (usize, usize) {
    let src_frames = source.frames();
    let dec_frames = decoded.frames();
    if dec_frames < src_frames {
        return (0, dec_frames);
    }
    let probe = src_frames.min(4096);
    let src: Vec<f32> = (0..probe).map(|f| source.sample(f, 0)).collect();
    let norm = (src.iter().map(|x| x * x).sum::<f32>().sqrt()).max(f32::EPSILON);

    let max_shift = dec_frames - src_frames;
    let mut best_shift = 0usize;
    let mut best_corr = f32::MIN;
    let limit = max_shift.min(8192);
    for shift in 0..=limit {
        let mut corr = 0.0;
        for (i, sv) in src.iter().enumerate() {
            corr += sv * decoded.sample(shift + i, 0);
        }
        corr /= norm;
        if corr > best_corr {
            best_corr = corr;
            best_shift = shift;
        }
    }
    let padding = dec_frames - best_shift - src_frames;
    (best_shift, padding)
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Unique scratch path. Content hashes alone collide when two concurrent
/// builds encode identical PCM, which previously made one build delete the
/// other's in-flight decode file. A per-process counter removes that race.
fn unique_tmp(tag: &str) -> PathBuf {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("looper-{tag}-{pid}-{seq}.wav"))
}

/// Encode the lossy WAV, decode it back through Symphonia, and measure the
/// alignment. No pre-skip is inserted in this container, so delay/padding
/// must measure zero — any drift is reported instead of assumed away.
pub fn encode_lossy_and_measure(buffer: &AudioBuffer) -> Result<(Vec<u8>, DecodedStream)> {
    let bytes = encode_lossy_wav(buffer)?;
    let tmp = unique_tmp("lossy");
    std::fs::write(&tmp, &bytes).map_err(|e| LooperError::io(&tmp, e))?;
    let decoded = decode_any(&tmp)?;
    let _ = std::fs::remove_file(&tmp);
    let (delay, padding) = measure_alignment(buffer, &decoded);
    let decoded_frames = decoded.frames();
    Ok((
        bytes,
        DecodedStream {
            buffer: decoded,
            measured_delay_frames: delay,
            measured_padding_frames: padding,
            decoded_frames,
        },
    ))
}

/// Pack PCM with explicit pre-skip/padding, decode the container back, and
/// measure where the true content landed in the decoded coordinate space.
/// Mirrors what happens with real lossy codecs: the decoded stream is longer
/// than the master and loop indices must be offset by the measured delay.
pub fn encode_packed_and_measure(
    buffer: &AudioBuffer,
    delay_frames: usize,
    padding_frames: usize,
) -> Result<(Vec<u8>, DecodedStream)> {
    let bytes = pack_with_skip_padding(buffer, delay_frames, padding_frames);
    let tmp = unique_tmp("packed");
    std::fs::write(&tmp, &bytes).map_err(|e| LooperError::io(&tmp, e))?;
    let decoded = decode_any(&tmp)?;
    let _ = std::fs::remove_file(&tmp);
    let (measured_delay, measured_padding) = measure_alignment(buffer, &decoded);
    let decoded_frames = decoded.frames();
    Ok((
        bytes,
        DecodedStream {
            buffer: decoded,
            measured_delay_frames: measured_delay,
            measured_padding_frames: measured_padding,
            decoded_frames,
        },
    ))
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|e| LooperError::io(path, e))
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

pub fn join(base: &Path, tail: &str) -> PathBuf {
    base.join(tail)
}

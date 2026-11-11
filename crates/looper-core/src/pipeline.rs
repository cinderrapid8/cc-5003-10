use crate::dsp::{
    apply_crossfade, apply_gain, measure_seam, plan_fade, rank_candidates, resample, verify_loop,
    FadePlan,
};
use crate::error::{LooperError, Result};
use crate::io;
use crate::manifest::{
    AssetRecord, CodecResource, LoopRecord, Manifest, ProgressEvent, ResourceCoordinates,
    SeamMetricsJson, VariantProvenance, VariantRecord,
};
use crate::model::{AssetSpec, AudioBuffer, FadeCurve, LoopRegion, Project, VariantSpec};
use std::path::{Path, PathBuf};

pub fn builder_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Description of one variant after the region has been resolved.
struct VariantJob {
    name: String,
    provenance: VariantProvenance,
    buffer: AudioBuffer,
    region: LoopRegion,
    fade_ms: f64,
    curve: FadeCurve,
    min_keep: usize,
    encode_lossy: bool,
    encode_packed: bool,
    packed_delay_frames: usize,
    packed_padding_frames: usize,
}

/// Validate a project without touching the filesystem beyond reading the
/// sources. Structural mistakes (duplicate ids, unknown variant kinds,
/// bad parameters) are reported here with explicit messages.
pub fn validate_project(project: &Project) -> Result<()> {
    if project.schema_version != Project::SCHEMA_VERSION {
        return Err(LooperError::Project(format!(
            "schema version {} unsupported (need {})",
            project.schema_version,
            Project::SCHEMA_VERSION
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for asset in &project.assets {
        if asset.id.trim().is_empty() {
            return Err(LooperError::config(&asset.id, "asset id is empty"));
        }
        if !seen.insert(asset.id.clone()) {
            return Err(LooperError::config(&asset.id, "duplicate asset id"));
        }
        if !(asset.fade_ms.is_finite() && asset.fade_ms > 0.0) {
            return Err(LooperError::config(&asset.id, "fade_ms must be > 0"));
        }
        if asset.packed_delay_frames == 0 && asset.packed_padding_frames == 0 {
            return Err(LooperError::config(
                &asset.id,
                "packed resource needs a non-zero delay or padding to be meaningful",
            ));
        }
        let mut names = std::collections::HashSet::new();
        for variant in &asset.variants {
            if !names.insert(variant.name().to_string()) {
                return Err(LooperError::config(
                    &asset.id,
                    format!("duplicate variant name '{}'", variant.name()),
                ));
            }
            if let VariantSpec::Parametric {
                gain,
                resample_ratio,
                ..
            } = variant
            {
                if !(gain.is_finite() && *gain > 0.0) {
                    return Err(LooperError::config(
                        &asset.id,
                        format!("variant '{}' gain must be > 0", variant.name()),
                    ));
                }
                if !(resample_ratio.is_finite() && *resample_ratio > 0.0) {
                    return Err(LooperError::config(
                        &asset.id,
                        format!("variant '{}' resample_ratio must be > 0", variant.name()),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn load_source(path: &Path, expected_hash: &Option<String>, asset: &str) -> Result<(AudioBuffer, String)> {
    let hash = io::sha256_file(path)?;
    if let Some(expected) = expected_hash {
        if !hash.eq_ignore_ascii_case(expected) {
            return Err(LooperError::config(
                asset,
                format!(
                    "source hash mismatch for {}: file is {hash}, project pins {expected}; \
                     same filename is not treated as same resource",
                    path.display()
                ),
            ));
        }
    }
    let buffer = if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("wav"))
        .unwrap_or(false)
    {
        io::read_wav(path)?
    } else {
        io::decode_any(path)?
    };
    Ok((buffer, hash))
}

fn resolve_region(buffer: &AudioBuffer, region: Option<&LoopRegion>) -> Result<LoopRegion> {
    let frames = buffer.frames();
    match region {
        Some(r) => {
            if r.start_frame >= frames || r.end_frame > frames || r.end_frame <= r.start_frame + 1 {
                return Err(LooperError::config(
                    "region",
                    format!(
                        "region [{}, {}) out of range for {} decoded frames",
                        r.start_frame, r.end_frame, frames
                    ),
                ));
            }
            Ok(r.clone())
        }
        None => Ok(LoopRegion {
            start_frame: 0,
            end_frame: frames,
            search_window: 256.min(frames / 4).max(1),
        }),
    }
}

fn fade_frames_for(fade_ms: f64, sample_rate: u32) -> usize {
    (fade_ms * sample_rate as f64 / 1000.0).round().max(1.0) as usize
}

fn peaks(buffer: &AudioBuffer) -> Vec<f32> {
    (0..buffer.channels as usize)
        .map(|ch| {
            (0..buffer.frames())
                .map(|f| buffer.sample(f, ch).abs())
                .fold(0f32, f32::max)
        })
        .collect()
}

/// Build one variant: candidate search (zero crossing is only a candidate
/// condition), crossfade planning with hard length validation, then encode
/// and verify every emitted resource against its actual decoded output.
fn build_variant(
    job: &VariantJob,
    out_dir: &Path,
    asset_id: &str,
    emit: &dyn Fn(ProgressEvent),
) -> Result<VariantRecord> {
    let buffer = &job.buffer;
    let region = &job.region;
    let fade_frames = fade_frames_for(job.fade_ms, buffer.sample_rate);

    emit(ProgressEvent::Searching {
        asset: asset_id.to_string(),
        candidates: 0,
    });
    let mut ranked = rank_candidates(
        buffer,
        region.start_frame,
        region.end_frame,
        region.search_window,
    );
    if ranked.is_empty() {
        return Err(LooperError::NoCandidate {
            asset: asset_id.to_string(),
            requested: region.end_frame as i64,
            message: "search window produced no candidates".to_string(),
        });
    }
    ranked.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
    let candidate_count = ranked.len();
    let accepted: crate::dsp::LoopCandidate = ranked[0].clone();
    let accepted_end = accepted.end_frame;

    // Refuse when the region cannot hold the requested fade. We never widen
    // the region or read neighboring audio.
    let plan: FadePlan = plan_fade(
        region.start_frame,
        accepted_end,
        fade_frames,
        job.min_keep,
        job.curve,
        &format!("{asset_id}/{}", job.name),
    )?;

    emit(ProgressEvent::Fading {
        asset: asset_id.to_string(),
        variant: job.name.clone(),
        fade_frames,
    });
    let sliced = AudioBuffer {
        sample_rate: buffer.sample_rate,
        channels: buffer.channels,
        samples: buffer.samples
            [region.start_frame * buffer.channels as usize..accepted_end * buffer.channels as usize]
            .to_vec(),
    };
    // Crossfade indices are rebased onto the sliced loop (same lengths).
    let local_plan = FadePlan {
        head_lo: plan.head_lo - region.start_frame,
        head_hi: plan.head_hi - region.start_frame,
        tail_lo: plan.tail_lo - region.start_frame,
        tail_hi: plan.tail_hi - region.start_frame,
        ..plan.clone()
    };
    let loop_buffer = apply_crossfade(&sliced, &local_plan);
    let master_seam = verify_loop(&loop_buffer).into();

    // --- WAV master (written first so its byte hash names every variant) --
    let wav_bytes = io::write_wav_to_bytes(&loop_buffer)?;
    let wav_hash = io::hash_bytes(&wav_bytes);
    // Content-addressed stem: identical loop PCM always yields the same file
    // name across machines and rebuilds, so a rebuild diffs cleanly.
    let stem = format!("{asset_id}__{}__{}", job.name, &wav_hash[..16]);
    let wav_name = format!("{stem}.wav");
    let wav_path = out_dir.join(&wav_name);
    std::fs::write(&wav_path, &wav_bytes).map_err(|e| LooperError::io(&wav_path, e))?;
    let wav_resource = CodecResource {
        codec: "wav_pcm_s16".to_string(),
        file: wav_name.clone(),
        sha256: wav_hash,
        bytes: wav_bytes.len() as u64,
        sample_rate: loop_buffer.sample_rate,
        channels: loop_buffer.channels,
        decoded_frames: loop_buffer.frames(),
        measured_delay_frames: 0,
        measured_padding_frames: 0,
    };

    let mut resources = vec![wav_resource];
    let mut resource_coordinates = vec![ResourceCoordinates {
        codec: "wav_pcm_s16".to_string(),
        start_frame: 0,
        end_frame: loop_buffer.frames(),
        loop_length_frames: loop_buffer.frames(),
    }];
    let mut encoded_seam: Option<SeamMetricsJson> = None;

    // --- Lossy WAV: encode, then decode back and measure -----------------
    if job.encode_lossy {
        emit(ProgressEvent::Encoding {
            asset: asset_id.to_string(),
            variant: job.name.clone(),
            codec: "wav_lossy_q8".to_string(),
        });
        let (lossy_bytes, measured) = io::encode_lossy_and_measure(&loop_buffer)?;
        let lossy_name = format!("{stem}.lossy.wav");
        let lossy_path = out_dir.join(&lossy_name);
        std::fs::write(&lossy_path, &lossy_bytes)
            .map_err(|e| LooperError::io(&lossy_path, e))?;
        let lossy_hash = io::hash_bytes(&lossy_bytes);

        emit(ProgressEvent::Verifying {
            asset: asset_id.to_string(),
            variant: job.name.clone(),
            codec: "wav_lossy_q8".to_string(),
        });
        let dec = &measured.buffer;
        if measured.measured_delay_frames != 0 || measured.measured_padding_frames != 0 {
            return Err(LooperError::Encode(format!(
                "plain lossy WAV unexpectedly introduced delay/padding: {} / {} frames",
                measured.measured_delay_frames, measured.measured_padding_frames
            )));
        }
        let seam = measure_seam(dec, 0, loop_buffer.frames());
        encoded_seam = Some(SeamMetricsJson::from(seam));

        resources.push(CodecResource {
            codec: "wav_lossy_q8".to_string(),
            file: lossy_name,
            sha256: lossy_hash,
            bytes: lossy_bytes.len() as u64,
            sample_rate: dec.sample_rate,
            channels: dec.channels,
            decoded_frames: measured.decoded_frames,
            measured_delay_frames: 0,
            measured_padding_frames: 0,
        });
        resource_coordinates.push(ResourceCoordinates {
            codec: "wav_lossy_q8".to_string(),
            start_frame: 0,
            end_frame: loop_buffer.frames(),
            loop_length_frames: loop_buffer.frames(),
        });
    }

    // --- Packed resource with explicit pre-skip / padding ----------------
    // This is the codec-coordinate test: the decoded stream is longer than
    // the master, and the loop indices live inside that decoded space.
    if job.encode_packed {
        emit(ProgressEvent::Encoding {
            asset: asset_id.to_string(),
            variant: job.name.clone(),
            codec: "wav_packed_skip_pad".to_string(),
        });
        let (packed_bytes, measured) = io::encode_packed_and_measure(
            &loop_buffer,
            job.packed_delay_frames,
            job.packed_padding_frames,
        )?;
        let packed_name = format!("{stem}.packed.wav");
        let packed_path = out_dir.join(&packed_name);
        std::fs::write(&packed_path, &packed_bytes)
            .map_err(|e| LooperError::io(&packed_path, e))?;
        let packed_hash = io::hash_bytes(&packed_bytes);

        emit(ProgressEvent::Verifying {
            asset: asset_id.to_string(),
            variant: job.name.clone(),
            codec: "wav_packed_skip_pad".to_string(),
        });
        let delay = measured.measured_delay_frames;
        let dec = &measured.buffer;
        let enc_start = delay;
        let enc_end = delay + loop_buffer.frames();
        if enc_end > dec.frames() {
            return Err(LooperError::Encode(format!(
                "decoded packed stream shorter than source loop ({dec_f} < {need})",
                dec_f = dec.frames(),
                need = enc_end
            )));
        }
        if delay == 0 {
            return Err(LooperError::Encode(
                "packed resource measured zero pre-skip; alignment check failed".to_string(),
            ));
        }
        let seam = measure_seam(dec, enc_start, enc_end);
        // Keep the most informative encoded seam (packed, since it exercises
        // the offset coordinates); FLAC's is stored first if both exist.
        if encoded_seam.is_none() {
            encoded_seam = Some(SeamMetricsJson::from(seam));
        }

        resources.push(CodecResource {
            codec: "wav_packed_skip_pad".to_string(),
            file: packed_name,
            sha256: packed_hash,
            bytes: packed_bytes.len() as u64,
            sample_rate: dec.sample_rate,
            channels: dec.channels,
            decoded_frames: measured.decoded_frames,
            measured_delay_frames: measured.measured_delay_frames,
            measured_padding_frames: measured.measured_padding_frames,
        });
        resource_coordinates.push(ResourceCoordinates {
            codec: "wav_packed_skip_pad".to_string(),
            start_frame: enc_start,
            end_frame: enc_end,
            loop_length_frames: loop_buffer.frames(),
        });
    }

    let loop_record = LoopRecord {
        source_start_frame: region.start_frame,
        requested_end_frame: region.end_frame,
        accepted_end_frame: accepted_end,
        search_window_frames: region.search_window,
        resource_coordinates,
        fade: plan,
        master_seam,
        encoded_seam,
        candidate_count,
        candidates: ranked,
    };

    Ok(VariantRecord {
        name: job.name.clone(),
        provenance: job.provenance.clone(),
        loop_record,
        resources,
        dc_offset_per_channel: loop_buffer.dc_offset(),
        peak_per_channel: peaks(&loop_buffer),
    })
}

/// Full build. Sources are resolved relative to `base_dir`; outputs go to
/// `out_dir`. Every progress step is forwarded through `emit`.
pub fn build(
    project: &Project,
    base_dir: &Path,
    out_dir: &Path,
    emit: &dyn Fn(ProgressEvent),
) -> Result<(Manifest, PathBuf)> {
    validate_project(project)?;
    io::ensure_dir(out_dir)?;

    let project_bytes = serde_json::to_vec(project).map_err(|e| LooperError::Project(e.to_string()))?;
    let project_hash = io::hash_bytes(&project_bytes);

    emit(ProgressEvent::Started {
        total_assets: project.assets.len(),
    });

    let mut asset_records = Vec::new();
    let mut total_resources = 0usize;

    for (index, asset) in project.assets.iter().enumerate() {
        let result = build_asset(asset, base_dir, out_dir, emit, index);
        match result {
            Ok((record, n)) => {
                total_resources += n;
                asset_records.push(record);
            }
            Err(e) => {
                emit(ProgressEvent::Rejected {
                    asset: asset.id.clone(),
                    reason: e.to_string(),
                });
                return Err(e);
            }
        }
    }

    let manifest = Manifest {
        schema_version: Project::SCHEMA_VERSION,
        project_name: project.name.clone(),
        project_sha256: project_hash,
        builder_version: builder_version().to_string(),
        created_with: format!("looper-core {}", builder_version()),
        assets: asset_records,
    };
    let manifest_path = out_dir.join("soundbank.json");
    let json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| LooperError::Project(e.to_string()))?;
    std::fs::write(&manifest_path, json).map_err(|e| LooperError::io(&manifest_path, e))?;

    emit(ProgressEvent::Finished {
        resources: total_resources,
        manifest: "soundbank.json".to_string(),
    });
    Ok((manifest, manifest_path))
}

fn build_asset(
    asset: &AssetSpec,
    base_dir: &Path,
    out_dir: &Path,
    emit: &dyn Fn(ProgressEvent),
    index: usize,
) -> Result<(AssetRecord, usize)> {
    emit(ProgressEvent::AssetStarted {
        asset: asset.id.clone(),
        index,
    });
    let source_path = base_dir.join(&asset.source);
    let (source_buffer, source_hash) =
        load_source(&source_path, &asset.source_sha256, &asset.id)?;
    emit(ProgressEvent::SourceLoaded {
        asset: asset.id.clone(),
        frames: source_buffer.frames(),
    });
    let base_region = resolve_region(&source_buffer, asset.region.as_ref())?;

    // Base variant always exists — the asset itself.
    let mut jobs: Vec<VariantJob> = Vec::new();
    jobs.push(VariantJob {
        name: "base".to_string(),
        provenance: VariantProvenance::Base,
        buffer: source_buffer.clone(),
        region: base_region.clone(),
        fade_ms: asset.fade_ms,
        curve: asset.fade_curve,
        min_keep: asset.min_keep_frames,
        encode_lossy: asset.encode_lossy,
        encode_packed: asset.encode_packed,
        packed_delay_frames: asset.packed_delay_frames,
        packed_padding_frames: asset.packed_padding_frames,
    });

    for variant in &asset.variants {
        match variant {
            VariantSpec::Parametric {
                name,
                gain,
                resample_ratio,
            } => {
                // Parametric variants transform the finished base loop's
                // PCM, so the crossfade survives the gain change and the
                // resampled loop keeps its relative proportions.
                let fade_frames = fade_frames_for(asset.fade_ms, source_buffer.sample_rate);
                let plan = plan_fade(
                    base_region.start_frame,
                    base_region.end_frame,
                    fade_frames,
                    asset.min_keep_frames,
                    asset.fade_curve,
                    &format!("{}/{}", asset.id, name),
                )?;
                let sliced = AudioBuffer {
                    sample_rate: source_buffer.sample_rate,
                    channels: source_buffer.channels,
                    samples: source_buffer.samples
                        [base_region.start_frame * source_buffer.channels as usize
                            ..base_region.end_frame * source_buffer.channels as usize]
                        .to_vec(),
                };
                let local_plan = FadePlan {
                    head_lo: plan.head_lo - base_region.start_frame,
                    head_hi: plan.head_hi - base_region.start_frame,
                    tail_lo: plan.tail_lo - base_region.start_frame,
                    tail_hi: plan.tail_hi - base_region.start_frame,
                    ..plan
                };
                let looped = apply_crossfade(&sliced, &local_plan);
                let gained = apply_gain(&looped, *gain);
                let transformed = resample(&gained, *resample_ratio)?;
                let transformed_rate = transformed.sample_rate;
                // Resampling changes frame counts: rebuild a region over
                // the full transformed loop and scale the fade.
                let new_frames = transformed.frames();
                let scaled_fade =
                    (fade_frames as f32 / resample_ratio).round().max(1.0) as usize;
                let region = LoopRegion {
                    start_frame: 0,
                    end_frame: new_frames,
                    search_window: 1,
                };
                jobs.push(VariantJob {
                    name: name.clone(),
                    provenance: VariantProvenance::Parametric {
                        gain: *gain,
                        resample_ratio: *resample_ratio,
                        derived_from: asset.id.clone(),
                    },
                    buffer: transformed,
                    region,
                    fade_ms: scaled_fade as f64 * 1000.0 / transformed_rate as f64,
                    curve: asset.fade_curve,
                    min_keep: asset.min_keep_frames,
                    encode_lossy: asset.encode_lossy,
                    encode_packed: asset.encode_packed,
                    packed_delay_frames: asset.packed_delay_frames,
                    packed_padding_frames: asset.packed_padding_frames,
                });
            }
            VariantSpec::Independent {
                name,
                source,
                source_sha256,
                region,
                fade_ms,
                fade_curve,
                min_keep_frames,
            } => {
                let path = base_dir.join(source);
                let (buf, _hash) = load_source(&path, source_sha256, &format!("{}/{}", asset.id, name))?;
                let resolved = resolve_region(&buf, region.as_ref())?;
                jobs.push(VariantJob {
                    name: name.clone(),
                    provenance: VariantProvenance::Independent {
                        source: source.clone(),
                        source_sha256: source_sha256.clone(),
                    },
                    buffer: buf,
                    region: resolved,
                    fade_ms: *fade_ms,
                    curve: *fade_curve,
                    min_keep: *min_keep_frames,
                    encode_lossy: asset.encode_lossy,
                    encode_packed: asset.encode_packed,
                    packed_delay_frames: asset.packed_delay_frames,
                    packed_padding_frames: asset.packed_padding_frames,
                });
            }
        }
    }

    let mut variant_records = Vec::new();
    let mut resource_count = 0usize;
    for job in &jobs {
        let record = build_variant(job, out_dir, &asset.id, emit)?;
        resource_count += record.resources.len();
        variant_records.push(record);
        emit(ProgressEvent::AssetDone {
            asset: asset.id.clone(),
            index,
        });
    }

    Ok((
        AssetRecord {
            id: asset.id.clone(),
            source_file: asset.source.clone(),
            source_sha256: source_hash,
            source_sample_rate: source_buffer.sample_rate,
            source_channels: source_buffer.channels,
            source_decoded_frames: source_buffer.frames(),
            fade_curve: asset.fade_curve,
            variants: variant_records,
        },
        resource_count,
    ))
}

/// Save a project file (pretty JSON, trailing newline) — the "project save"
/// feature. Deterministic key order via serde struct field order.
pub fn save_project(project: &Project, path: &Path) -> Result<()> {
    let mut json =
        serde_json::to_string_pretty(project).map_err(|e| LooperError::Project(e.to_string()))?;
    json.push('\n');
    std::fs::write(path, json).map_err(|e| LooperError::io(path, e))
}

pub fn load_project(path: &Path) -> Result<Project> {
    let bytes = std::fs::read(path).map_err(|e| LooperError::io(path, e))?;
    let project: Project =
        serde_json::from_slice(&bytes).map_err(|e| LooperError::Project(e.to_string()))?;
    validate_project(&project)?;
    Ok(project)
}

/// Re-verify an already built manifest against the files on disk. This is
/// what the runtime demo calls: it re-decodes every encoded resource and
/// checks the seam in the resource's own coordinate space.
pub fn reverify_manifest(manifest: &Manifest, out_dir: &Path) -> Result<Vec<ReverifyReport>> {
    let mut reports = Vec::new();
    for asset in &manifest.assets {
        for variant in &asset.variants {
            for resource in &variant.resources {
                let path = out_dir.join(&resource.file);
                let decoded = io::decode_any(&path)?;
                if decoded.frames() != resource.decoded_frames {
                    return Err(LooperError::Decode {
                        path,
                        message: format!(
                            "frame count drift: manifest says {}, decoder yields {}",
                            resource.decoded_frames,
                            decoded.frames()
                        ),
                    });
                }
                let coords = variant
                    .loop_record
                    .resource_coordinates
                    .iter()
                    .find(|c| c.codec == resource.codec)
                    .ok_or_else(|| LooperError::Project(format!(
                        "no coordinates for codec {}", resource.codec
                    )))?;
                let seam = measure_seam(&decoded, coords.start_frame, coords.end_frame);
                reports.push(ReverifyReport {
                    asset: asset.id.clone(),
                    variant: variant.name.clone(),
                    codec: resource.codec.clone(),
                    file: resource.file.clone(),
                    decoded_frames: decoded.frames(),
                    seam: seam.into(),
                });
            }
        }
    }
    Ok(reports)
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ReverifyReport {
    pub asset: String,
    pub variant: String,
    pub codec: String,
    pub file: String,
    pub decoded_frames: usize,
    pub seam: SeamMetricsJson,
}

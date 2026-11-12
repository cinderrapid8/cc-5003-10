//! Release-time support: offline preflight, real headless audition renders and
//! manifest summaries.
//!
//! Everything in this module works against files on disk and decoded PCM only.
//! It is the programmable backbone of the packaging scripts, so a release can
//! never be a web page that opens while its audio resources are missing or
//! undecodable: a missing source/resource or a decoder failure is an error.

use crate::dsp::measure_seam;
use crate::error::{LooperError, Result};
use crate::io;
use crate::manifest::Manifest;
use crate::model::{Project, VariantSpec};
use serde::Serialize;
use std::path::Path;

/// Outcome of the static release preflight. Every check that can fail before
/// audio is touched is recorded here with an explicit message.
#[derive(Clone, Debug, Serialize)]
pub struct PreflightReport {
    pub project_name: String,
    pub assets: usize,
    pub variants: usize,
    /// Every source file referenced by assets or independent variants.
    pub referenced_sources: Vec<String>,
    pub missing_sources: Vec<String>,
    pub checks: Vec<PreflightCheck>,
    pub ok: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PreflightCheck {
    pub scope: String,
    pub kind: String,
    pub detail: String,
    pub ok: bool,
}

impl PreflightReport {
    fn fail(&mut self, scope: &str, kind: &str, detail: String) {
        self.ok = false;
        self.checks.push(PreflightCheck {
            scope: scope.to_string(),
            kind: kind.to_string(),
            detail,
            ok: false,
        });
    }

    fn pass(&mut self, scope: &str, kind: &str, detail: String) {
        self.checks.push(PreflightCheck {
            scope: scope.to_string(),
            kind: kind.to_string(),
            detail,
            ok: true,
        });
    }
}

/// Static preflight for a packaged project.
///
/// Checks, independently of the build:
/// * every referenced source file exists under `base_dir` (a missing source is
///   a hard failure, never a silently skipped asset);
/// * loop regions stay inside the decoded source and keep a legal ordering,
///   i.e. the loop boundary is expressible at all;
/// * every variant is either explicitly parametric or points at an existing
///   independent source — display names alone never define a variant;
/// * packed resources request meaningful skip/padding, otherwise the decoded
///   coordinate space would be indistinguishable from the master.
///
/// Decoding each source also proves the decoder needed at build time is
/// actually present in this environment.
pub fn preflight_project(project: &Project, base_dir: &Path) -> Result<PreflightReport> {
    let mut report = PreflightReport {
        project_name: project.name.clone(),
        assets: project.assets.len(),
        variants: 0,
        referenced_sources: Vec::new(),
        missing_sources: Vec::new(),
        checks: Vec::new(),
        ok: true,
    };

    let mut seen_sources = std::collections::HashSet::new();
    let mut note_source = |rel: &str, report: &mut PreflightReport| {
        if seen_sources.insert(rel.to_string()) {
            report.referenced_sources.push(rel.to_string());
            let path = base_dir.join(rel);
            if !path.is_file() {
                report.missing_sources.push(rel.to_string());
                report.fail(
                    rel,
                    "missing_source",
                    format!("referenced source {} does not exist under {}", rel, base_dir.display()),
                );
            } else {
                report.pass(rel, "source_present", format!("source file readable: {rel}"));
            }
        }
    };

    let mut asset_ids = std::collections::HashSet::new();
    for asset in &project.assets {
        let scope = asset.id.clone();
        if asset.id.trim().is_empty() || !asset_ids.insert(asset.id.clone()) {
            report.fail(&scope, "asset_id", "asset id is empty or duplicated".into());
        }
        note_source(&asset.source, &mut report);

        // Decode the primary source so a missing/incompatible decoder fails
        // the release instead of being discovered by the end user.
        let source_path = base_dir.join(&asset.source);
        let source_buffer = if source_path.is_file() {
            match io::read_wav(&source_path) {
                Ok(buf) => Some(buf),
                Err(_) => match io::decode_any(&source_path) {
                    Ok(buf) => Some(buf),
                    Err(e) => {
                        report.fail(
                            &scope,
                            "decoder",
                            format!("cannot decode {}: {e}", source_path.display()),
                        );
                        None
                    }
                },
            }
        } else {
            None
        };

        if let Some(buf) = &source_buffer {
            if let Some(region) = &asset.region {
                check_region(&mut report, &scope, region, buf.frames());
            } else {
                report.pass(&scope, "region", format!("full-length region over {} decoded frames", buf.frames()));
            }
        }

        if asset.packed_delay_frames == 0 && asset.packed_padding_frames == 0 {
            report.fail(
                &scope,
                "packed_config",
                "packed resource needs non-zero delay or padding".into(),
            );
        } else {
            report.pass(
                &scope,
                "packed_config",
                format!(
                    "packed skip={} pad={} frames",
                    asset.packed_delay_frames, asset.packed_padding_frames
                ),
            );
        }

        let mut variant_names = std::collections::HashSet::new();
        for variant in &asset.variants {
            report.variants += 1;
            let vscope = format!("{}/{}", asset.id, variant.name());
            if !variant_names.insert(variant.name().to_string()) {
                report.fail(&vscope, "variant_name", "duplicate variant name".into());
            }
            match variant {
                VariantSpec::Parametric {
                    name,
                    gain,
                    resample_ratio,
                } => {
                    if !(gain.is_finite() && *gain > 0.0) {
                        report.fail(&vscope, "variant_param", format!("{name}: gain must be > 0"));
                    }
                    if !(resample_ratio.is_finite() && *resample_ratio > 0.0) {
                        report.fail(
                            &vscope,
                            "variant_param",
                            format!("{name}: resample_ratio must be > 0"),
                        );
                    }
                    if gain.is_finite() && *gain > 0.0 && resample_ratio.is_finite() && *resample_ratio > 0.0 {
                        report.pass(
                            &vscope,
                            "variant_parametric",
                            format!("gain={gain} resample={resample_ratio}"),
                        );
                    }
                }
                VariantSpec::Independent {
                    name: _,
                    source,
                    region,
                    ..
                } => {
                    note_source(source, &mut report);
                    let ind_path = base_dir.join(source);
                    if let Ok(buf) = io::read_wav(&ind_path).or_else(|_| io::decode_any(&ind_path)) {
                        match region {
                            Some(r) => check_region(&mut report, &vscope, r, buf.frames()),
                            None => report.pass(
                                &vscope,
                                "region",
                                format!("full-length region over {} decoded frames", buf.frames()),
                            ),
                        }
                    } else if ind_path.is_file() {
                        report.fail(
                            &vscope,
                            "decoder",
                            format!("cannot decode independent source {}", ind_path.display()),
                        );
                    }
                }
            }
        }
    }

    Ok(report)
}

fn check_region(
    report: &mut PreflightReport,
    scope: &str,
    region: &crate::model::LoopRegion,
    frames: usize,
) {
    if region.start_frame >= frames
        || region.end_frame > frames
        || region.end_frame <= region.start_frame + 1
    {
        report.fail(
            scope,
            "loop_boundary",
            format!(
                "region [{}, {}) out of range for {} decoded frames",
                region.start_frame, region.end_frame, frames
            ),
        );
    } else {
        report.pass(
            scope,
            "loop_boundary",
            format!(
                "region [{}, {}) inside {} decoded frames, search window ±{}",
                region.start_frame, region.end_frame, frames, region.search_window
            ),
        );
    }
}

/// One rendered audition: the decoded resource, the loop segment actually
/// repeated and the seam measured across the wrap in decoded coordinates.
#[derive(Clone, Debug, Serialize)]
pub struct AuditionRender {
    pub asset: String,
    pub variant: String,
    pub codec: String,
    pub source_file: String,
    pub output_file: String,
    pub rounds: usize,
    pub decoded_frames: usize,
    pub loop_start_frame: usize,
    pub loop_end_frame: usize,
    pub loop_length_frames: usize,
    pub rendered_frames: usize,
    pub sample_rate: u32,
    pub channels: u16,
    pub seam_amplitude_jump: f32,
    pub seam_spectral_discontinuity: f32,
}

/// Render a real, listenable multi-round loop for every resource of every
/// variant. This is the headless equivalent of pressing "play loop" in the UI:
/// each resource is decoded from disk, the manifest's decoded-space
/// coordinates slice the loop, and the segment is concatenated `rounds` times
/// into a WAV that is itself re-decoded to prove the output is playable.
///
/// A missing resource file, a decode failure or coordinates outside the
/// decoded stream all return an error — an empty shell cannot pass.
pub fn render_auditions(
    manifest: &Manifest,
    out_dir: &Path,
    audition_dir: &Path,
    rounds: usize,
) -> Result<Vec<AuditionRender>> {
    if rounds == 0 {
        return Err(LooperError::Project("audition rounds must be >= 1".to_string()));
    }
    io::ensure_dir(audition_dir)?;
    let mut renders = Vec::new();

    for asset in &manifest.assets {
        for variant in &asset.variants {
            for resource in &variant.resources {
                let resource_path = out_dir.join(&resource.file);
                if !resource_path.is_file() {
                    return Err(LooperError::Decode {
                        path: resource_path,
                        message: "resource listed in manifest is missing on disk".to_string(),
                    });
                }
                let decoded = io::decode_any(&resource_path)?;
                if decoded.frames() != resource.decoded_frames {
                    return Err(LooperError::Decode {
                        path: resource_path.clone(),
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
                    .ok_or_else(|| {
                        LooperError::Project(format!(
                            "manifest has no loop coordinates for codec {}",
                            resource.codec
                        ))
                    })?;
                if coords.end_frame > decoded.frames() || coords.start_frame >= coords.end_frame {
                    return Err(LooperError::Decode {
                        path: resource_path.clone(),
                        message: format!(
                            "loop coordinates [{}, {}) fall outside {} decoded frames",
                            coords.start_frame, coords.end_frame, decoded.frames()
                        ),
                    });
                }

                let channels = decoded.channels as usize;
                let loop_len = coords.end_frame - coords.start_frame;
                let mut rendered =
                    Vec::with_capacity(loop_len * channels * rounds);
                let segment = &decoded.samples
                    [coords.start_frame * channels..coords.end_frame * channels];
                for _ in 0..rounds {
                    rendered.extend_from_slice(segment);
                }
                let output = crate::model::AudioBuffer {
                    sample_rate: decoded.sample_rate,
                    channels: decoded.channels,
                    samples: rendered,
                };
                let seam = measure_seam(&output, 0, loop_len);

                let out_name = format!(
                    "audition__{}__{}__{}__{}x.wav",
                    asset.id, variant.name, resource.codec, rounds
                );
                let out_path = audition_dir.join(&out_name);
                io::write_wav(&out_path, &output)?;
                // Re-decode the rendered file: the deliverable itself must be
                // decodable through the same path the runtime player uses.
                let replay = io::decode_any(&out_path)?;
                if replay.frames() != loop_len * rounds {
                    return Err(LooperError::Decode {
                        path: out_path.clone(),
                        message: format!(
                            "audition render decoded to {} frames, expected {}",
                            replay.frames(),
                            loop_len * rounds
                        ),
                    });
                }

                renders.push(AuditionRender {
                    asset: asset.id.clone(),
                    variant: variant.name.clone(),
                    codec: resource.codec.clone(),
                    source_file: resource.file.clone(),
                    output_file: out_name,
                    rounds,
                    decoded_frames: decoded.frames(),
                    loop_start_frame: coords.start_frame,
                    loop_end_frame: coords.end_frame,
                    loop_length_frames: loop_len,
                    rendered_frames: replay.frames(),
                    sample_rate: decoded.sample_rate,
                    channels: decoded.channels,
                    seam_amplitude_jump: seam.amplitude_jump,
                    seam_spectral_discontinuity: seam.spectral_discontinuity,
                });
            }
        }
    }
    Ok(renders)
}

/// Compact, machine-readable summary of a built manifest. Used by the launch
/// self-check and the go-live verification so operators read actual loop
/// output numbers rather than just confirming a process is alive.
#[derive(Clone, Debug, Serialize)]
pub struct ManifestSummary {
    pub schema_version: u32,
    pub project_name: String,
    pub builder_version: String,
    pub assets: usize,
    pub variants: usize,
    pub resources: usize,
    pub codecs: Vec<String>,
    pub total_resource_bytes: u64,
    pub loops: Vec<LoopSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LoopSummary {
    pub asset: String,
    pub variant: String,
    pub codec: String,
    pub file: String,
    pub decoded_frames: usize,
    pub measured_delay_frames: usize,
    pub measured_padding_frames: usize,
    pub loop_start_frame: usize,
    pub loop_end_frame: usize,
    pub loop_length_frames: usize,
    pub amplitude_jump: f32,
}

pub fn summarize_manifest(manifest: &Manifest) -> ManifestSummary {
    let mut codecs = std::collections::BTreeSet::new();
    let mut resources = 0usize;
    let mut total_bytes = 0u64;
    let mut loops = Vec::new();

    for asset in &manifest.assets {
        for variant in &asset.variants {
            for resource in &variant.resources {
                resources += 1;
                total_bytes += resource.bytes;
                codecs.insert(resource.codec.clone());
                let coords = variant
                    .loop_record
                    .resource_coordinates
                    .iter()
                    .find(|c| c.codec == resource.codec);
                let (start, end, length) = coords
                    .map(|c| (c.start_frame, c.end_frame, c.loop_length_frames))
                    .unwrap_or((0, 0, 0));
                loops.push(LoopSummary {
                    asset: asset.id.clone(),
                    variant: variant.name.clone(),
                    codec: resource.codec.clone(),
                    file: resource.file.clone(),
                    decoded_frames: resource.decoded_frames,
                    measured_delay_frames: resource.measured_delay_frames,
                    measured_padding_frames: resource.measured_padding_frames,
                    loop_start_frame: start,
                    loop_end_frame: end,
                    loop_length_frames: length,
                    amplitude_jump: variant.loop_record.master_seam.amplitude_jump,
                });
            }
        }
    }

    ManifestSummary {
        schema_version: manifest.schema_version,
        project_name: manifest.project_name.clone(),
        builder_version: manifest.builder_version.clone(),
        assets: manifest.assets.len(),
        variants: manifest
            .assets
            .iter()
            .map(|a| a.variants.len())
            .sum(),
        resources,
        codecs: codecs.into_iter().collect(),
        total_resource_bytes: total_bytes,
        loops,
    }
}

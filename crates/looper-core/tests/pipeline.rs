use looper_core::dsp::{apply_crossfade, measure_seam, plan_fade, rank_candidates, verify_loop};
use looper_core::model::FadeCurve;
use looper_core::io;
use looper_core::manifest::ProgressEvent;
use looper_core::model::{AssetSpec, FadeCurve as ModelFade, LoopRegion, Project, VariantSpec};
use looper_core::pipeline::{build, load_project, reverify_manifest, save_project};
use looper_core::synth;
use std::collections::HashSet;
use tempfile::tempdir;

fn project_for(dir: &std::path::Path, fade_ms: f64) -> Project {
    let names = synth::write_fixture_set(dir).unwrap();
    assert_eq!(names.len(), 3);
    Project {
        name: "test-bank".to_string(),
        schema_version: Project::SCHEMA_VERSION,
        assets: vec![AssetSpec {
            id: "engine".to_string(),
            source: "engine_dc_offset.wav".to_string(),
            source_sha256: None,
            region: Some(LoopRegion {
                start_frame: 4800,
                end_frame: 80000,
                search_window: 512,
            }),
            fade_ms,
            fade_curve: ModelFade::EqualPower,
            min_keep_frames: 256,
            variants: vec![
                VariantSpec::Parametric {
                    name: "low".to_string(),
                    gain: 0.5,
                    resample_ratio: 1.0,
                },
                VariantSpec::Independent {
                    name: "ind".to_string(),
                    source: "ambient_stereo_phase.wav".to_string(),
                    source_sha256: None,
                    region: Some(LoopRegion {
                        start_frame: 4800,
                        end_frame: 72000,
                        search_window: 256,
                    }),
                    fade_ms: 40.0,
                    fade_curve: ModelFade::Linear,
                    min_keep_frames: 128,
                },
            ],
            encode_lossy: true,
            encode_packed: true,
            packed_delay_frames: 1024,
            packed_padding_frames: 768,
        }],
    }
}

#[test]
fn build_produces_manifest_and_real_audio() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let out = dir.path().join("out");
    let project = project_for(&src, 30.0);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let ev2 = events.clone();
    let (manifest, manifest_path) = build(
        &project,
        &src,
        &out,
        &move |ev| ev2.lock().unwrap().push(ev),
    )
    .unwrap();

    assert!(manifest_path.exists());
    let stages: HashSet<String> = events
        .lock()
        .unwrap()
        .iter()
        .map(|e| match e {
            ProgressEvent::Encoding { codec, .. } => format!("encoding:{codec}"),
            ProgressEvent::Verifying { codec, .. } => format!("verifying:{codec}"),
            ProgressEvent::Finished { .. } => "finished".into(),
            _ => "other".into(),
        })
        .collect();
    assert!(stages.contains("encoding:wav_lossy_q8"));
    assert!(stages.contains("verifying:wav_lossy_q8"));
    assert!(stages.contains("verifying:wav_packed_skip_pad"));
    assert!(stages.contains("finished"));

    // Three variants (base + 2), each with WAV + FLAC + packed = 9 files.
    let asset = &manifest.assets[0];
    assert_eq!(asset.variants.len(), 3);
    for v in &asset.variants {
        assert_eq!(v.resources.len(), 3);
        for r in &v.resources {
            let p = out.join(&r.file);
            assert!(p.exists(), "missing {}", r.file);
            assert_eq!(io::sha256_file(&p).unwrap(), r.sha256);
        }
    }
}

#[test]
fn loop_indices_are_sample_accurate_and_traceable() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let out = dir.path().join("out");
    let project = project_for(&src, 30.0);
    let (manifest, _) = build(&project, &src, &out, &|_| {}).unwrap();
    let base = &manifest.assets[0].variants[0];
    let lr = &base.loop_record;

    // Accepted end stays inside the search window.
    let diff = lr.accepted_end_frame.abs_diff(lr.requested_end_frame);
    assert!(diff <= 512);
    assert_eq!(lr.source_start_frame, 4800);

    // Fade intervals and final loop length are fully traceable.
    let fade = &lr.fade;
    assert_eq!(fade.head_lo, lr.source_start_frame);
    assert_eq!(fade.tail_hi, lr.accepted_end_frame);
    assert_eq!(
        fade.loop_length_frames,
        lr.accepted_end_frame - lr.source_start_frame
    );
    assert_eq!(
        fade.unblended_frames,
        fade.tail_lo - fade.head_hi
    );

    // Packed resource coordinates include the measured encoder delay and
    // stay within the decoded stream.
    let packed = lr
        .resource_coordinates
        .iter()
        .find(|c| c.codec == "wav_packed_skip_pad")
        .unwrap();
    let packed_res = base
        .resources
        .iter()
        .find(|r| r.codec == "wav_packed_skip_pad")
        .unwrap();
    assert!(packed_res.measured_delay_frames > 0, "pre-skip must be measured");
    assert!(packed_res.measured_padding_frames > 0);
    assert_eq!(packed.start_frame, packed_res.measured_delay_frames);
    assert!(packed.end_frame <= packed_res.decoded_frames);
    assert_eq!(packed.loop_length_frames, fade.loop_length_frames);
}

#[test]
fn zero_crossing_is_only_a_candidate_condition() {
    let sample = synth::mechanical_noninteger_period();
    let buf = &sample.buffer;
    // Request an end that is clearly mid-period; the ranking must find a
    // better neighbor and report per-channel + spectral metrics.
    let ranked = rank_candidates(buf, 2400, 60000, 384);
    assert!(ranked.len() > 100);
    let winner = ranked.iter().min_by(|a, b| a.score.total_cmp(&b.score)).unwrap();
    assert!(!winner.metrics.amplitude_jump_per_channel.is_empty());
    assert!(winner.metrics.spectral_discontinuity >= 0.0);
    // The winner need not be a zero crossing.
    let best_jump = ranked
        .iter()
        .map(|c| c.metrics.amplitude_jump)
        .fold(f32::INFINITY, f32::min);
    assert!(winner.metrics.amplitude_jump <= best_jump + 1e-6);
}

#[test]
fn short_segment_is_refused_without_widening() {
    let sample = synth::mechanical_noninteger_period();
    let buf = &sample.buffer;
    // 200-frame region with a 50 ms fade (~2400 frames): must be rejected.
    let result = plan_fade(1000, 1200, 2400, 1, FadeCurve::EqualPower, "tiny");
    assert!(matches!(
        result,
        Err(looper_core::LooperError::SegmentTooShort { .. })
    ));

    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let mut project = project_for(&src, 50.0);
    project.assets[0].region = Some(LoopRegion {
        start_frame: 0,
        end_frame: 100,
        search_window: 4,
    });
    let out = dir.path().join("out");
    let err = build(&project, &src, &out, &|_| {}).unwrap_err();
    assert!(matches!(
        err,
        looper_core::LooperError::SegmentTooShort { .. }
    ));
    // Nothing was emitted for the rejected asset.
    assert!(out.exists());
    assert!(std::fs::read_dir(&out).unwrap().next().is_none());
    let _ = buf;
}

#[test]
fn crossfade_improves_aligned_seam_and_keeps_length() {
    let sample = synth::mechanical_noninteger_period();
    let buf = &sample.buffer;
    // Pick a loop length that is near an integer number of the 73.7-frame
    // period so head and tail are in phase — the regime crossfade is built
    // for. A length that is badly misaligned against the period cannot be
    // repaired by blending (head and tail are then anti-phased), and the
    // builder never pretends otherwise.
    let start = 2400usize;
    let end = start + (400f32 * 73.7).round() as usize;
    let raw = measure_seam(buf, start, end);
    // A fade spanning two signal periods (148 ≈ 2 × 73.7) is the regime where
    // crossfade provably tightens the seam. A fade much longer than the period
    // blends anti-phased material and cannot help — the builder reports the
    // measured seam either way rather than claiming a fix it did not make.
    let fade = 148usize;
    let plan = plan_fade(start, end, fade, 128, FadeCurve::Linear, "x").unwrap();
    let sliced = looper_core::model::AudioBuffer {
        sample_rate: buf.sample_rate,
        channels: buf.channels,
        samples: buf.samples[start * buf.channels as usize..end * buf.channels as usize].to_vec(),
    };
    let local = looper_core::dsp::FadePlan {
        head_lo: 0,
        head_hi: fade,
        tail_lo: end - start - fade,
        tail_hi: end - start,
        ..plan
    };
    let looped = apply_crossfade(&sliced, &local);
    assert_eq!(looped.frames(), end - start);
    // Unblended middle is untouched.
    for f in fade..end - start - fade {
        assert_eq!(looped.sample(f, 0), sliced.sample(f, 0));
    }
    let wrapped = verify_loop(&looped);
    assert!(
        wrapped.amplitude_jump < raw.amplitude_jump,
        "aligned crossfade should reduce seam: raw={} wrapped={}",
        raw.amplitude_jump,
        wrapped.amplitude_jump
    );
}

#[test]
fn rebuild_is_deterministic_and_verifiable() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let out1 = dir.path().join("out1");
    let out2 = dir.path().join("out2");
    let project = project_for(&src, 30.0);
    let project_path = dir.path().join("project.json");
    save_project(&project, &project_path).unwrap();
    let reloaded = load_project(&project_path).unwrap();
    assert_eq!(reloaded, project);

    let (m1, _) = build(&project, &src, &out1, &|_| {}).unwrap();
    let (m2, _) = build(&reloaded, &src, &out2, &|_| {}).unwrap();
    assert_eq!(m1, m2);

    // File names are content-derived: same set and same hashes.
    let mut h1: Vec<String> = m1.assets.iter().flat_map(|a| {
        a.variants
            .iter()
            .flat_map(|v| v.resources.iter().map(|r| r.sha256.clone()))
    }).collect();
    let mut h2: Vec<String> = m2.assets.iter().flat_map(|a| {
        a.variants
            .iter()
            .flat_map(|v| v.resources.iter().map(|r| r.sha256.clone()))
    }).collect();
    h1.sort();
    h2.sort();
    assert_eq!(h1, h2);

    // Re-decode every resource and verify against manifest coordinates.
    let reports = reverify_manifest(&m1, &out1).unwrap();
    assert_eq!(reports.len(), 9);
    for r in reports {
        assert!(r.seam.amplitude_jump.is_finite());
    }
}

#[test]
fn same_filename_with_wrong_hash_is_rejected() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let mut project = project_for(&src, 30.0);
    project.assets[0].source_sha256 = Some("deadbeef".repeat(8));
    let out = dir.path().join("out");
    let err = build(&project, &src, &out, &|_| {}).unwrap_err();
    assert!(err.to_string().contains("hash mismatch"));
}

#[test]
fn independent_variant_keeps_its_own_identity() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let out = dir.path().join("out");
    let project = project_for(&src, 30.0);
    let (manifest, _) = build(&project, &src, &out, &|_| {}).unwrap();
    let ind = &manifest.assets[0].variants[2];
    match &ind.provenance {
        looper_core::manifest::VariantProvenance::Independent { source, .. } => {
            assert!(source.contains("ambient_stereo_phase"));
        }
        other => panic!("expected independent provenance, got {other:?}"),
    }
    let parametric = &manifest.assets[0].variants[1];
    match &parametric.provenance {
        looper_core::manifest::VariantProvenance::Parametric { gain, .. } => {
            assert!((*gain - 0.5).abs() < 1e-6);
        }
        other => panic!("expected parametric provenance, got {other:?}"),
    }
}

#[test]
fn dc_offset_is_reported_in_manifest() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let out = dir.path().join("out");
    let project = project_for(&src, 30.0);
    let (manifest, _) = build(&project, &src, &out, &|_| {}).unwrap();
    let base = &manifest.assets[0].variants[0];
    assert!(base.dc_offset_per_channel[0].abs() > 0.05);
}

#[test]
fn encoded_loop_is_verified_on_decoded_pcm_not_wav_length() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let out = dir.path().join("out");
    let project = project_for(&src, 30.0);
    let (manifest, _) = build(&project, &src, &out, &|_| {}).unwrap();
    let base = &manifest.assets[0].variants[0];
    let packed_res = base
        .resources
        .iter()
        .find(|r| r.codec == "wav_packed_skip_pad")
        .unwrap();
    // The decoded packed stream is longer than the WAV master due to skip+pad.
    let wav_res = base.resources.iter().find(|r| r.codec == "wav_pcm_s16").unwrap();
    assert!(packed_res.decoded_frames > wav_res.decoded_frames);
    // The manifest stores the seam measured on the decoded stream.
    let encoded_seam = base.loop_record.encoded_seam.as_ref().unwrap();
    assert!(encoded_seam.amplitude_jump.is_finite());
    // reverify_manifest rejects frame-count drift if the file were swapped.
    let reports = reverify_manifest(&manifest, &out).unwrap();
    let packed_report = reports
        .iter()
        .find(|r| r.codec == "wav_packed_skip_pad")
        .unwrap();
    assert_eq!(packed_report.decoded_frames, packed_res.decoded_frames);

    // Lossy WAV uses the same container: decoded frame count equals master.
    let lossy_res = base
        .resources
        .iter()
        .find(|r| r.codec == "wav_lossy_q8")
        .unwrap();
    assert_eq!(lossy_res.decoded_frames, wav_res.decoded_frames);
    assert_eq!(lossy_res.measured_delay_frames, 0);
}

use looper_core::model::{AssetSpec, FadeCurve as ModelFade, LoopRegion, Project, VariantSpec};
use looper_core::{build, preflight_project, render_auditions, summarize_manifest, synth};
use tempfile::tempdir;

fn project_for(dir: &std::path::Path) -> Project {
    synth::write_fixture_set(dir).unwrap();
    Project {
        name: "release-bank".to_string(),
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
            fade_ms: 30.0,
            fade_curve: ModelFade::EqualPower,
            min_keep_frames: 256,
            variants: vec![VariantSpec::Parametric {
                name: "low".to_string(),
                gain: 0.5,
                resample_ratio: 0.9,
            }],
            encode_lossy: true,
            encode_packed: true,
            packed_delay_frames: 1024,
            packed_padding_frames: 768,
        }],
    }
}

#[test]
fn preflight_passes_for_complete_project() {
    let dir = tempdir().unwrap();
    let src = dir.path().to_path_buf();
    let project = project_for(&src);
    let report = preflight_project(&project, &src).unwrap();
    assert!(report.ok, "all checks should pass: {:?}", report.checks);
    assert_eq!(report.missing_sources.len(), 0);
    assert!(report.checks.iter().any(|c| c.kind == "loop_boundary" && c.ok));
    assert!(report.checks.iter().any(|c| c.kind == "packed_config" && c.ok));
}

#[test]
fn preflight_flags_missing_source_and_bad_boundary() {
    let dir = tempdir().unwrap();
    let mut project = project_for(dir.path());
    project.assets[0].source = "nope.wav".to_string();
    project.assets[0].region = Some(LoopRegion {
        start_frame: 100,
        end_frame: 50,
        search_window: 4,
    });
    let report = preflight_project(&project, dir.path()).unwrap();
    assert!(!report.ok);
    assert!(report.missing_sources.contains(&"nope.wav".to_string()));
    assert!(report
        .checks
        .iter()
        .any(|c| c.kind == "missing_source" && !c.ok));
}

#[test]
fn preflight_flags_undecodable_source() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("garbage.wav"), vec![0u8; 256]).unwrap();
    let mut project = project_for(dir.path());
    project.assets[0].source = "garbage.wav".to_string();
    let report = preflight_project(&project, dir.path()).unwrap();
    assert!(!report.ok);
    assert!(report.checks.iter().any(|c| c.kind == "decoder" && !c.ok));
}

#[test]
fn audition_renders_one_real_loop_per_resource_and_rejects_missing() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let out = dir.path().join("out");
    let audition = dir.path().join("audition");
    let project = project_for(&src);
    let (manifest, _) = build(&project, &src, &out, &|_| {}).unwrap();

    let rounds = 3;
    let renders = render_auditions(&manifest, &out, &audition, rounds).unwrap();
    let resource_total: usize = manifest
        .assets
        .iter()
        .map(|a| a.variants.iter().map(|v| v.resources.len()).sum::<usize>())
        .sum();
    assert_eq!(renders.len(), resource_total);
    // One distinct WAV per render (codec is part of the file name).
    let mut names: Vec<String> = renders.iter().map(|r| r.output_file.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), resource_total);
    for r in &renders {
        assert_eq!(r.rendered_frames, r.loop_length_frames * rounds);
        assert!(r.loop_end_frame <= r.decoded_frames);
        assert!(r.seam_amplitude_jump.is_finite());
        assert!(audition.join(&r.output_file).is_file());
    }

    // Removing a resource turns the audition into a hard error.
    let victim = manifest.assets[0].variants[0].resources[0]
        .file
        .clone();
    std::fs::remove_file(out.join(&victim)).unwrap();
    let err = render_auditions(&manifest, &out, &dir.path().join("audition2"), rounds).unwrap_err();
    assert!(err.to_string().contains("missing on disk"));
}

#[test]
fn summary_counts_resources_and_keeps_coordinates_in_bounds() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let out = dir.path().join("out");
    let project = project_for(&src);
    let (manifest, _) = build(&project, &src, &out, &|_| {}).unwrap();
    let summary = summarize_manifest(&manifest);
    assert_eq!(summary.assets, 1);
    assert_eq!(summary.variants, 2); // base + one parametric
    assert!(summary.resources >= 6);
    assert!(!summary.codecs.is_empty());
    for l in &summary.loops {
        assert!(l.loop_length_frames > 0);
        assert!(l.loop_end_frame <= l.decoded_frames);
    }
    let json = serde_json::to_string(&summary).unwrap();
    assert!(json.contains("loop_length_frames"));
}

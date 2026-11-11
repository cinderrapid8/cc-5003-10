use looper_core::manifest::ProgressEvent;
use looper_core::model::{
    AssetSpec, FadeCurve, LoopRegion, Project, VariantSpec,
};
use looper_core::{build, load_project, reverify_manifest, save_project, synth};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn usage() -> &'static str {
    "looper-cli — game loop soundbank builder\n\n\
USAGE:\n  looper-cli <COMMAND> [ARGS]\n\n\
COMMANDS:\n  \
  synth <DIR>                Write deterministic synthetic test WAVs\n  \
  init <PROJECT.json> <SRC_DIR>  Create an example project over synth fixtures\n  \
  build <PROJECT.json> <BASE_DIR> <OUT_DIR>\n  \
                             Build the soundbank (WAV masters + OGG), print progress\n  \
  verify <MANIFEST.json> <OUT_DIR>\n  \
                             Re-decode every resource and verify its loop seam\n  \
  help\n"
}

fn print_progress(ev: &ProgressEvent) {
    match ev {
        ProgressEvent::Started { total_assets } => {
            println!("[start] {total_assets} asset(s)");
        }
        ProgressEvent::AssetStarted { asset, index } => {
            println!("[asset {index}] {asset}");
        }
        ProgressEvent::SourceLoaded { asset, frames } => {
            println!("  loaded {asset}: {frames} decoded frames");
        }
        ProgressEvent::Searching { asset, .. } => {
            println!("  searching loop candidates for {asset} (zero crossing = candidate only)");
        }
        ProgressEvent::Fading {
            asset,
            variant,
            fade_frames,
        } => println!("  crossfade {asset}/{variant}: {fade_frames} frames per side"),
        ProgressEvent::Encoding {
            asset,
            variant,
            codec,
        } => println!("  encoding {asset}/{variant} -> {codec}"),
        ProgressEvent::Verifying {
            asset,
            variant,
            codec,
        } => println!("  verifying decoded {codec} seam for {asset}/{variant}"),
        ProgressEvent::AssetDone { asset, index } => {
            println!("[done {index}] {asset}");
        }
        ProgressEvent::Finished { resources, manifest } => {
            println!("[finished] {resources} resource file(s), manifest {manifest}");
        }
        ProgressEvent::Rejected { asset, reason } => {
            eprintln!("[rejected] {asset}: {reason}");
        }
    }
}

fn example_project(src_dir: &str) -> Project {
    Project {
        name: "game-loop-demo".to_string(),
        schema_version: Project::SCHEMA_VERSION,
        assets: vec![
            AssetSpec {
                id: "engine_idle".to_string(),
                source: format!("{src_dir}/engine_dc_offset.wav"),
                source_sha256: None,
                region: Some(LoopRegion {
                    start_frame: 4800,
                    end_frame: 80000,
                    search_window: 512,
                }),
                fade_ms: 30.0,
                fade_curve: FadeCurve::EqualPower,
                min_keep_frames: 256,
                variants: vec![
                    VariantSpec::Parametric {
                        name: "intensity_low".to_string(),
                        gain: 0.55,
                        resample_ratio: 0.92,
                    },
                    VariantSpec::Parametric {
                        name: "intensity_high".to_string(),
                        gain: 1.35,
                        resample_ratio: 1.08,
                    },
                ],
                encode_lossy: true,
                encode_packed: true,
                packed_delay_frames: 1024,
                packed_padding_frames: 768,
            },
            AssetSpec {
                id: "mechanical_hum".to_string(),
                source: format!("{src_dir}/mechanical_noninteger_period.wav"),
                source_sha256: None,
                region: Some(LoopRegion {
                    start_frame: 2400,
                    end_frame: 60000,
                    search_window: 384,
                }),
                fade_ms: 25.0,
                fade_curve: FadeCurve::EqualPower,
                min_keep_frames: 128,
                variants: vec![VariantSpec::Independent {
                    name: "hum_alt".to_string(),
                    source: format!("{src_dir}/ambient_stereo_phase.wav"),
                    source_sha256: None,
                    region: Some(LoopRegion {
                        start_frame: 4800,
                        end_frame: 72000,
                        search_window: 256,
                    }),
                    fade_ms: 40.0,
                    fade_curve: FadeCurve::Linear,
                    min_keep_frames: 128,
                }],
                encode_lossy: true,
                encode_packed: true,
                packed_delay_frames: 1024,
                packed_padding_frames: 768,
            },
            AssetSpec {
                id: "ambient_bed".to_string(),
                source: format!("{src_dir}/ambient_stereo_phase.wav"),
                source_sha256: None,
                region: None,
                fade_ms: 50.0,
                fade_curve: FadeCurve::EqualPower,
                min_keep_frames: 512,
                variants: vec![],
                encode_lossy: true,
                encode_packed: true,
                packed_delay_frames: 1024,
                packed_padding_frames: 768,
            },
        ],
    }
}

fn run() -> looper_core::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");
    match cmd {
        "help" | "-h" | "--help" => {
            print!("{}", usage());
            Ok(())
        }
        "synth" => {
            let dir = PathBuf::from(args.get(2).ok_or_else(|| looper_error_msg("missing DIR"))?);
            let names = synth::write_fixture_set(&dir)?;
            for n in names {
                println!("wrote {}", dir.join(n).display());
            }
            Ok(())
        }
        "init" => {
            let project_path = PathBuf::from(args.get(2).ok_or_else(|| looper_error_msg("missing PROJECT.json"))?);
            let src_dir = args.get(3).map(String::as_str).unwrap_or("samples");
            let project = example_project(src_dir);
            save_project(&project, &project_path)?;
            println!("wrote example project {}", project_path.display());
            Ok(())
        }
        "build" => {
            let project_path = Path::new(args.get(2).ok_or_else(|| looper_error_msg("missing PROJECT.json"))?);
            let base_dir = PathBuf::from(args.get(3).ok_or_else(|| looper_error_msg("missing BASE_DIR"))?);
            let out_dir = PathBuf::from(args.get(4).ok_or_else(|| looper_error_msg("missing OUT_DIR"))?);
            let project = load_project(project_path)?;
            let (manifest, path) = build(&project, &base_dir, &out_dir, &|ev| print_progress(&ev))?;
            println!(
                "manifest {}: {} asset(s), {} resource(s)",
                path.display(),
                manifest.assets.len(),
                manifest
                    .assets
                    .iter()
                    .map(|a| a.variants.iter().map(|v| v.resources.len()).sum::<usize>())
                    .sum::<usize>()
            );
            Ok(())
        }
        "verify" => {
            let manifest_path = Path::new(args.get(2).ok_or_else(|| looper_error_msg("missing MANIFEST.json"))?);
            let out_dir = PathBuf::from(args.get(3).ok_or_else(|| looper_error_msg("missing OUT_DIR"))?);
            let manifest: looper_core::Manifest = serde_json::from_slice(
                &std::fs::read(manifest_path).map_err(|e| looper_core::LooperError::io(manifest_path, e))?,
            )
            .map_err(|e| looper_error_msg(&e.to_string()))?;
            let reports = reverify_manifest(&manifest, &out_dir)?;
            for r in &reports {
                println!(
                    "{}/{} [{}] decoded={} jump={:.6} spectral={:.6} zc={}",
                    r.asset,
                    r.variant,
                    r.codec,
                    r.decoded_frames,
                    r.seam.amplitude_jump,
                    r.seam.spectral_discontinuity,
                    r.seam.zero_crossing
                );
            }
            println!("verified {} resource(s)", reports.len());
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}\n{}", usage());
            Err(looper_error_msg("unknown command"))
        }
    }
}

fn looper_error_msg(msg: &str) -> looper_core::LooperError {
    looper_core::LooperError::Project(msg.to_string())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

//! Tauri command surface. Every command is a thin wrapper over looper-core so
//! the desktop app, the demo server and the CLI share identical behavior.

use looper_core::manifest::ProgressEvent;
use looper_core::model::Project;
use looper_core::{build, load_project, reverify_manifest, save_project, synth};
use std::path::PathBuf;

#[derive(serde::Serialize)]
pub struct CommandError {
    message: String,
}

impl From<looper_core::LooperError> for CommandError {
    fn from(e: looper_core::LooperError) -> Self {
        Self {
            message: e.to_string(),
        }
    }
}

pub type CmdResult<T> = Result<T, CommandError>;

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn synth_fixtures(dir: String) -> CmdResult<Vec<String>> {
    Ok(synth::write_fixture_set(&PathBuf::from(dir))?)
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn save_project_file(path: String, project: Project) -> CmdResult<()> {
    Ok(save_project(&project, &PathBuf::from(path))?)
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn load_project_file(path: String) -> CmdResult<Project> {
    Ok(load_project(&PathBuf::from(path))?)
}

/// Runs the build on a worker thread and streams `build-progress` events to
/// the window. The frontend receives the same ProgressEvent enum the CLI
/// prints, then a final `build-finished` (manifest) or `build-error`.
#[cfg(feature = "desktop")]
#[tauri::command]
pub fn run_build(
    app: tauri::AppHandle,
    project: Project,
    base_dir: String,
    out_dir: String,
) -> CmdResult<looper_core::Manifest> {
    let (manifest, _path) = build(
        &project,
        &PathBuf::from(base_dir),
        &PathBuf::from(out_dir),
        &|event: ProgressEvent| {
            let _ = app.emit("build-progress", event);
        },
    )?;
    Ok(manifest)
}

#[cfg(feature = "desktop")]
#[tauri::command]
pub fn verify_soundbank(
    manifest_path: String,
    out_dir: String,
) -> CmdResult<Vec<looper_core::ReverifyReport>> {
    let manifest: looper_core::Manifest =
        serde_json::from_slice(&std::fs::read(PathBuf::from(manifest_path))?)
            .map_err(|e| CommandError {
                message: e.to_string(),
            })?;
    Ok(reverify_manifest(&manifest, &PathBuf::from(out_dir))?)
}

#[cfg(feature = "desktop")]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            synth_fixtures,
            save_project_file,
            load_project_file,
            run_build,
            verify_soundbank,
        ])
        .run(tauri::generate_context!())
        .expect("tauri run");
}

/// Stub used when the crate is compiled without the native toolchain.
#[cfg(not(feature = "desktop"))]
pub fn run() {
    eprintln!("desktop feature disabled; use looper-cli or looper-server");
}

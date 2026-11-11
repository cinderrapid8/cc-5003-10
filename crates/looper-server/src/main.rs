use axum::body::Bytes;
use axum::extract::Query;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use looper_core::manifest::ProgressEvent;
use looper_core::model::Project;
use looper_core::{build, load_project, reverify_manifest, save_project, synth};
use serde::Deserialize;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

#[derive(Clone)]
struct AppState {
    #[allow(dead_code)]
    cwd: Arc<PathBuf>,
}

#[derive(Deserialize)]
struct DirBody {
    dir: String,
}

#[derive(Deserialize)]
struct BuildBody {
    project: Project,
    base_dir: String,
    out_dir: String,
}

#[derive(Deserialize)]
struct SaveBody {
    path: String,
    project: Project,
}

#[derive(Deserialize)]
struct PathQuery {
    path: String,
}

#[derive(Deserialize)]
struct VerifyQuery {
    manifest: String,
    out_dir: String,
}

async fn synth_fixtures(Json(body): Json<DirBody>) -> Json<serde_json::Value> {
    let dir = PathBuf::from(&body.dir);
    match tokio::task::spawn_blocking(move || synth::write_fixture_set(&dir)).await {
        Ok(Ok(names)) => Json(serde_json::json!({ "ok": true, "files": names })),
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn save_project_endpoint(Json(body): Json<SaveBody>) -> Json<serde_json::Value> {
    let path = PathBuf::from(&body.path);
    match tokio::task::spawn_blocking(move || save_project(&body.project, &path)).await {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })),
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn load_project_endpoint(Query(q): Query<PathQuery>) -> Json<serde_json::Value> {
    let path = PathBuf::from(&q.path);
    match tokio::task::spawn_blocking(move || load_project(&path)).await {
        Ok(Ok(project)) => Json(serde_json::json!({ "ok": true, "project": project })),
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

/// Build with progress streamed as Server-Sent Events. The final event is
/// `manifest` on success or `error` on rejection.
async fn build_stream(
    bytes: Bytes,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let body: BuildBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(e) => {
            let (tx, rx) = mpsc::channel(8);
            let _ = tx
                .send(Ok(Event::default().event("error").data(e.to_string())))
                .await;
            return Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default());
        }
    };

    let (tx, rx) = mpsc::channel(32);
    tokio::task::spawn_blocking(move || {
        let tx2 = tx.clone();
        let emit = move |ev: ProgressEvent| {
            let json = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
            let _ = tx.blocking_send(Ok(Event::default().event("progress").data(json)));
        };
        let result = build(
            &body.project,
            &PathBuf::from(&body.base_dir),
            &PathBuf::from(&body.out_dir),
            &emit,
        );
        match result {
            Ok((manifest, path)) => {
                let payload = serde_json::json!({
                    "manifest": manifest,
                    "manifest_path": path.to_string_lossy(),
                });
                let _ = tx2.blocking_send(Ok(Event::default()
                    .event("manifest")
                    .data(serde_json::to_string(&payload).unwrap())));
            }
            Err(e) => {
                let _ = tx2
                    .blocking_send(Ok(Event::default().event("error").data(e.to_string())));
            }
        }
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

async fn verify_endpoint(Query(q): Query<VerifyQuery>) -> Json<serde_json::Value> {
    let manifest_path = PathBuf::from(&q.manifest);
    let out_dir = PathBuf::from(&q.out_dir);
    let task = tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&manifest_path)
            .map_err(|e| looper_core::LooperError::io(&manifest_path, e))?;
        let manifest: looper_core::Manifest = serde_json::from_slice(&bytes)
            .map_err(|e| looper_core::LooperError::Project(e.to_string()))?;
        reverify_manifest(&manifest, &out_dir)
    })
    .await;
    match task {
        Ok(Ok(reports)) => Json(serde_json::json!({ "ok": true, "reports": reports })),
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn read_manifest(Query(q): Query<PathQuery>) -> Json<serde_json::Value> {
    match tokio::fs::read_to_string(&q.path).await {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => Json(serde_json::json!({ "ok": true, "manifest": v })),
            Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        },
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn hash_file(Query(q): Query<PathQuery>) -> Json<serde_json::Value> {
    let path = PathBuf::from(&q.path);
    match tokio::task::spawn_blocking(move || looper_core::io::sha256_file(&path)).await {
        Ok(Ok(hash)) => Json(serde_json::json!({ "ok": true, "sha256": hash })),
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

#[tokio::main]
async fn main() {
    let cwd = Arc::new(std::env::current_dir().expect("cwd"));
    let port = std::env::var("LOOPER_PORT").unwrap_or_else(|_| "8088".to_string());
    let frontend = std::env::var("LOOPER_FRONTEND")
        .unwrap_or_else(|_| "frontend/dist".to_string());

    let state = AppState { cwd: cwd.clone() };
    let app = Router::new()
        .route("/api/synth", post(synth_fixtures))
        .route("/api/save", post(save_project_endpoint))
        .route("/api/load", get(load_project_endpoint))
        .route("/api/build", post(build_stream))
        .route("/api/verify", get(verify_endpoint))
        .route("/api/manifest", get(read_manifest))
        .route("/api/hash", get(hash_file))
        .nest_service("/files", ServeDir::new(cwd.as_ref()))
        // axum 0.8: static frontend goes through the fallback service rather
        // than a root nest, so API routes always take precedence.
        .fallback_service(ServeDir::new(&frontend))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("bind");
    println!(
        "looper demo server on http://127.0.0.1:{port} (serving {frontend}, files under /files)"
    );
    axum::serve(listener, app).await.expect("serve");
}

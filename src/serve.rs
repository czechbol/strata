use anyhow::Result;
use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use std::path::PathBuf;
use tokio::fs;

const INDEX_HTML: &[u8] = include_bytes!("../web/index.html");
const APP_JS: &[u8] = include_bytes!("../web/app.js");
const STYLES_CSS: &[u8] = include_bytes!("../web/styles.css");

async fn root() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        APP_JS,
    )
}

async fn styles_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLES_CSS)
}

async fn data_file(
    axum::extract::State(data_dir): axum::extract::State<PathBuf>,
    Path(file): Path<String>,
) -> Response {
    // Reject path traversal attempts
    if file.contains("..") || file.contains('/') {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let path = data_dir.join(&file);
    match fs::read(&path).await {
        Ok(bytes) => {
            let content_type = if file.ends_with(".json") {
                "application/json"
            } else if file.ends_with(".msgpack") {
                "application/octet-stream"
            } else {
                "application/octet-stream"
            };
            ([(header::CONTENT_TYPE, content_type)], bytes).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn serve(dir: PathBuf, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/", get(root))
        .route("/app.js", get(app_js))
        .route("/styles.css", get(styles_css))
        .route("/data/:file", get(data_file))
        .with_state(dir);

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("Serving at http://localhost:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

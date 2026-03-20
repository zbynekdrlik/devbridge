use axum::http::{StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../crates/devbridge-ui/dist/"]
struct Assets;

const FALLBACK_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>DevBridge Dashboard</title>
    <style>
        body { font-family: system-ui, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #f5f5f5; }
        .card { text-align: center; padding: 2rem; background: white; border-radius: 8px; box-shadow: 0 2px 8px rgba(0,0,0,0.1); }
        h1 { color: #333; }
        p { color: #666; }
    </style>
</head>
<body>
    <div class="card">
        <h1>DevBridge Dashboard</h1>
        <p>The UI has not been built yet. Run <code>npm run build</code> in <code>crates/devbridge-ui</code> to generate the frontend.</p>
    </div>
</body>
</html>"#;

/// Serve embedded static files, falling back to index.html for SPA routing.
pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    // Try to serve the exact file requested.
    if let Some(file) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime.as_ref())],
            file.data,
        )
            .into_response();
    }

    // SPA fallback: serve index.html for any non-file route.
    if let Some(index) = Assets::get("index.html") {
        let mime = mime_guess::from_path("index.html").first_or_octet_stream();
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime.as_ref())],
            index.data,
        )
            .into_response();
    }

    // If the dist folder was never built, return a simple placeholder page.
    Html(FALLBACK_HTML).into_response()
}

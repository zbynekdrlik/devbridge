use axum::http::Uri;
use axum::response::{Html, IntoResponse, Response};

const FALLBACK_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>DevBridge Dashboard</title>
    <style>
        body { font-family: system-ui, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #0f172a; color: #e2e8f0; }
        .card { text-align: center; padding: 2rem; background: #1e293b; border-radius: 8px; border: 1px solid #334155; }
        h1 { color: #3b82f6; }
        p { color: #94a3b8; }
        code { background: #0f172a; padding: 0.2rem 0.4rem; border-radius: 4px; font-size: 0.9rem; }
    </style>
</head>
<body>
    <div class="card">
        <h1>DevBridge Dashboard</h1>
        <p>The UI has not been built yet.</p>
        <p>Run <code>trunk build --release</code> in <code>crates/devbridge-ui</code> to generate the frontend.</p>
    </div>
</body>
</html>"#;

/// Serve embedded static files, falling back to a placeholder when the UI is not built.
///
/// Once the Leptos WASM frontend is built via `trunk build --release`, this handler
/// will be replaced with a rust-embed based implementation that serves the dist/ assets.
/// For now, it always returns the placeholder page.
pub async fn static_handler(_uri: Uri) -> Response {
    // TODO: Once devbridge-ui/dist/ is built, use rust-embed to serve assets:
    //   #[derive(RustEmbed)]
    //   #[folder = "../../crates/devbridge-ui/dist/"]
    //   struct Assets;
    Html(FALLBACK_HTML).into_response()
}

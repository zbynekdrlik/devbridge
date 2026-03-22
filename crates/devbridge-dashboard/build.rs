use std::path::Path;

fn main() {
    // Ensure the UI dist directory exists so rust-embed compiles even when
    // trunk hasn't been run (e.g. Linux CI, local dev without frontend build).
    let dist = Path::new("../../crates/devbridge-ui/dist");
    if !dist.exists() {
        std::fs::create_dir_all(dist).ok();
    }
}

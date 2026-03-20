use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let subcommand = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match subcommand {
        "build" => cmd_build()?,
        "dist" => cmd_dist()?,
        "help" | "--help" | "-h" => print_help(),
        other => {
            eprintln!("Unknown subcommand: {other}");
            print_help();
            std::process::exit(1);
        }
    }

    Ok(())
}

fn print_help() {
    println!("Usage: cargo xtask <COMMAND>");
    println!();
    println!("Commands:");
    println!("  build   Build the WASM UI and the service binary (release)");
    println!("  dist    Build everything and create the Tauri installer");
    println!("  help    Print this help message");
}

fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask should be in a subdirectory of the project root")
        .to_path_buf()
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!(
            "Command `{:?}` failed with exit code: {}",
            cmd,
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

/// Build the WASM UI with trunk, then build the service binary in release mode.
fn cmd_build() -> anyhow::Result<()> {
    let root = project_root();
    let ui_dir = root.join("crates/devbridge-ui");

    println!("==> Building WASM UI with trunk...");
    run(Command::new("trunk")
        .args(["build", "--release"])
        .current_dir(&ui_dir))?;

    println!("==> Building devbridge-service (release)...");
    run(Command::new("cargo")
        .args(["build", "--release", "-p", "devbridge-service"])
        .current_dir(&root))?;

    println!("==> Build complete.");
    Ok(())
}

/// Build everything, then create the Tauri installer.
fn cmd_dist() -> anyhow::Result<()> {
    cmd_build()?;

    let root = project_root();
    let app_dir = root.join("crates/devbridge-app");

    println!("==> Creating Tauri installer...");
    run(Command::new("cargo")
        .args(["tauri", "build"])
        .current_dir(&app_dir))?;

    println!("==> Distribution build complete.");
    Ok(())
}

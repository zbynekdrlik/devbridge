# DevBridge - Windows Print Bridge

Reliable printing from Windows Server to remote store sites over VPN. DevBridge
captures print jobs via an IPP virtual printer on the server, transfers them over
gRPC with mTLS authentication, and prints on local USB/WiFi printers at each
store location.

## Quick Start

```sh
# Check everything compiles
cargo build --workspace

# Run all tests
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings

# Format
cargo fmt --all

# Build the WASM UI (from crates/devbridge-ui/)
trunk build --release

# Full build via xtask
cargo xtask build

# Distribution build (includes Tauri installer)
cargo xtask dist
```

## Architecture

```
 HQ / Server                          Store / Client
 ┌──────────────────────┐             ┌──────────────────────┐
 │  ERP / POS App       │             │  devbridge-client    │
 │        │              │             │        │              │
 │        ▼              │             │        ▼              │
 │  IPP Virtual Printer  │  gRPC/mTLS │  Local Print Driver  │
 │  (devbridge-server)  ├────VPN─────►│  (USB / WiFi)        │
 │        │              │             │        │              │
 │        ▼              │             │        ▼              │
 │  Job Queue + Storage  │             │  Hardware Printer    │
 └──────────────────────┘             └──────────────────────┘

 Shared: devbridge-core (config, proto, types)
 Management: devbridge-dashboard (web UI), devbridge-app (Tauri tray)
 Install: devbridge-service (Windows service entry point)
```

## Workspace Crates

| Crate                 | Purpose                                  |
| --------------------- | ---------------------------------------- |
| `devbridge-core`      | Shared types, config, proto codegen, DB  |
| `devbridge-server`    | IPP listener, gRPC server, spool manager |
| `devbridge-client`    | gRPC client, local print dispatcher      |
| `devbridge-dashboard` | Axum web dashboard (serves embedded UI)  |
| `devbridge-service`   | Windows service binary (entry point)     |
| `devbridge-ui`        | Leptos WASM frontend (excluded from ws)  |
| `devbridge-app`       | Tauri v2 tray app (excluded from ws)     |
| `xtask`               | Build orchestration                      |

## License

MIT - see [LICENSE-MIT](LICENSE-MIT).

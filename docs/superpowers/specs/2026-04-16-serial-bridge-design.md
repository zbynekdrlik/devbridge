# Serial Bridge (COM Port Forwarding) Design

## Problem

Barcode scanners at store POS terminals connect via USB-to-serial adapters (CH341) to local COM ports. The ERP application (Codex) runs on the central RDP terminal server (pz-server) and reads barcode data from a COM port. Previously, RDP COM port redirection (RDPDR) forwarded the client's COM port to the server session. This broke on Windows Server 2019 (confirmed: `fDisableCpm=0`, `redirectcomports:i:1`, yet `change port /query` shows no redirected ports). The issue has persisted for weeks across daily RDP reconnects.

DevBridge already bridges print jobs between the server and store clients over gRPC. Extending it to bridge serial port data eliminates the dependency on RDP COM redirect entirely.

## Scope

- **Initial target**: pjkeb store only (CH341 COM5, 9600 baud, EAN-13 barcodes as ASCII newline-terminated lines)
- **Direction**: client-to-server only (barcode scanners are read-only devices)
- **Server-side virtual COM port**: com0com open-source driver creates paired virtual ports

## Architecture

### Data flow

```
pjkeb POS terminal:
  Scanner --RS-232--> CH341 USB adapter --> COM5 (9600/8N1)
    --> DevBridge client serial_bridge module reads COM5
    --> gRPC StreamSerialData --> pz-server

pz-server:
  DevBridge server receives SerialData via gRPC
    --> writes to COM20 (com0com virtual port, write end)
    --> com0com pairs COM20 <-> COM21
    --> Codex ERP opens COM21 (read end), receives barcode data
```

### Components

#### 1. Proto extension (`proto/devbridge.proto`)

Add to the `PrintBridge` service:

```protobuf
// Client streams serial port data (barcode scans) to the server.
// Server writes to a paired virtual COM port for ERP consumption.
rpc StreamSerialData(stream SerialData) returns (stream SerialAck);

message SerialData {
  string client_id = 1;
  bytes data = 2;
}

message SerialAck {
  bool ok = 1;
}
```

Client-to-server streaming with a trivial ack backpressure channel. The `data` field contains raw bytes read from the serial port (typically a barcode string + `\r\n`). Messages are sent per-read (one barcode per message).

#### 2. Client serial reader (`crates/devbridge-client/src/serial_bridge.rs`)

New module — runs as a background tokio task alongside the existing print job receiver.

Responsibilities:
- Open the configured local COM port (e.g., COM5) with configured baud rate (default 9600), 8N1
- Read in a blocking loop (via `spawn_blocking` to avoid blocking the async runtime, same pattern as `ghostscript.rs`)
- On each `ReadLine()`, send a `SerialData` message over the gRPC stream
- On port error (device unplugged): log warning, retry with exponential backoff (1s, 2s, 4s... max 30s), reconnect when device reappears
- On gRPC disconnect: buffer up to 10 recent barcodes in memory, flush when reconnected

Rust serial port crate: `serialport` (https://crates.io/crates/serialport) — cross-platform, well-maintained, supports Windows COM ports.

#### 3. Server serial writer (`crates/devbridge-server/src/serial_bridge.rs`)

New module — handles incoming `StreamSerialData` gRPC streams.

Responsibilities:
- On client connection: look up the client's serial bridge config (which virtual COM port to write to)
- Open the com0com virtual port (e.g., COM20) with matching baud rate
- Write each received `SerialData.data` to the virtual port
- On write error: log, attempt to reopen the port
- Multiple clients can have independent serial bridges (one virtual port pair per client)

#### 4. com0com installation

com0com is an open-source (GPL) virtual null-modem driver for Windows. It creates paired virtual COM ports — writing to one end makes data available on the other.

- **Installation**: bundled in the DevBridge NSIS installer (server mode only). The installer runs `setup.exe install - -` to install the driver silently.
- **Port pair creation**: `post-install.ps1` creates pairs using com0com's `setupc.exe`:
  ```
  setupc.exe install PortName=COM20,EmuBR=yes PortName=COM21,EmuBR=yes
  ```
  `EmuBR=yes` enables baud rate emulation so the port behaves like a real serial port.
- **Per-store pairs**: each store with a scanner gets its own pair. For pjkeb: COM20↔COM21.
- **Persistence**: com0com port pairs survive reboots (stored in driver registry).

#### 5. Configuration

Client config (`config.toml` on pjkeb):
```toml
[serial_bridge]
enabled = true
port = "COM5"
baud_rate = 9600
data_bits = 8
stop_bits = 1
parity = "none"
```

Server config (`config.toml` on pz-server) — per-client serial bridge mapping:
```toml
[[serial_bridges]]
client_id = "pjkeb-client"
virtual_port = "COM20"
baud_rate = 9600
```

The server config maps client IDs to virtual COM port write-ends. The paired read-end (COM21) is configured in Codex ERP.

#### 6. Codex ERP configuration change

One-time manual change: update pjkeb's Codex scanner port from the old RDP-redirected port number to COM21 (the com0com read end). This is a Codex config file or database setting — not a DevBridge concern.

## Error handling

| Scenario | Behavior |
|---|---|
| Scanner unplugged on client | Client logs warning, retries opening COM port with backoff. Reconnects when plugged back. |
| gRPC connection lost | Client buffers up to 10 barcodes in memory. Flushes on reconnect. Existing reconnect logic handles gRPC. |
| com0com port not available on server | Server logs error at startup, serial bridge disabled for that client. Print bridging unaffected. |
| Client has no `[serial_bridge]` config | Feature disabled, no COM port opened. Zero overhead. |
| Multiple rapid scans | Each barcode is a separate `SerialData` message. gRPC handles ordering. No deduplication — ERP is responsible for that. |

## What this does NOT include (YAGNI)

- **Bidirectional** (server-to-client) serial data — barcode scanners are read-only
- **Multiple serial ports per client** — one scanner per store is the current setup
- **Dynamic port pair creation** — fixed config, added manually per new store
- **Dashboard UI** for serial bridge monitoring — config file + logs only
- **Linux/macOS com0com equivalent** — pz-server is Windows only; macOS clients (pz-david) don't have barcode scanners
- **Auto-detection of scanner baud rate** — configured explicitly per store

## Testing

- **Unit test**: mock serial port read → verify gRPC `SerialData` message content matches
- **Unit test**: mock gRPC `SerialData` receive → verify write to mock serial port
- **Integration test**: use a loopback serial port pair (com0com on CI Windows runner or socat on Linux) to verify end-to-end data flow
- **E2E test on pjkeb**: scan a barcode → verify it appears on COM21 on pz-server → verify Codex receives it
- **Resilience test**: unplug scanner during operation → verify client reconnects and resumes

## Dependencies

- **Rust crate**: `serialport` ^4.0 (cross-platform serial port access)
- **com0com**: v3.0 signed driver (bundled in NSIS installer, server mode only)
- **Proto change**: backward-compatible addition to `PrintBridge` service

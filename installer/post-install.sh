#!/usr/bin/env bash
# DevBridge Post-Install Configuration for macOS
# Run after DMG installation to configure service, dirs, and launchd plists.
# Idempotent: safe to run on upgrades (stops service first, updates config, restarts).
#
# Usage (env vars passed from install.sh or set manually):
#   DEVBRIDGE_MODE=server bash post-install.sh
#   DEVBRIDGE_MODE=client DEVBRIDGE_SERVER_HOST=print-server.lan \
#     DEVBRIDGE_TARGET_PRINTER="Canon MG3600" bash post-install.sh

set -euo pipefail

# --- Configuration from environment variables ---
MODE="${DEVBRIDGE_MODE:-client}"
SERVER_HOST="${DEVBRIDGE_SERVER_HOST:-print-server.lan}"
CLIENT_ID="${DEVBRIDGE_CLIENT_ID:-}"
TARGET_PRINTER="${DEVBRIDGE_TARGET_PRINTER:-}"
PRINT_BACKEND="${DEVBRIDGE_PRINT_BACKEND:-cups}"
PRINTER_ADDRESS="${DEVBRIDGE_PRINTER_ADDRESS:-}"
PRINTER_TLS="${DEVBRIDGE_PRINTER_TLS:-}"
GHOSTSCRIPT_DEVICE="${DEVBRIDGE_GHOSTSCRIPT_DEVICE:-}"
GHOSTSCRIPT_RESOLUTION="${DEVBRIDGE_GHOSTSCRIPT_RESOLUTION:-}"
VIRTUAL_PRINTER_NAME="${DEVBRIDGE_VIRTUAL_PRINTER_NAME:-}"
PRINTER_DISPLAY_NAME="${DEVBRIDGE_PRINTER_DISPLAY_NAME:-}"
IPP_PORT="${DEVBRIDGE_IPP_PORT:-631}"
GRPC_PORT="${DEVBRIDGE_GRPC_PORT:-50051}"
DASHBOARD_PORT="${DEVBRIDGE_DASHBOARD_PORT:-9120}"
PRINTER_NAME="${DEVBRIDGE_PRINTER_NAME:-DevBridge}"

# --- Paths ---
INSTALL_DIR="/Applications/DevBridge.app/Contents/MacOS"
SERVICE_BIN="/Applications/DevBridge.app/Contents/MacOS/devbridge-service"
TRAY_BIN="/Applications/DevBridge.app/Contents/MacOS/devbridge-app"
DATA_DIR="/Library/Application Support/DevBridge"
LOGS_DIR="/Library/Logs/DevBridge"
CONFIG_PATH="${DATA_DIR}/config.toml"
SERVICE_PLIST_SRC="/Applications/DevBridge.app/Contents/Resources/com.devbridge.service.plist"
SERVICE_PLIST_DST="/Library/LaunchDaemons/com.devbridge.service.plist"
TRAY_PLIST_DST="${HOME}/Library/LaunchAgents/com.devbridge.tray.plist"

echo "=== DevBridge Post-Install - ${MODE} mode ==="

# --- Stop existing instance if upgrading ---
if launchctl list com.devbridge.service > /dev/null 2>&1; then
    echo "Stopping existing service for upgrade..."
    launchctl unload "$SERVICE_PLIST_DST" 2>/dev/null || true
fi
pkill -x "devbridge-service" 2>/dev/null || true
sleep 2

# --- Create data directory structure ---
echo "Creating data directories..."
for sub in certs spool logs; do
    mkdir -p "${DATA_DIR}/${sub}"
    echo "  Created ${DATA_DIR}/${sub}"
done
mkdir -p "$LOGS_DIR"
echo "  Created ${LOGS_DIR}"

# --- Check/install Ghostscript for direct backends ---
if [ "$PRINT_BACKEND" = "direct_ipp" ] || [ "$PRINT_BACKEND" = "direct_raw" ]; then
    if ! command -v gs > /dev/null 2>&1; then
        echo "WARNING: Ghostscript not found. Direct IPP/RAW printing requires Ghostscript." >&2
        echo "  Install with: brew install ghostscript" >&2
    else
        GS_VER="$(gs --version 2>/dev/null || echo 'unknown')"
        echo "  Ghostscript found: ${GS_VER}"
    fi
fi

# --- Write config.toml ---
echo "Writing configuration..."
if [ "${CI:-}" = "true" ]; then
    LOG_LEVEL="debug"
else
    LOG_LEVEL="info"
fi

# Escape data dir for TOML (forward slashes are safe in TOML strings)
TOML_DATA_DIR="${DATA_DIR}"

if [ "$MODE" = "server" ]; then
    cat > "$CONFIG_PATH" <<EOF
[general]
mode = "server"
log_level = "${LOG_LEVEL}"
data_dir = "${TOML_DATA_DIR}"

[server]
ipp_port = ${IPP_PORT}
grpc_port = ${GRPC_PORT}
dashboard_port = ${DASHBOARD_PORT}
printer_name = "${PRINTER_NAME}"
spool_dir = "${TOML_DATA_DIR}/spool"

[client]
server_address = "127.0.0.1:${GRPC_PORT}"
target_printer = "unused"
dashboard_port = 9121
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60

[jobs]
max_retries = 3
retry_delay_secs = 30
job_expiry_hours = 24
max_payload_size_mb = 100
print_timeout_secs = 1800
EOF
else
    # Build optional client fields
    OPTIONAL_FIELDS=""
    [ -n "$CLIENT_ID" ]           && OPTIONAL_FIELDS="${OPTIONAL_FIELDS}client_id = \"${CLIENT_ID}\"\n"
    [ -n "$PRINTER_DISPLAY_NAME" ] && OPTIONAL_FIELDS="${OPTIONAL_FIELDS}printer_display_name = \"${PRINTER_DISPLAY_NAME}\"\n"
    [ -n "$PRINT_BACKEND" ]       && OPTIONAL_FIELDS="${OPTIONAL_FIELDS}print_backend = \"${PRINT_BACKEND}\"\n"
    [ -n "$PRINTER_ADDRESS" ]     && OPTIONAL_FIELDS="${OPTIONAL_FIELDS}printer_address = \"${PRINTER_ADDRESS}\"\n"
    [ -n "$PRINTER_TLS" ]         && OPTIONAL_FIELDS="${OPTIONAL_FIELDS}printer_tls = true\n"
    [ -n "$GHOSTSCRIPT_DEVICE" ]  && OPTIONAL_FIELDS="${OPTIONAL_FIELDS}ghostscript_device = \"${GHOSTSCRIPT_DEVICE}\"\n"
    [ -n "$GHOSTSCRIPT_RESOLUTION" ] && OPTIONAL_FIELDS="${OPTIONAL_FIELDS}ghostscript_resolution = ${GHOSTSCRIPT_RESOLUTION}\n"
    [ -n "$VIRTUAL_PRINTER_NAME" ] && OPTIONAL_FIELDS="${OPTIONAL_FIELDS}virtual_printer_name = \"${VIRTUAL_PRINTER_NAME}\"\n"

    cat > "$CONFIG_PATH" <<EOF
[general]
mode = "client"
log_level = "${LOG_LEVEL}"
data_dir = "${TOML_DATA_DIR}"

[server]
ipp_port = ${IPP_PORT}
grpc_port = ${GRPC_PORT}
dashboard_port = 9121
printer_name = "unused"
spool_dir = "${TOML_DATA_DIR}/spool"

[client]
server_address = "${SERVER_HOST}:${GRPC_PORT}"
target_printer = "${TARGET_PRINTER}"
dashboard_port = ${DASHBOARD_PORT}
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60
EOF
    # Append optional fields
    if [ -n "$OPTIONAL_FIELDS" ]; then
        printf "%b" "$OPTIONAL_FIELDS" >> "$CONFIG_PATH"
    fi

    cat >> "$CONFIG_PATH" <<EOF

[jobs]
max_retries = 3
retry_delay_secs = 30
job_expiry_hours = 24
max_payload_size_mb = 100
print_timeout_secs = 1800
EOF
fi

echo "  Config written to ${CONFIG_PATH}"

# --- Install launchd daemon plist ---
echo "Installing launchd daemon plist..."
if [ -f "$SERVICE_PLIST_SRC" ]; then
    cp "$SERVICE_PLIST_SRC" "$SERVICE_PLIST_DST"
else
    # Write plist inline if not bundled in the .app
    cat > "$SERVICE_PLIST_DST" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.devbridge.service</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Applications/DevBridge.app/Contents/MacOS/devbridge-service</string>
        <string>-c</string>
        <string>/Library/Application Support/DevBridge/config.toml</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/Library/Logs/DevBridge/devbridge.log</string>
    <key>StandardErrorPath</key>
    <string>/Library/Logs/DevBridge/devbridge-error.log</string>
    <key>WorkingDirectory</key>
    <string>/Library/Application Support/DevBridge</string>
</dict>
</plist>
PLIST
fi
chmod 644 "$SERVICE_PLIST_DST"
echo "  Service plist installed to ${SERVICE_PLIST_DST}"

# --- Install auto-update launchd daemon (issue #54) ---
# Stage autoupdate.sh into DATA_DIR and load a launchd daemon that runs it at
# load (≈ boot) and every 6 hours. PATCH-only self-upgrade so macOS clients
# (e.g. pz-david) no longer wait for an operator to run `curl | bash`. All
# safety guards (kill-switch, patch-lock, skip-if-printing, fail-safe) live in
# autoupdate.sh; it re-runs the hardened install.sh to actually upgrade.
echo "Installing auto-update launchd daemon..."
AUTOUPDATE_SRC="/Applications/DevBridge.app/Contents/Resources/autoupdate.sh"
AUTOUPDATE_DST="${DATA_DIR}/autoupdate.sh"
AUTOUPDATE_PLIST_SRC="/Applications/DevBridge.app/Contents/Resources/com.devbridge.autoupdate.plist"
AUTOUPDATE_PLIST_DST="/Library/LaunchDaemons/com.devbridge.autoupdate.plist"
if [ -f "$AUTOUPDATE_SRC" ]; then
    cp "$AUTOUPDATE_SRC" "$AUTOUPDATE_DST"
    chmod +x "$AUTOUPDATE_DST"
    echo "  Staged auto-update script at ${AUTOUPDATE_DST}"

    if [ -f "$AUTOUPDATE_PLIST_SRC" ]; then
        cp "$AUTOUPDATE_PLIST_SRC" "$AUTOUPDATE_PLIST_DST"
    else
        # Write the plist inline if not bundled in the .app. Daemon points at
        # the staged DATA_DIR script (not the bundled one) so it survives app
        # upgrades. RunAtLoad ≈ boot; the four calendar slots = every 6 hours.
        cat > "$AUTOUPDATE_PLIST_DST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.devbridge.autoupdate</string>
    <key>ProgramArguments</key>
    <array>
        <string>/bin/bash</string>
        <string>${AUTOUPDATE_DST}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>StartCalendarInterval</key>
    <array>
        <dict><key>Hour</key><integer>0</integer><key>Minute</key><integer>0</integer></dict>
        <dict><key>Hour</key><integer>6</integer><key>Minute</key><integer>0</integer></dict>
        <dict><key>Hour</key><integer>12</integer><key>Minute</key><integer>0</integer></dict>
        <dict><key>Hour</key><integer>18</integer><key>Minute</key><integer>0</integer></dict>
    </array>
    <key>StandardOutPath</key>
    <string>${LOGS_DIR}/autoupdate.log</string>
    <key>StandardErrorPath</key>
    <string>${LOGS_DIR}/autoupdate-error.log</string>
    <key>WorkingDirectory</key>
    <string>${DATA_DIR}</string>
</dict>
</plist>
PLIST
    fi
    chmod 644 "$AUTOUPDATE_PLIST_DST"
    # Reload so an upgrade picks up any plist/script change — but NOT when this
    # post-install was invoked from within the auto-update daemon itself
    # (DEVBRIDGE_FROM_AUTOUPDATE=1). Unloading that daemon mid-run SIGTERMs the
    # autoupdate.sh that is calling us, killing its post-update logging tail; the
    # already-running daemon will pick up the new script/plist on its next fire
    # (issue #54 re-entrancy guard).
    if [ "${DEVBRIDGE_FROM_AUTOUPDATE:-}" = "1" ]; then
        echo "  Auto-update daemon staged at ${AUTOUPDATE_PLIST_DST} (reload skipped — running from within auto-update)"
    else
        launchctl unload "$AUTOUPDATE_PLIST_DST" 2>/dev/null || true
        launchctl load "$AUTOUPDATE_PLIST_DST" 2>/dev/null || true
        echo "  Auto-update daemon installed to ${AUTOUPDATE_PLIST_DST} (boot + every 6h)"
    fi
else
    echo "  WARNING: autoupdate.sh not found at ${AUTOUPDATE_SRC}; auto-update daemon not installed"
fi

# --- Install tray app launch agent ---
if [ -f "$TRAY_BIN" ]; then
    mkdir -p "${HOME}/Library/LaunchAgents"
    cat > "$TRAY_PLIST_DST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.devbridge.tray</string>
    <key>ProgramArguments</key>
    <array>
        <string>${TRAY_BIN}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>StandardOutPath</key>
    <string>${LOGS_DIR}/devbridge-tray.log</string>
    <key>StandardErrorPath</key>
    <string>${LOGS_DIR}/devbridge-tray-error.log</string>
</dict>
</plist>
PLIST
    chmod 644 "$TRAY_PLIST_DST"
    echo "  Tray agent plist installed to ${TRAY_PLIST_DST}"
else
    echo "  Tray app not found at ${TRAY_BIN}, skipping tray agent"
fi

# --- Start service via launchctl ---
echo "Starting DevBridge service..."
launchctl load "$SERVICE_PLIST_DST"
sleep 3

if pgrep -x "devbridge-service" > /dev/null 2>&1; then
    PID="$(pgrep -x "devbridge-service" | head -1)"
    echo "  Service is running (PID: ${PID})"
else
    echo "WARNING: Service process not found. Check logs at ${LOGS_DIR}"
fi

# --- Launch tray app for current user ---
if [ -f "$TRAY_BIN" ]; then
    # Kill any existing tray app to avoid duplicate icons after upgrade
    pkill -x "devbridge-app" 2>/dev/null || true
    sleep 1
    # Load launch agent for current user session
    launchctl load "$TRAY_PLIST_DST" 2>/dev/null || true
    echo "  Tray app launched"
fi

echo ""
echo "=== Post-install complete ==="
echo "  Mode:      ${MODE}"
LOCAL_IP=$(ipconfig getifaddr en0 2>/dev/null || hostname -I 2>/dev/null | awk '{print $1}' || echo 'localhost')
echo "  Dashboard: http://${LOCAL_IP}:${DASHBOARD_PORT}"
echo "  Data dir:  ${DATA_DIR}"
echo "  Logs:      ${LOGS_DIR}"

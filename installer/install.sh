#!/usr/bin/env bash
# DevBridge one-liner installer for macOS
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.sh | bash
#   DEVBRIDGE_VERSION=dev curl -fsSL https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.sh | bash

set -euo pipefail

REPO="zbynekdrlik/devbridge"
REQUESTED_VERSION="${DEVBRIDGE_VERSION:-latest}"
TEMP_DIR="$(mktemp -d /tmp/devbridge-install.XXXXXX)"

cleanup() {
    rm -rf "$TEMP_DIR"
}
trap cleanup EXIT

echo "==> DevBridge Installer"

# --- Detect release ---
if [ "$REQUESTED_VERSION" = "latest" ]; then
    echo "Fetching latest stable release..."
    RELEASE_URL="https://api.github.com/repos/${REPO}/releases/latest"
elif [ "$REQUESTED_VERSION" = "dev" ]; then
    echo "Fetching dev release..."
    RELEASE_URL="https://api.github.com/repos/${REPO}/releases/tags/dev-latest"
else
    echo "Fetching release ${REQUESTED_VERSION}..."
    RELEASE_URL="https://api.github.com/repos/${REPO}/releases/tags/${REQUESTED_VERSION}"
fi

RELEASE_JSON="${TEMP_DIR}/release.json"
if ! curl -fsSL -H "User-Agent: DevBridge-Installer" -o "$RELEASE_JSON" "$RELEASE_URL"; then
    echo "ERROR: Failed to fetch release '${REQUESTED_VERSION}' from GitHub. Check your internet connection and version." >&2
    exit 1
fi

VERSION="$(grep '"tag_name"' "$RELEASE_JSON" | head -1 | sed 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')"
echo "Version: ${VERSION}"

# --- Find DMG asset ---
DOWNLOAD_URL="$(grep '"browser_download_url"' "$RELEASE_JSON" | grep '\.dmg"' | head -1 | sed 's/.*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')"
if [ -z "$DOWNLOAD_URL" ]; then
    echo "ERROR: No .dmg asset found in release ${VERSION}" >&2
    exit 1
fi

FILE_NAME="$(basename "$DOWNLOAD_URL")"
DMG_PATH="${TEMP_DIR}/${FILE_NAME}"

# Find SHA256SUMS asset
CHECKSUM_URL="$(grep '"browser_download_url"' "$RELEASE_JSON" | grep 'SHA256SUMS"' | head -1 | sed 's/.*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')"

# --- Download DMG ---
echo "Downloading ${FILE_NAME}..."
curl -fsSL -o "$DMG_PATH" "$DOWNLOAD_URL"

# --- Verify checksum ---
if [ -n "$CHECKSUM_URL" ]; then
    CHECKSUM_FILE="${TEMP_DIR}/SHA256SUMS"
    curl -fsSL -o "$CHECKSUM_FILE" "$CHECKSUM_URL"

    EXPECTED_HASH="$(grep "$FILE_NAME" "$CHECKSUM_FILE" | awk '{print $1}' | tr '[:upper:]' '[:lower:]')"
    ACTUAL_HASH="$(shasum -a 256 "$DMG_PATH" | awk '{print $1}' | tr '[:upper:]' '[:lower:]')"

    if [ -n "$EXPECTED_HASH" ] && [ "$ACTUAL_HASH" != "$EXPECTED_HASH" ]; then
        echo "ERROR: Checksum verification failed!" >&2
        echo "  Expected: ${EXPECTED_HASH}" >&2
        echo "  Actual:   ${ACTUAL_HASH}" >&2
        exit 1
    fi
    echo "Checksum verified."
else
    echo "WARNING: No SHA256SUMS file found in release; skipping checksum verification."
fi

# --- Mount DMG ---
echo "Mounting disk image..."
MOUNT_POINT="${TEMP_DIR}/mount"
mkdir -p "$MOUNT_POINT"
hdiutil attach -nobrowse -mountpoint "$MOUNT_POINT" "$DMG_PATH"

unmount_dmg() {
    hdiutil detach "$MOUNT_POINT" -force 2>/dev/null || true
    cleanup
}
trap unmount_dmg EXIT

# --- Copy .app to /Applications ---
APP_SRC="$(find "$MOUNT_POINT" -name "DevBridge.app" -maxdepth 2 | head -1)"
if [ -z "$APP_SRC" ]; then
    echo "ERROR: DevBridge.app not found in disk image" >&2
    exit 1
fi

echo "Installing DevBridge.app to /Applications..."
# Remove existing installation if present
if [ -d "/Applications/DevBridge.app" ]; then
    rm -rf "/Applications/DevBridge.app"
fi
cp -R "$APP_SRC" "/Applications/DevBridge.app"

# --- Unmount DMG ---
hdiutil detach "$MOUNT_POINT" -force
trap cleanup EXIT

# --- Remove quarantine attribute (unsigned app) ---
echo "Removing quarantine attribute..."
xattr -cr "/Applications/DevBridge.app"

# --- Run post-install.sh bundled inside the .app ---
POST_INSTALL="/Applications/DevBridge.app/Contents/Resources/post-install.sh"
if [ -f "$POST_INSTALL" ]; then
    echo "Running post-install script..."
    chmod +x "$POST_INSTALL"
    bash "$POST_INSTALL"
else
    echo "WARNING: post-install.sh not found at ${POST_INSTALL}. Skipping post-install configuration."
fi

# --- Verify service running ---
echo "Checking service status..."
sleep 3
if pgrep -x "devbridge-service" > /dev/null 2>&1; then
    PID="$(pgrep -x "devbridge-service" | head -1)"
    echo "DevBridge service is running (PID: ${PID})."
else
    echo "Attempting to load launchd daemon..."
    launchctl load /Library/LaunchDaemons/com.devbridge.service.plist 2>/dev/null || true
    sleep 3
    if pgrep -x "devbridge-service" > /dev/null 2>&1; then
        PID="$(pgrep -x "devbridge-service" | head -1)"
        echo "DevBridge service started (PID: ${PID})."
    else
        echo "WARNING: Could not start service. Please start it manually or check logs."
    fi
fi

echo ""
echo "==> DevBridge ${VERSION} installed successfully."

#!/usr/bin/env bash
# DevBridge Auto-Update Task (macOS)
#
# Loaded by post-install.sh as the launchd daemon "com.devbridge.autoupdate"
# (RunAtLoad ≈ machine startup + StartCalendarInterval every 6 hours, see
# com.devbridge.autoupdate.plist). It checks GitHub for a newer PATCH release on
# the SAME major.minor as the installed app and, when one exists and it is safe,
# re-runs the macOS installer/install.sh to upgrade in place. Operators no longer
# have to run `curl | bash` by hand. See issue #54.
#
# SAFETY MODEL — same as the Windows autoupdate.ps1: PATCH-only lock,
# kill-switch (env var DEVBRIDGE_AUTO_UPDATE=off OR marker file), skip-if-printing
# via the dashboard active_jobs, and fail-safe no-op on any error.
#
# NOTE: this script deliberately does NOT use `set -e`. Every guard is an
# explicit early `exit 0` so a transient failure (no network, unreadable version)
# is a clean no-op that leaves the running service untouched, never a half-applied
# upgrade.

REPO="zbynekdrlik/devbridge"
APP_PLIST="/Applications/DevBridge.app/Contents/Info.plist"
DATA_DIR="/Library/Application Support/DevBridge"
LOGS_DIR="/Library/Logs/DevBridge"
LOG_FILE="${LOGS_DIR}/autoupdate.log"
MARKER_FILE="${DATA_DIR}/autoupdate.disabled"
DASHBOARD_URL="http://127.0.0.1:9120/api/status"

mkdir -p "$LOGS_DIR" 2>/dev/null || true

log() {
    # $1 = message, $2 = level (default INFO)
    local level="${2:-INFO}"
    local line
    line="$(date '+%Y-%m-%d %H:%M:%S') [${level}] $1"
    echo "$line" | tee -a "$LOG_FILE" 2>/dev/null || echo "$line"
}

# --- Pure decision helpers (mirror autoupdate.ps1; covered by the same logic) ---

# Echo "MAJOR MINOR PATCH" for a semver-ish string, or nothing if unparseable.
parse_semver() {
    local v="$1"
    v="${v#v}"; v="${v#V}"
    if [[ "$v" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+) ]]; then
        echo "${BASH_REMATCH[1]} ${BASH_REMATCH[2]} ${BASH_REMATCH[3]}"
    fi
}

# Echo the update reason: unparseable-installed | unparseable-latest |
# minor-or-major-change | downgrade | up-to-date | patch-update.
# patch-update is the ONLY value that authorizes an upgrade.
update_decision() {
    local installed="$1" latest="$2"
    local i l
    i="$(parse_semver "$installed")"
    l="$(parse_semver "$latest")"
    if [ -z "$i" ]; then echo "unparseable-installed"; return; fi
    if [ -z "$l" ]; then echo "unparseable-latest"; return; fi
    read -r imaj imin ipat <<<"$i"
    read -r lmaj lmin lpat <<<"$l"
    if [ "$imaj" -ne "$lmaj" ] || [ "$imin" -ne "$lmin" ]; then
        echo "minor-or-major-change"; return
    fi
    if [ "$lpat" -gt "$ipat" ]; then echo "patch-update"; return; fi
    if [ "$lpat" -lt "$ipat" ]; then echo "downgrade"; return; fi
    echo "up-to-date"
}

# Echo "disabled <reason>" or "enabled none". reason: marker-file | env-var | none.
killswitch_state() {
    local env_value="$1" marker_exists="$2"
    if [ "$marker_exists" = "true" ]; then echo "disabled marker-file"; return; fi
    shopt -s nocasematch
    if [[ "$env_value" =~ ^[[:space:]]*(off|false|0|no|disabled)[[:space:]]*$ ]]; then
        shopt -u nocasematch
        echo "disabled env-var"; return
    fi
    shopt -u nocasematch
    echo "enabled none"
}

log "=== DevBridge auto-update check starting (pid $$) ==="

# 1. Kill-switch.
marker_exists="false"; [ -f "$MARKER_FILE" ] && marker_exists="true"
ks="$(killswitch_state "${DEVBRIDGE_AUTO_UPDATE:-}" "$marker_exists")"
read -r ks_state ks_reason <<<"$ks"
if [ "$ks_state" = "disabled" ]; then
    log "Auto-update DISABLED via ${ks_reason} (env DEVBRIDGE_AUTO_UPDATE='${DEVBRIDGE_AUTO_UPDATE:-}', marker='${MARKER_FILE}'). Exiting no-op."
    exit 0
fi
log "Kill-switch check: enabled (env DEVBRIDGE_AUTO_UPDATE='${DEVBRIDGE_AUTO_UPDATE:-}', no marker file)."

# 2. Read installed version from the app bundle Info.plist.
INSTALLED=""
if [ -f "$APP_PLIST" ]; then
    INSTALLED="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP_PLIST" 2>/dev/null || true)"
fi
if [ -z "$INSTALLED" ]; then
    log "Could not read installed version from ${APP_PLIST}. Exiting no-op (service left untouched)." "WARN"
    exit 0
fi
log "Installed version: ${INSTALLED}"

# 3. Latest version: DEVBRIDGE_VERSION pin, else GitHub latest stable.
LATEST=""
if [ -n "${DEVBRIDGE_VERSION:-}" ]; then
    LATEST="$DEVBRIDGE_VERSION"
    log "DEVBRIDGE_VERSION pin present: target version '${LATEST}'."
else
    RELEASE_JSON="$(curl -fsSL -H 'User-Agent: DevBridge-AutoUpdate' --max-time 30 \
        "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null || true)"
    if [ -z "$RELEASE_JSON" ]; then
        log "Failed to fetch latest release from GitHub. Exiting no-op." "WARN"
        exit 0
    fi
    LATEST="$(echo "$RELEASE_JSON" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')"
    log "GitHub latest release tag: '${LATEST}'."
fi

# 4. Patch-only update decision.
DECISION="$(update_decision "$INSTALLED" "$LATEST")"
log "Update decision: ${DECISION} (installed=${INSTALLED}, latest=${LATEST})."
if [ "$DECISION" != "patch-update" ]; then
    log "No patch-update to apply (${DECISION}). Exiting no-op."
    exit 0
fi

# 5. Skip-if-printing: read active_jobs from the local dashboard. Unknown => skip.
ACTIVE_JOBS=""
STATUS_JSON="$(curl -fsSL --max-time 10 "$DASHBOARD_URL" 2>/dev/null || true)"
if [ -n "$STATUS_JSON" ]; then
    ACTIVE_JOBS="$(echo "$STATUS_JSON" | grep -o '"active_jobs"[[:space:]]*:[[:space:]]*[0-9]*' | grep -o '[0-9]*$' | head -1)"
fi
if [ -z "$ACTIVE_JOBS" ]; then
    log "Could not read active_jobs from dashboard (unreachable or field missing). Treating as unknown; skipping this cycle." "WARN"
    exit 0
fi
log "Dashboard active_jobs = ${ACTIVE_JOBS}."
if [ "$ACTIVE_JOBS" -gt 0 ]; then
    log "Not safe to update right now (jobs-active=${ACTIVE_JOBS}). Will retry next cycle. Exiting no-op."
    exit 0
fi
log "Safe-to-update check: idle (active_jobs=0)."

# 6. Apply via the hardened install.sh (owns the stop/swap/config-preserve flow).
log "Applying patch-update ${INSTALLED} -> ${LATEST} via install.sh (DEVBRIDGE_VERSION='${LATEST}')."
if DEVBRIDGE_VERSION="$LATEST" curl -fsSL \
        "https://raw.githubusercontent.com/${REPO}/main/installer/install.sh" | bash; then
    NEW_INSTALLED=""
    [ -f "$APP_PLIST" ] && NEW_INSTALLED="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP_PLIST" 2>/dev/null || true)"
    log "Post-update installed version: ${NEW_INSTALLED} (was ${INSTALLED}, target ${LATEST})."
    if [ -n "$NEW_INSTALLED" ] && [ "$NEW_INSTALLED" != "$INSTALLED" ]; then
        log "Auto-update SUCCESS: ${INSTALLED} -> ${NEW_INSTALLED}."
    else
        log "Auto-update ran but installed version did not change (still ${NEW_INSTALLED}). Investigate install.sh output above." "WARN"
    fi
else
    log "Auto-update FAILED while running install.sh. The previous version remains installed." "ERROR"
    exit 1
fi

log "=== DevBridge auto-update check finished ==="
exit 0

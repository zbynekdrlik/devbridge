#!/usr/bin/env bash
# shellcheck disable=SC2016,SC2034  # check() evals single-quoted assertions on purpose
set -euo pipefail
# Tests for deploy/publish-release.sh (issue #123) with a FAKE gh on PATH: it
# records every call (args + the GH_TOKEN it saw) and answers from a small
# state dir, so no network and no real token is involved.
# Run: bash deploy/tests/publish-release.test.sh  (CI job "Release Script Tests")

here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../publish-release.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fails=0
passes=0

mkdir -p "$tmp/bin"
cat > "$tmp/bin/gh" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
# State: $FAKE/releases/<tag> exists = release exists; its assets are lines in it.
echo "token=${GH_TOKEN:-} $*" >> "$FAKE/calls"
case "$1 $2" in
  "release view")
    tag="$3"
    if [[ ! -f "$FAKE/releases/$tag" ]]; then echo "release not found" >&2; exit 1; fi
    if [[ "$*" == *"--json assets"* ]]; then cat "$FAKE/releases/$tag"; else echo "$tag"; fi ;;
  "release create") touch "$FAKE/releases/$3" ;;
  "release edit") ;;
  "release upload") basename "${@: -1}" >> "$FAKE/releases/$3" ;;
  "release delete-asset") grep -vx "$4" "$FAKE/releases/$3" > "$FAKE/tmp" || true; mv "$FAKE/tmp" "$FAKE/releases/$3" ;;
  "release download") cp "$FAKE/SHA256SUMS" "${@: -1}/SHA256SUMS" ;;
  "api repos/"*)
    name="${2##*/contents/}"; name="${name%%\?*}"
    if [[ -f "$FAKE/remote/$name" ]]; then echo "sha-$name $(base64 -w0 < "$FAKE/remote/$name")"
    else echo "gh: Not Found (HTTP 404)" >&2; exit 1; fi ;;
  "api -X") ;;
  *) echo "fake gh: unexpected $*" >&2; exit 3 ;;
esac
FAKE
chmod +x "$tmp/bin/gh"

reset() {
    export FAKE="$tmp/fake"
    rm -rf "$FAKE"
    mkdir -p "$FAKE/releases" "$FAKE/remote"
    : > "$FAKE/calls"
    unset GH_TOKEN DEVBRIDGE_GH_TOKEN_FILE
}
run() { PATH="$tmp/bin:$PATH" bash "$script" "$@"; }
ok() { passes=$((passes + 1)); echo "ok   - $1"; }
bad() { fails=$((fails + 1)); echo "FAIL - $1"; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

# --- token handling ---------------------------------------------------------
reset
if run drop-dmgs o/r dev-latest 2>"$tmp/err"; then bad "refuses without any token"; else
    check "refuses without any token" 'grep -q "neither DEVBRIDGE_GH_TOKEN_FILE nor GH_TOKEN" "$tmp/err" && [[ ! -s "$FAKE/calls" ]]'
fi

reset
export DEVBRIDGE_GH_TOKEN_FILE="$tmp/missing-token"
if run drop-dmgs o/r dev-latest 2>"$tmp/err"; then bad "refuses a missing token file"; else
    check "refuses a missing token file" 'grep -q "missing or empty" "$tmp/err"'
fi

reset
echo "tok-1" > "$tmp/token"
export DEVBRIDGE_GH_TOKEN_FILE="$tmp/token"
echo "notes" > "$tmp/notes"
echo "a" > "$tmp/A.exe"
echo "b" > "$tmp/SHA256SUMS"
run publish o/r v1.2.3 main stable "$tmp/notes" "$tmp/A.exe" "$tmp/SHA256SUMS" 2>"$tmp/err"
check "every gh call carries the token from the file" '! grep -v "^token=tok-1 " "$FAKE/calls"'
check "the token is never printed" '! grep -q "tok-1" "$tmp/err"'

# --- publish ------------------------------------------------------------------
check "stable: creates the release at the target, marked latest" 'grep -q "release create v1.2.3 -R o/r --target main --title v1.2.3 --notes-file $tmp/notes --latest" "$FAKE/calls"'
check "stable: uploads every asset" '[[ "$(cat "$FAKE/releases/v1.2.3")" == "A.exe
SHA256SUMS" ]]'

reset
export GH_TOKEN="job-token"
touch "$FAKE/releases/dev-latest"
run publish o/r dev-latest main dev "$tmp/notes" "$tmp/A.exe" 2>/dev/null
check "dev: an existing release is edited, not re-created" '! grep -q "release create" "$FAKE/calls" && grep -q "release edit dev-latest -R o/r --notes-file $tmp/notes --prerelease --latest=false" "$FAKE/calls"'
check "dev: uses GH_TOKEN when no token file" 'grep -q "^token=job-token release upload dev-latest -R o/r --clobber $tmp/A.exe" "$FAKE/calls"'

reset
export GH_TOKEN="job-token"
if run publish o/r v1 main weird "$tmp/notes" "$tmp/A.exe" 2>"$tmp/err"; then bad "rejects an unknown kind"; else
    check "rejects an unknown kind" 'grep -q "kind must be dev or stable" "$tmp/err" && [[ ! -s "$FAKE/calls" ]]'
fi
if run publish o/r v1 main stable "$tmp/notes" "$tmp/nope.exe" 2>"$tmp/err"; then bad "rejects a missing asset before any call"; else
    check "rejects a missing asset before any call" 'grep -q "asset $tmp/nope.exe not found" "$tmp/err" && [[ ! -s "$FAKE/calls" ]]'
fi

# --- drop-dmgs --------------------------------------------------------------------
reset
export GH_TOKEN="job-token"
printf 'A_0.8.1_x64-setup.exe\nA_0.8.0_aarch64.dmg\nSHA256SUMS\nA_0.7.9_aarch64.dmg\n' > "$FAKE/releases/dev-latest"
run drop-dmgs o/r dev-latest 2>"$tmp/err"
check "drop-dmgs removes every DMG and nothing else" '[[ "$(cat "$FAKE/releases/dev-latest")" == "A_0.8.1_x64-setup.exe
SHA256SUMS" ]] && grep -q "dropped 2 DMG" "$tmp/err"'

reset
export GH_TOKEN="job-token"
run drop-dmgs o/r dev-latest 2>"$tmp/err"
check "drop-dmgs is a no-op without the release" 'grep -q "no DMG to drop" "$tmp/err"'

reset
export GH_TOKEN="job-token"
mkdir -p "$tmp/bin2"
printf '#!/usr/bin/env bash\necho "HTTP 401: Bad credentials" >&2\nexit 1\n' > "$tmp/bin2/gh"
chmod +x "$tmp/bin2/gh"
if PATH="$tmp/bin2:$PATH" bash "$script" drop-dmgs o/r dev-latest 2>"$tmp/err"; then bad "an auth failure is not taken for a missing release"; else
    check "an auth failure is not taken for a missing release" 'grep -q "Bad credentials" "$tmp/err"'
fi

# --- sync-scripts -----------------------------------------------------------------
reset
export GH_TOKEN="job-token"
echo "same" > "$FAKE/remote/install.ps1"
echo "same" > "$tmp/install.ps1"
echo "old" > "$FAKE/remote/autoupdate.ps1"
echo "new" > "$tmp/autoupdate.ps1"
echo "fresh" > "$tmp/install.sh"
run sync-scripts o/r main "msg" "$tmp/install.ps1" "$tmp/autoupdate.ps1" "$tmp/install.sh" 2>"$tmp/err"
check "sync: an unchanged file is not written" '! grep -q "PUT repos/o/r/contents/install.ps1" "$FAKE/calls"'
check "sync: a changed file is written with its sha" 'grep -q "PUT repos/o/r/contents/autoupdate.ps1 -f message=msg -f branch=main -f content=$(base64 -w0 < "$tmp/autoupdate.ps1") -f sha=sha-autoupdate.ps1" "$FAKE/calls"'
check "sync: a new file is written without a sha" 'grep "PUT repos/o/r/contents/install.sh " "$FAKE/calls" | grep -qv "sha="'
check "sync: reports the count" 'grep -q "2 script(s) updated" "$tmp/err"'

# --- attach-dmg -------------------------------------------------------------------
reset
export GH_TOKEN="job-token"
touch "$FAKE/releases/v1.2.3"
printf 'aaa  DevBridge_1.2.3_x64-setup.exe\nbbb  DevBridge_1.2.2_aarch64.dmg\n' > "$FAKE/SHA256SUMS"
echo "dmg-bytes" > "$tmp/DevBridge_1.2.3_aarch64.dmg"
run attach-dmg o/r v1.2.3 "$tmp/DevBridge_1.2.3_aarch64.dmg" 2>"$tmp/err"
check "attach: uploads the DMG and SHA256SUMS" '[[ "$(cat "$FAKE/releases/v1.2.3")" == "DevBridge_1.2.3_aarch64.dmg
SHA256SUMS" ]]'
hash="$(sha256sum "$tmp/DevBridge_1.2.3_aarch64.dmg" | awk '{print $1}')"
check "attach: logs the new DMG hash" 'grep -q "attached DevBridge_1.2.3_aarch64.dmg (sha256 $hash)" "$tmp/err"'

# The SHA256SUMS rewrite itself: keep exe lines, drop old DMG lines, add this one.
reset
export GH_TOKEN="job-token"
touch "$FAKE/releases/v1.2.3"
printf 'aaa  DevBridge_1.2.3_x64-setup.exe\nbbb  DevBridge_1.2.2_aarch64.dmg\n' > "$FAKE/SHA256SUMS"
sed -i 's|"release upload") basename|"release upload") if [[ "${@: -1}" == */SHA256SUMS ]]; then cp "${@: -1}" "$FAKE/uploaded-sums"; fi; basename|' "$tmp/bin/gh"
run attach-dmg o/r v1.2.3 "$tmp/DevBridge_1.2.3_aarch64.dmg" 2>/dev/null
check "attach: SHA256SUMS keeps the exe line, drops the old DMG, adds the new one" '[[ "$(cat "$FAKE/uploaded-sums")" == "aaa  DevBridge_1.2.3_x64-setup.exe
$hash  DevBridge_1.2.3_aarch64.dmg" ]]'

echo "publish-release tests: $passes passed, $fails failed"
[[ $passes -gt 0 && $fails -eq 0 ]]

#!/usr/bin/env bash
set -euo pipefail
# Publish a DevBridge release to a GitHub repository with the gh CLI (issue #123).
#
# Used for BOTH targets so the two never drift:
#   * zbynekdrlik/devbridge-releases -- the public repo stores install and
#     auto-update from. Runs on the self-hosted runner of the `devbridge` unix
#     account on dev1 with the account's GitHub App token (owner rule: no PAT,
#     no Actions secret). DEVBRIDGE_GH_TOKEN_FILE points at that token file; it
#     is re-minted every 30 min, so it is RE-READ for every gh call (a long
#     upload must not outlive one token) and never printed.
#   * zbynekdrlik/devbridge -- GitHub-hosted job, GH_TOKEN = the job token.
#
# Commands:
#   drop-dmgs    <repo> <tag>                      delete every .dmg asset (no-op without the release)
#   publish      <repo> <tag> <target> <kind> <notes-file> <file>...
#                kind = dev (prerelease, never latest) | stable (marked latest);
#                creates the release at <target> when missing, then uploads every
#                file (--clobber) one by one
#   sync-scripts <repo> <branch> <message> <file>...
#                put each file at the repo ROOT of <branch> (contents API, one
#                commit per changed file; unchanged files are left alone)
#   attach-dmg   <repo> <tag> <dmg>                upload the DMG and a SHA256SUMS
#                that keeps the release's other lines, drops older DMG lines,
#                and adds this DMG

log() { echo "publish-release: $*" >&2; }
die() {
    log "ERROR: $*"
    exit 1
}

# gh with the right token: re-read the token file per call when one is
# configured, else the caller's GH_TOKEN.
ghx() {
    if [[ -n "${DEVBRIDGE_GH_TOKEN_FILE:-}" ]]; then
        [[ -s "$DEVBRIDGE_GH_TOKEN_FILE" ]] || die "token file DEVBRIDGE_GH_TOKEN_FILE=$DEVBRIDGE_GH_TOKEN_FILE is missing or empty"
        GH_TOKEN="$(< "$DEVBRIDGE_GH_TOKEN_FILE")" gh "$@"
    else
        [[ -n "${GH_TOKEN:-}" ]] || die "neither DEVBRIDGE_GH_TOKEN_FILE nor GH_TOKEN is set"
        gh "$@"
    fi
}

# 0 = release exists, 1 = it does not; any other failure (auth, network) dies.
release_exists() {
    local repo="$1" tag="$2" out
    if out="$(ghx release view "$tag" -R "$repo" --json tagName -q .tagName 2>&1)"; then
        return 0
    fi
    if grep -q "release not found" <<<"$out"; then
        return 1
    fi
    die "reading release $tag of $repo failed: $out"
}

release_title() {
    if [[ "$1" == "dev-latest" ]]; then
        echo "Dev Build (latest)"
    else
        echo "$1"
    fi
}

cmd_drop_dmgs() {
    [[ $# -eq 2 ]] || die "usage: drop-dmgs <repo> <tag>"
    local repo="$1" tag="$2" names name dropped=0
    if ! release_exists "$repo" "$tag"; then
        log "$repo has no release $tag yet; no DMG to drop"
        return 0
    fi
    names="$(ghx release view "$tag" -R "$repo" --json assets -q '.assets[].name')"
    while IFS= read -r name; do
        if [[ "$name" == *.dmg ]]; then
            ghx release delete-asset "$tag" "$name" -R "$repo" --yes
            dropped=$((dropped + 1))
        fi
    done <<<"$names"
    log "$repo $tag: dropped $dropped DMG(s)"
}

cmd_publish() {
    [[ $# -ge 6 ]] || die "usage: publish <repo> <tag> <target> <dev|stable> <notes-file> <file>..."
    local repo="$1" tag="$2" target="$3" kind="$4" notes="$5" f
    shift 5
    [[ -f "$notes" ]] || die "notes file $notes not found"
    for f in "$@"; do
        [[ -f "$f" ]] || die "asset $f not found"
    done
    local -a flags
    case "$kind" in
        dev) flags=(--prerelease --latest=false) ;;
        stable) flags=(--latest) ;;
        *) die "kind must be dev or stable, got '$kind'" ;;
    esac
    if release_exists "$repo" "$tag"; then
        log "$repo release $tag exists; updating notes and assets"
        ghx release edit "$tag" -R "$repo" --notes-file "$notes" "${flags[@]}"
    else
        log "$repo: creating release $tag at $target ($kind)"
        ghx release create "$tag" -R "$repo" --target "$target" --title "$(release_title "$tag")" \
            --notes-file "$notes" "${flags[@]}"
    fi
    for f in "$@"; do
        ghx release upload "$tag" -R "$repo" --clobber "$f"
        log "$repo $tag: uploaded $(basename "$f")"
    done
}

cmd_sync_scripts() {
    [[ $# -ge 4 ]] || die "usage: sync-scripts <repo> <branch> <message> <file>..."
    local repo="$1" branch="$2" message="$3" f name sha remote local_b64 out changed=0
    shift 3
    for f in "$@"; do
        [[ -f "$f" ]] || die "script $f not found"
        name="$(basename "$f")"
        local_b64="$(base64 -w0 < "$f")"
        sha=""
        remote=""
        if out="$(ghx api "repos/$repo/contents/$name?ref=$branch" -q '.sha + " " + .content' 2>&1)"; then
            sha="${out%% *}"
            remote="$(tr -d '\n' <<<"${out#* }")"
        elif ! grep -q "Not Found" <<<"$out"; then
            die "reading $name of $repo failed: $out"
        fi
        if [[ -n "$sha" && "$remote" == "$local_b64" ]]; then
            log "$repo/$name unchanged"
            continue
        fi
        local -a shaarg=()
        if [[ -n "$sha" ]]; then
            shaarg=(-f "sha=$sha")
        fi
        ghx api -X PUT "repos/$repo/contents/$name" -f "message=$message" -f "branch=$branch" \
            -f "content=$local_b64" "${shaarg[@]}" -q .commit.sha >/dev/null
        changed=$((changed + 1))
        log "$repo/$name updated"
    done
    log "$repo: $changed script(s) updated"
}

cmd_attach_dmg() {
    [[ $# -eq 3 ]] || die "usage: attach-dmg <repo> <tag> <dmg>"
    local repo="$1" tag="$2" dmg="$3" work name hash
    [[ -f "$dmg" && "$dmg" == *.dmg ]] || die "DMG $dmg not found"
    name="$(basename "$dmg")"
    hash="$(sha256sum "$dmg" | awk '{print $1}')"
    work="$(mktemp -d)"
    ghx release download "$tag" -R "$repo" -p SHA256SUMS -D "$work"
    { grep -v '\.dmg$' "$work/SHA256SUMS" || true; echo "$hash  $name"; } > "$work/SHA256SUMS.new"
    mv "$work/SHA256SUMS.new" "$work/SHA256SUMS"
    ghx release upload "$tag" -R "$repo" --clobber "$dmg"
    ghx release upload "$tag" -R "$repo" --clobber "$work/SHA256SUMS"
    rm -rf "$work"
    log "$repo $tag: attached $name (sha256 $hash)"
}

main() {
    [[ $# -ge 1 ]] || die "usage: publish-release.sh <drop-dmgs|publish|sync-scripts|attach-dmg> ..."
    local cmd="$1"
    shift
    case "$cmd" in
        drop-dmgs) cmd_drop_dmgs "$@" ;;
        publish) cmd_publish "$@" ;;
        sync-scripts) cmd_sync_scripts "$@" ;;
        attach-dmg) cmd_attach_dmg "$@" ;;
        *) die "unknown command '$cmd'" ;;
    esac
}

main "$@"

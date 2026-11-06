#!/bin/sh
# Checks a published release on a Mac: install, guides, browser, and uninstall.
#
#   curl -fsSL https://raw.githubusercontent.com/gosuda/dalzzack/<tag>/scripts/verify-release.sh | sh -s <tag>
#
# All of it runs with a temporary home and temp folder, so the real install, the
# agent files, and running daemons stay as they are. WebKit does not read HOME,
# so it can add an empty profile under the real ~/Library. Run this on the Mac
# desktop, not over SSH: it opens a browser window.
#
# Each check prints PASS or FAIL. The last line is ALL PASSED <tag>, and the
# exit status is 0 only then.

set -u

ID=verify-release

pass() {
    printf 'PASS: %s\n' "$1"
}

fail() {
    printf 'FAIL: %s\n' "$1"
    [ -z "${2:-}" ] || cat "$2"
    exit 1
}

# All work is in main, so a command that reads stdin cannot eat this script.
main() {
    tag=${1:-}
    [ -n "$tag" ] || fail "usage: verify-release.sh <tag>"
    [ "$(uname -s)" = Darwin ] || fail "this script is for macOS"
    version=${tag#v}
    version=${version%%-*}

    root=$(mktemp -d)
    dz=$root/home/.local/bin/dalzzack
    trap '"$dz" kill --id "$ID" >/dev/null 2>&1; rm -rf "$root"' EXIT
    log=$root/log
    mkdir -p "$root/home/.claude" "$root/temp" "$root/original"

    export HOME="$root/home" TMPDIR="$root/temp" CODEX_HOME="$root/home/.codex"
    export SHELL=/bin/zsh DALZZACK_OUTPUT=full DALZZACK_VERSION="$tag"
    unset ZDOTDIR DALZZACK_INSTALL_DIR DALZZACK_NO_MODIFY_PATH

    printf 'alias verify=true\n' >"$HOME/.zshenv"
    printf '# My notes\n' >"$HOME/.claude/CLAUDE.md"
    cp "$HOME/.zshenv" "$root/original/zshenv"
    cp "$HOME/.claude/CLAUDE.md" "$root/original/CLAUDE.md"

    curl -fsSL "https://raw.githubusercontent.com/gosuda/dalzzack/$tag/scripts/install.sh" \
        -o "$root/install.sh" || fail "download install.sh for $tag"
    sh "$root/install.sh" >"$log" 2>&1 || fail "install.sh installs $tag" "$log"
    pass "install.sh installs $tag and checks its hash"

    [ "$("$dz" --version)" = "dalzzack $version" ] || fail "--version prints dalzzack $version"
    pass "--version prints dalzzack $version"

    grep -qF '# Added by the dalzzack installer' "$HOME/.zshenv" ||
        fail "install.sh adds the PATH lines to .zshenv"
    pass "install.sh adds the PATH lines to .zshenv"

    "$dz" install all >"$log" 2>&1 || fail "install all" "$log"
    grep -q 'dalzzack-begin' "$HOME/.claude/CLAUDE.md" || fail "install all writes CLAUDE.md"
    grep -q 'dalzzack-begin' "$CODEX_HOME/AGENTS.md" || fail "install all writes the Codex guide"
    grep -qF '# My notes' "$HOME/.claude/CLAUDE.md" || fail "install all keeps the existing text"
    pass "install all writes both guides and keeps the existing text"

    "$dz" start --id "$ID" --ttl 5m >"$log" 2>&1 || fail "start" "$log"
    "$dz" navigate -i "$ID" --timeout-ms 3000 \
        --url 'data:text/html,%3Ch1%3Edalzzack%20verify%3C/h1%3E' >"$log" 2>&1 ||
        fail "navigate" "$log"
    "$dz" get-text -i "$ID" >"$log" 2>&1 || fail "get-text" "$log"
    grep -q 'dalzzack verify' "$log" || fail "get-text reads the page" "$log"
    pass "start, navigate, and get-text work"

    "$dz" start --id "$ID-kill" --ttl 5m >"$log" 2>&1 || fail "start a second daemon" "$log"
    "$dz" kill --id "$ID-kill" >"$log" 2>&1 || fail "kill" "$log"
    grep -Eq '"status": *"killed"' "$log" || fail "kill ends a real daemon" "$log"
    pass "kill ends a real daemon"

    data=$HOME/Library/Application\ Support/dalzzack
    [ -d "$data" ] || fail "the data folder exists before uninstall"

    files="$HOME/.zshenv $HOME/.claude/CLAUDE.md $CODEX_HOME/AGENTS.md $dz"
    # shellcheck disable=SC2086 # The paths are ours and have no spaces.
    before=$(cksum $files)
    "$dz" uninstall >"$log" 2>&1 || fail "uninstall alone" "$log"
    grep -Eq '"status": *"planned"' "$log" || fail "uninstall alone reports planned" "$log"
    # shellcheck disable=SC2086
    [ "$before" = "$(cksum $files)" ] || fail "uninstall alone changes no file"
    "$dz" get-text -i "$ID" >"$log" 2>&1 || fail "uninstall alone keeps the daemon" "$log"
    [ -d "$data" ] || fail "uninstall alone keeps the data folder"
    pass "uninstall alone reports planned and changes nothing"

    "$dz" uninstall --yes --purge >"$log" 2>&1 || fail "uninstall --yes --purge" "$log"
    grep -Eq '"status": *"completed"' "$log" || fail "uninstall --yes --purge completes" "$log"
    pass "uninstall --yes --purge completes"

    if pgrep -f "$dz" >/dev/null; then fail "the daemon stopped"; fi
    pass "the daemon stopped"

    [ ! -e "$dz" ] || fail "the binary is gone"
    pass "the binary is gone"

    cmp -s "$root/original/zshenv" "$HOME/.zshenv" || fail ".zshenv matches its original bytes"
    pass ".zshenv matches its original bytes"

    cmp -s "$root/original/CLAUDE.md" "$HOME/.claude/CLAUDE.md" ||
        fail "CLAUDE.md matches its original bytes"
    pass "CLAUDE.md matches its original bytes"

    [ ! -e "$CODEX_HOME/AGENTS.md" ] || fail "the Codex guide is gone"
    pass "the Codex guide is gone"

    [ ! -e "$data" ] || fail "the data folder is gone"
    pass "the data folder is gone"

    printf 'ALL PASSED %s\n' "$tag"
}

main "$@" </dev/null

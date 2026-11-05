#!/bin/sh
# Installs the latest dalzzack release on macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/gosuda/dalzzack/main/scripts/install.sh | sh
#
# Environment:
#   DALZZACK_INSTALL_DIR     Where to put the binary (default: ~/.local/bin).
#                            Use a folder you can write to: daemon logs go next
#                            to the binary.
#   DALZZACK_NO_MODIFY_PATH  Set to 1 to leave your shell profile alone.

set -eu

asset=dalzzack-macos-universal.tar.gz
base_url=https://github.com/gosuda/dalzzack/releases/latest/download

fail() {
    printf 'dalzzack install: %s\n' "$1" >&2
    exit 1
}

# Prints the profile file that the user's shell reads, or nothing if unknown.
# zsh: .zshenv, because agents run `zsh -c` and `zsh -lc`, which skip .zshrc.
# bash: never create .bash_profile, because it would hide an existing .profile.
shell_profile() {
    case "${SHELL##*/}" in
        zsh) printf '%s\n' "${ZDOTDIR:-$HOME}/.zshenv" ;;
        bash)
            if [ -f "$HOME/.bash_profile" ]; then
                printf '%s\n' "$HOME/.bash_profile"
            else
                printf '%s\n' "$HOME/.profile"
            fi
            ;;
    esac
}

add_to_path() {
    case ":$PATH:" in
        *":$1:"*) return 0 ;;
    esac

    profile=$(shell_profile)
    if [ -z "$profile" ]; then
        printf 'Add %s to your PATH: this script does not know your shell.\n' "$1"
        return 0
    fi

    line="export PATH=\"$1:\$PATH\""
    if ! grep -qsF "$line" "$profile"; then
        printf '\n# Added by the dalzzack installer\n%s\n' "$line" >>"$profile"
        printf 'Added %s to PATH in %s\n' "$1" "$profile"
    fi
    printf 'Open a new terminal to use dalzzack.\n'
}

# All work is in main, so a download that stops early runs nothing.
main() {
    [ "$(uname -s)" = Darwin ] ||
        fail "this script is for macOS. On Windows, use install.ps1. On Linux, build from source."

    dir=${DALZZACK_INSTALL_DIR:-$HOME/.local/bin}
    dest=$dir/dalzzack
    existing=$(command -v dalzzack 2>/dev/null || true)

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    printf 'Downloading %s\n' "$asset"
    curl -fsSL -o "$tmp/$asset" "$base_url/$asset" || fail "cannot download $asset"
    curl -fsSL -o "$tmp/$asset.sha256" "$base_url/$asset.sha256" || fail "cannot download $asset.sha256"

    expected=$(awk '{print $1}' "$tmp/$asset.sha256")
    actual=$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')
    [ "$expected" = "$actual" ] ||
        fail "checksum mismatch for $asset: expected $expected, got $actual"

    tar -xzf "$tmp/$asset" -C "$tmp"
    mkdir -p "$dir"
    # Replace the file, not its contents: macOS kills a signed binary that was
    # rewritten in place.
    rm -f "$dest"
    mv "$tmp/dalzzack" "$dest"
    chmod 755 "$dest"

    version=$("$dest" --version) || fail "$dest does not run"
    printf 'Installed %s to %s\n' "$version" "$dest"

    if [ "${DALZZACK_NO_MODIFY_PATH:-}" != 1 ]; then
        add_to_path "$dir"
    fi

    if [ -n "$existing" ] && [ "$existing" != "$dest" ]; then
        printf 'Warning: another dalzzack is on PATH at %s. Delete it, or it can hide this install.\n' "$existing" >&2
    fi

    printf '\nNext, give your agents the usage guide:\n  dalzzack install all\n'
    printf '\nTo update later:\n  dalzzack upgrade\n'
}

main

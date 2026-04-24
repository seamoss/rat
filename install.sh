#!/usr/bin/env sh
# Rat one-line installer.
#
#   curl -sSfL https://raw.githubusercontent.com/seamoss/rat/dev/install.sh | sh
#
# Environment overrides:
#   RAT_VERSION=latest            Release tag to install (default: latest).
#   RAT_PREFIX_INSTALL=$HOME/.local/bin
#                                 Directory to install rat + rat-daemon into.
#   RAT_REPO=seamoss/rat          GitHub repo (for forks / mirrors).

set -eu

REPO="${RAT_REPO:-seamoss/rat}"
PREFIX="${RAT_PREFIX_INSTALL:-$HOME/.local/bin}"
VERSION="${RAT_VERSION:-latest}"

die() {
    printf 'rat-install: %s\n' "$*" >&2
    exit 1
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux)  os_part="unknown-linux-gnu" ;;
        Darwin) os_part="apple-darwin" ;;
        *)      die "unsupported OS: $os" ;;
    esac
    case "$arch" in
        x86_64|amd64)  arch_part="x86_64" ;;
        aarch64|arm64) arch_part="aarch64" ;;
        *)             die "unsupported arch: $arch" ;;
    esac
    printf '%s-%s' "$arch_part" "$os_part"
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "required tool not found: $1"
}

need curl
need tar
need uname

target=$(detect_target)
printf 'rat-install: target=%s prefix=%s version=%s\n' "$target" "$PREFIX" "$VERSION"

if [ "$VERSION" = "latest" ]; then
    api="https://api.github.com/repos/$REPO/releases/latest"
else
    api="https://api.github.com/repos/$REPO/releases/tags/$VERSION"
fi

# Pick the first tarball asset whose URL contains our target triple.
# No jq dependency — a regex grep is fine for this one field.
asset_url=$(curl -fsSL "$api" \
    | grep -oE '"browser_download_url":[[:space:]]*"[^"]+'"$target"'[^"]*\.tar\.gz"' \
    | head -n1 \
    | sed 's/.*"browser_download_url":[[:space:]]*"//; s/"$//')

[ -n "$asset_url" ] || die "no tarball found for target $target in $VERSION release at $api"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t rat-install)
trap 'rm -rf "$tmp"' EXIT

printf 'rat-install: downloading %s\n' "$asset_url"
curl -fL -o "$tmp/rat.tar.gz" "$asset_url"

( cd "$tmp" && tar xzf rat.tar.gz )

# Taiki-e's upload action may drop the binaries at the archive root or
# inside a subdirectory named after the archive. Find them either way.
rat_bin=$(find "$tmp" -type f -name rat        | head -n1)
daemon_bin=$(find "$tmp" -type f -name rat-daemon | head -n1)
[ -n "$rat_bin"    ] || die "archive did not contain a 'rat' binary"
[ -n "$daemon_bin" ] || die "archive did not contain a 'rat-daemon' binary"

mkdir -p "$PREFIX"
install -m 0755 "$rat_bin"    "$PREFIX/rat"
install -m 0755 "$daemon_bin" "$PREFIX/rat-daemon"

printf '\nrat-install: installed rat + rat-daemon to %s\n' "$PREFIX"

case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *)
        printf '\nrat-install: %s is not on your PATH — add it to your shell rc:\n' "$PREFIX"
        printf '  export PATH="%s:$PATH"\n' "$PREFIX"
        ;;
esac

"$PREFIX/rat" --version

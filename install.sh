#!/bin/sh
# Install a released herdr-boxd binary. No sudo.
set -eu

repo="SkymanOne/herdr-boxd"

os=$(uname -s)
arch=$(uname -m)
case "$os:$arch" in
Darwin:arm64)
  target="aarch64-apple-darwin"
  ;;
Linux:x86_64)
  target="x86_64-unknown-linux-musl"
  ;;
*)
  echo "unsupported platform: $os $arch" >&2
  echo "supported: Darwin arm64 (aarch64-apple-darwin), Linux x86_64 (x86_64-unknown-linux-musl)" >&2
  exit 1
  ;;
esac

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

if [ -n "${HERDR_BOXD_VERSION:-}" ]; then
  version=${HERDR_BOXD_VERSION#v}
else
  curl -fsSL -o "$tmpdir/release.json" "https://api.github.com/repos/${repo}/releases/latest"
  version=$(awk -F'"' '/"tag_name"/ { print $4; exit }' "$tmpdir/release.json")
  version=${version#v}
  if [ -z "$version" ]; then
    echo "could not read the latest release tag" >&2
    exit 1
  fi
fi

archive="herdr-boxd-${version}-${target}.tar.gz"
base="https://github.com/${repo}/releases/download/v${version}"
curl -fsSL -o "$tmpdir/$archive" "${base}/${archive}"
curl -fsSL -o "$tmpdir/SHA256SUMS" "${base}/SHA256SUMS"

hash=$(awk -v file="$archive" '$2 == file { print $1; exit }' "$tmpdir/SHA256SUMS")
if [ -z "$hash" ]; then
  echo "SHA256SUMS has no entry for $archive" >&2
  exit 1
fi
printf '%s  %s\n' "$hash" "$archive" > "$tmpdir/check.sha256"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$tmpdir" && sha256sum -c check.sha256)
elif command -v shasum >/dev/null 2>&1; then
  (cd "$tmpdir" && shasum -a 256 -c check.sha256)
else
  echo "need sha256sum or shasum" >&2
  exit 1
fi

tar -xzf "$tmpdir/$archive" -C "$tmpdir"
dest=${HERDR_BOXD_INSTALL_DIR:-$HOME/.local/bin}
mkdir -p "$dest"
cp "$tmpdir/herdr-boxd" "$dest/herdr-boxd"
chmod 755 "$dest/herdr-boxd"
"$dest/herdr-boxd" --version

case ":${PATH}:" in
*":${dest}:"*) ;;
*) echo "warning: $dest is not on PATH" >&2 ;;
esac

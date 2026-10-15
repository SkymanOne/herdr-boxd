#!/bin/sh
# Prefer a checksum-verified release binary. Build from source if that is not available.
set -eu

root=$(CDPATH='' cd "$(dirname "$0")/.." && pwd)
cd "$root"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | sed -n 1p)
if [ -z "$version" ]; then
  echo "could not read the version from Cargo.toml" >&2
  exit 1
fi

if HERDR_BOXD_VERSION="$version" HERDR_BOXD_INSTALL_DIR="$root/target/release" sh "$root/install.sh"; then
  exit 0
fi

echo "release binary for $version is not usable; building from source" >&2
if command -v cargo >/dev/null 2>&1; then
  cargo build --release --locked
  exit 0
fi

echo "Rust and protoc are needed to build herdr-boxd from source" >&2
exit 1

#!/usr/bin/env bash
# Usage: package.sh <target> <asset> <dist-dir>
# Packages target/<target>/release/dd-test-wrapper as <dist-dir>/<asset>.tar.gz,
# with the README, the licenses, and the license texts of the crates it includes.
set -euo pipefail

target=$1
asset=$2
dist=$3

binary="target/$target/release/dd-test-wrapper"
if [[ ! -x "$binary" ]]; then
  echo "$binary does not exist. Build it with: cargo build --release --locked -p dd-test-wrapper --target $target" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cp "$binary" README.md LICENSE NOTICE "$work/"
cargo about generate --fail --locked -m crates/dd-test-wrapper/Cargo.toml --target "$target" -o "$work/COPYING" about.hbs

mkdir -p "$dist"
tar -czf "$dist/$asset.tar.gz" -C "$work" dd-test-wrapper README.md LICENSE NOTICE COPYING
echo "$dist/$asset.tar.gz"

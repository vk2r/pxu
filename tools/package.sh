#!/usr/bin/env bash
# Packages the static release binary (run `mise run release-build` first).
# Leaves pxu, pxu-<version>-x86_64-linux.tar.gz and SHA256SUMS in the output directory.
#   tools/package.sh [out-dir]    default: target/package
set -euo pipefail

out=${1:-target/package}
bin=target/x86_64-unknown-linux-musl/release/pxu
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)
name=pxu-$version-x86_64-linux

[[ -n $version ]] || { echo "package: no version in Cargo.toml" >&2; exit 1; }
[[ -x $bin ]] || { echo "package: $bin not found, run: mise run release-build" >&2; exit 1; }

rm -rf "$out"
mkdir -p "$out/$name"
cp "$bin" "$out/pxu"
cp "$bin" install.sh README.md "$out/$name/"
cp -r dist "$out/$name/dist"
[[ ! -f LICENSE ]] || cp LICENSE "$out/$name/"

tar --sort=name --owner=0 --group=0 --numeric-owner -C "$out" -czf "$out/$name.tar.gz" "$name"
rm -rf "${out:?}/$name"
(cd "$out" && sha256sum pxu "$name.tar.gz" | tee SHA256SUMS)

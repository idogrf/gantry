#!/bin/sh
# Build dist/gantry-<version>-x86_64-linux.tar.gz in an Ubuntu 22.04 container (podman),
# so the binary needs glibc 2.35 and runs on Ubuntu 22.04 and newer.
# PODMAN overrides the podman command (extra options allowed).
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
podman=${PODMAN:-podman}
toolchain=$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$root/rust-toolchain.toml")
# A worktree's first release build starts from a copy of the main checkout's compiled dependencies (a reflink where
# the filesystem supports one, else a full copy), like scripts/cargo does for debug builds. The workspace crates'
# fingerprints are dropped: the container mounts every checkout at /src, so their paths match and a copied
# fingerprint could look fresh when it is not.
common=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)
here=$(git -C "$root" rev-parse --path-format=absolute --git-dir)
main_target="$(dirname "$common")/target/ubuntu-22.04"
target="$root/target/ubuntu-22.04"
if [ "$here" != "$common" ] && [ ! -d "$target/release/deps" ] && [ -d "$main_target/release/deps" ]; then
  echo "build-tarball.sh: seeding $target from the main checkout's build (reflink if supported, else a copy)..." >&2
  mkdir -p "$target/release"
  for part in deps build .fingerprint; do
    [ -d "$main_target/release/$part" ] || continue
    cp -a --reflink=auto "$main_target/release/$part" "$target/release/" ||
      echo "build-tarball.sh: could not copy $part; it builds from scratch" >&2
  done
  for crate in "$root"/crates/*/; do
    rm -rf "$target/release/.fingerprint/$(basename "$crate")"-*
  done
fi
$podman build -q -t gantry-build-ubuntu22 --build-arg "RUST_TOOLCHAIN=$toolchain" \
  -f "$root/packaging/linux/ubuntu-22.04.Containerfile" "$root/packaging/linux" >/dev/null
# The container cannot always read git (a worktree's .git points outside /src): pass the commit in.
commit=$(git -C "$root" rev-parse --short=7 HEAD)
$podman run --rm -v "$root:/src:Z" -v gantry-cargo-ubuntu22:/cargo-home -w /src \
  -e CARGO_HOME=/cargo-home -e CARGO_TARGET_DIR=/src/target/ubuntu-22.04 -e CARGO_BUILD_JOBS=4 -e GANTRY_COMMIT="$commit" \
  gantry-build-ubuntu22 packaging/linux/pack-tarball.sh >/dev/null
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
echo "$root/dist/gantry-$version-x86_64-linux.tar.gz"

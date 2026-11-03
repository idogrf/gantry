# Contributing to Gantry

Thanks for wanting to help. Gantry is maintained by one person, so a little care up front saves everyone time.

## Before you start

- Questions and ideas go in GitHub Discussions. Bugs go in an issue.
- For anything beyond a small fix, open an issue first and wait for a reply before writing code. A pull request for a
  change nobody agreed on may be closed, however good the code is.
- Read the [AI policy](AI_POLICY.md) if any part of your change was written with an AI tool.
- Be respectful. Harassment or abuse gets you blocked, in issues, discussions and pull requests alike.

## Sign your commits (DCO)

Every commit needs a `Signed-off-by` line, which says you wrote the change or have the right to submit it under the
project's license (the [Developer Certificate of Origin](https://developercertificate.org/)). Add it with:

```sh
git commit -s
```

Contributions are licensed like the project: `MIT OR Apache-2.0`. There is no contributor license agreement.

## Build and test

You need a recent Rust toolchain (`rust-toolchain.toml` pins it). On Linux you also need development packages:

```sh
# Ubuntu
sudo apt install clang cmake libasound2-dev libfontconfig-dev libfreetype-dev libwayland-dev libxcb1-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libvulkan-dev libzstd-dev
# Fedora
sudo dnf install clang cmake alsa-lib-devel fontconfig-devel freetype-devel wayland-devel libxcb-devel \
  libxkbcommon-devel libxkbcommon-x11-devel vulkan-loader-devel libzstd-devel
```

```sh
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
```

CI runs the same checks. `scripts/cargo` is a wrapper that limits CPU and memory and allows one build at a time; plain
`cargo` works too.

## Branches and releases

- `main` is always the last release. New work does not go there directly.
- The next minor release collects on its own branch, `release/<next version>` (for example `release/0.13.0` after
  0.12.x). Branch from it and open your pull request against it (`--base release/0.13.0`), not against `main`.
- Only a fix for a bug in the released version goes to `main`, for a patch release (0.12.2). It is merged into the
  release branch afterwards, so you don't need to open it twice.
- When the release branch is ready, it is merged into `main` and tagged.
- Not sure which branch? Ask in the issue.

## Pull requests

- Keep a pull request to one change, with tests for new behavior.
- Public items need `///` docs, and new dependencies must pass `cargo deny check` (permissive licenses only).
- User-visible changes add a line under `## [Unreleased]` in `CHANGELOG.md`.
- Fill in the pull request template, including the sign-off and AI-use boxes.

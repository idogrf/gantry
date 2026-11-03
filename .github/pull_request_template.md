## What and why

<!-- One change per pull request. Link the issue this belongs to. -->

## Checklist

- [ ] This targets the open `release/x.y.z` branch (or `main` only for a fix to the released version).
- [ ] Every commit is signed off (`git commit -s`), per the DCO in CONTRIBUTING.md.
- [ ] I ran `cargo fmt --all`, `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] New behavior has tests, and a user-visible change adds a line under `## [Unreleased]` in `CHANGELOG.md`.
- [ ] AI tools wrote a substantial part of this, and I have said so above, understood it and tested it. (Leave
  unchecked if not.)

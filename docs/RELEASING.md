# Releasing

1. Bump `version` in `Cargo.toml`, run `cargo build` so `Cargo.lock`
   follows, update the action example in the README (`uses: ...@vX.Y.Z`),
   and commit.
2. Tag and push: `git tag vX.Y.Z && git push origin main vX.Y.Z`.

The `Release` workflow then:

- builds Linux (glibc 2.28, x86_64 and arm64), macOS (Apple Silicon and
  Intel) and Windows binaries and attaches them to the GitHub release;
- pushes `ghcr.io/<owner>/loadman:vX.Y.Z` and `:latest` for linux/amd64 and
  linux/arm64;
- points the Homebrew tap at the new archives.

## Homebrew tap

The `homebrew` job commits `Formula/loadman.rb` to `<owner>/homebrew-tap`.
It needs a repository secret named `HOMEBREW_TAP_TOKEN`: a fine-grained
personal access token limited to the tap repository with **Contents: Read
and write**. Without it the job only leaves a notice, and the formula can be
updated by hand:

```bash
gh release download vX.Y.Z -p 'loadman-vX.Y.Z-*.tar.gz' -D dist
.github/scripts/formula.sh vX.Y.Z dist > ../homebrew-tap/Formula/loadman.rb
```

# Releasing spot

The release workflow refuses to build unless the pushed tag matches the version
in `Cargo.toml`, so the order below matters.

1. Make sure `main` is green (`cargo fmt --all -- --check`, `cargo clippy
   --all-targets -- -D warnings`, `cargo test --all`). The fmt gate is separate
   from the test gate — run it before committing.
2. Bump `version` in `Cargo.toml`.
3. Commit the bump on its own: `git commit -am "Release vX.Y.Z"`.
4. Tag that commit: `git tag -a vX.Y.Z -m "vX.Y.Z"`.
5. Push the branch first, then the tag **by name**:

   ```sh
   git push origin main
   git push origin vX.Y.Z
   ```

   Never `git push --tags`. Stale local tags get pushed with it, and a local
   `latest` would collide with the rolling `latest` the release workflow
   publishes for the installer and self-updater.
6. Watch the Release workflow. It cross-builds Linux (x86_64, aarch64) and macOS
   (aarch64), publishes them plus `SHA256SUMS` as release assets, and those are
   what `install.sh` and `spot self update` download.

## Verifying a release

```sh
SPOT_VERSION=vX.Y.Z sh install.sh
spot --version
spot self update          # should report "Already up to date"
```

## Notes

- macOS is arm64-only on purpose: the Intel runner pool stalls in queue often
  enough to block releases, and it is not worth the wait.
- The `stay` symlink is created by `install.sh`, not by the release archive.
  A packaged build (deb) must create it in its own postinst.

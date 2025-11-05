## GitSnapFS

GitSnapFS exposes snapshots of a Git repository as a read-only filesystem designed for safe inspection, automated audits, and tooling integration.

- `/commits/<full-hex-commit-id>` presents the tree for an individual commit.
- `branches/`, `tags/`, and `HEAD` appear as symlinks into the appropriate commit snapshot.
- Every Git object maps consistently to a synthetic inode so multiple hardlinks work as expected.
- The daemon is read-only and resolves data lazily, so it tracks repository updates without a heavy startup scan.
- Hot upgrades keep the mount active by passing the FUSE file descriptor across an `exec`.

### Building & Running

Implementation is in progress. Once the Rust code is added you will be able to:

```
cargo run -- --repo <path-to-git-dir> --mountpoint <mountpoint>
```

The filesystem depends on `fuse-backend-rs` for FUSE plumbing and `gitoxide` (`gix`) for repository access. Additional CLI flags will control cache TTLs, allow-other mounts, and takeover of existing FUSE file descriptors.

### Development Notes

- Coding prompt and design details for contributors are documented in `codex_spec.md`.
- Tests will cover inode mapping, collision handling, and an integration scenario mounting a small test repository.
- Contributions should keep the filesystem strictly read-only and avoid libfuse/libgit2 C shims.

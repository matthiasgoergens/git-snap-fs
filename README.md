## GitSnapFS

GitSnapFS exposes snapshots of a Git repository as a read-only filesystem designed for safe inspection, automated audits, and tooling integration.

### Highlights

- `/commits/<full-hex-commit-id>` presents the tree for an individual commit.
- `branches/`, `tags/`, and `HEAD` materialise as symlinks into the matching commit snapshot.
- Synthetic inodes are derived from Git object IDs so links remain stable across views.
- The filesystem is strictly read-only and answers requests lazily; updates in the underlying repo are surfaced without a pre-scan.
- Hot upgrades keep the mount active by duping the FUSE file descriptor across an `exec`.
- Directory listings leave `.` and `..` to the kernel, letting path caches stay in userspace.
- We leverage the kernel’s zero-message open/opendir paths (`NO_OPEN_SUPPORT`, `NO_OPENDIR_SUPPORT`) for near-native performance once data is cached.

### Requirements

- Linux with FUSE kernel support that advertises `EXPORT_SUPPORT`, `ZERO_MESSAGE_OPEN`, and `ZERO_MESSAGE_OPENDIR`.
- `fusermount`/`fusermount3` (typically provided by `fuse` packages).
- Rust toolchain nightly or stable recent enough to build the dependency graph (`cargo`, `rustc`).

### Quick Start

```bash
cargo run -- --repo path/to/.git --mountpoint /tmp/gitfs
```

The mount exposes the root layout (`commits`, `branches`, `tags`, `HEAD`). Unmount with:

```bash
fusermount -u /tmp/gitfs   # or fusermount3 -u
```

### Development

- Design notes live in `codex_spec.md`.
- Formatting and linting are enforced with the following commands:

  ```bash
  cargo fmt
  cargo clippy --all-targets --all-features -- -D clippy::pedantic -D clippy::style -D clippy::cargo
  ```

- The `clippy.toml` documents unavoidable duplicate crate versions coming from upstream dependencies.
- Please keep the filesystem read-only and avoid libfuse/libgit2 shims; all Git access goes through `gix` and FUSE plumbing through `fuse-backend-rs`.

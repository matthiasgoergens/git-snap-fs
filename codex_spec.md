# Codex Prompt & Design Summary

## Prompt

SYSTEM (to the code assistant)  
You are an expert Rust engineer. Produce high-quality, idiomatic Rust code that compiles on stable Rust.  
Always explain nontrivial design choices in concise comments.

### Task

Implement a read-only FUSE filesystem (“GitSnapFS”) that exposes a Git repository (.git dir or bare repo) such that:
  • Each commit appears as a directory representing its snapshot.  
  • Branches (and optionally tags) appear as symbolic links to their target commit directory.  
  • Objects with the same Git id map to the same inode: derive a 64-bit inode from the object id.  
  • The filesystem is strictly read-only.  
  • No full-repo scan at startup; everything is resolved lazily, and the view updates as the repo updates.  
  • The daemon can hot-upgrade via exec without unmounting, keeping the FUSE fd and enough state to finish in-flight ops.

### Constraints & Requirements

Language & libs (pure Rust preferred):
  • Git access: gitoxide (`gix` crate); open bare or worktree repos.  
  • FUSE server: `fuse-backend-rs` (fusedev path over /dev/fuse). If you absolutely must, `fuser` is acceptable as a fallback, but prefer `fuse-backend-rs`.  
  • No C libraries (e.g., libgit2, libfuse) in the hot path.

Visible layout (example):
  ```
  /
  ├─ commits/<full-hex-commit-id>/{snapshot...}
  ├─ branches/<refname>  -> ../commits/<resolved-commit-id>
  ├─ tags/<tagname>      -> ../commits/<resolved-commit-id>
  └─ HEAD                -> ../commits/<resolved-commit-id>
  ```

Implementation notes:
  • Lookup of `/commits/<id>/...` resolves the commit (accept full hex; short ids optional) → tree → descend path on demand.  
  • `/branches`, `/tags`, `/HEAD` are symlinks resolved lazily; optionally apply a small TTL (e.g., 1–2 s) or use inotify+FUSE notify to invalidate when refs change.  
  • Git symlinks (mode 120000) are exposed as POSIX symlinks; blob contents are the link target.  
  • Submodules (mode 160000): ignore initially (treat as empty read-only dir or a stub entry), to be added later.

Inode mapping (stable and deduplicating):
  • `inode = (low_60_bits(oid) | (type_tag << 60))`, where `type_tag ∈ {0:blob, 1:tree, 2:commit, 3:symlink, 0x7F:synthetic}`.  
  • Maintain a lazy, process-lifetime map: `used: HashMap<ino, (full_oid, type_tag)>`.  
  • On first encounter of `(ino)`: insert. If the same ino is seen with a different `(oid, type_tag)`, mark a collision:  
    – The first object keeps working normally.  
    – For the second (and later) objects, fail lookup/open/getattr with `-EUCLEAN`.  
    (No startup scan; only objects actually traversed populate the table.)  
  • Optionally persist used/clash to a small state file (e.g., `${XDG_RUNTIME_DIR}/gitfs-<mount-id>.state`) so collisions survive exec upgrades.

Hot upgrade (exec) without unmount:
  • Mount using `/dev/fuse` and clear `FD_CLOEXEC` on the FUSE fd.  
  • Provide a “takeover” mode that adopts an existing FUSE fd from env var `GITSNAPFS_FUSE_FD`.  
  • Re-exec path: pause accepting new requests briefly, exec the new binary (same PID), inherit FUSE fd via env, and resume serving.  
  • Keep file handles stateless: for read-only blobs/dirs, set `fh = ino` and avoid per-handle mutable state.  
  • Readdir offsets must be deterministic: use index+1 over a stable (sorted) entry order so post-exec continuation works.

Operations to implement (minimum set):
  • `lookup`, `getattr`, `readdir` (and `readdirplus` if available), `open` (RO), `read`, `readlink`.  
  • Strict RO: fail `create`/`mkdir`/`rename`/`unlink`/`link`/`write`/`chmod`/`chown`/`utimens` with `EROFS`.

Attributes & times:
  • Files: mode from tree entry (`100644 → 0o444`, `100755 → 0o555` or `0o755` but RO), size = blob size.  
  • Directories: `0o555`.  
  • Symlinks: `0o777` (ignored by most clients).  
  • Timestamps: For entries under `/commits/<id>`, use the commit’s committer time. (Commits themselves are distinct inodes, so this does not violate “same object → same inode” for blobs/trees.)

Scalability notes:
  • Never list all commits in `/commits`. Support direct addressing and enumerate only refs (branches/tags). Optionally shard `commits/<aa>/<bb>/<cc>/<full>` for bounded directory sizes.  
  • Use small LRUs for decoded trees and small blobs; rely on `gix` for pack access/mmap.

CLI (long options only):
  ```
  --repo <PATH>            # path to .git or bare repo
  --mountpoint <PATH>
  --allow-other            # optional
  --attr-ttl <SECONDS>     # default 300 for commit trees; 2 for refs
  --entry-ttl <SECONDS>
  --ref-ttl <SECONDS>      # if not using inotify
  --tree-cache <COUNT>     # e.g., 4096
  --blob-small-cache <BYTES> # e.g., 134217728
  --takeover-fuse-fd <FD>  # internal: adopt existing mount
  --state-file <PATH>      # optional: persist collision table across exec
  ```

Deliverables:
  1. `Cargo.toml` with `gix` and `fuse-backend-rs` dependencies (stable Rust).  
  2. `src/main.rs` with argument parsing, adopt-or-mount logic, server loop, and (optional) signal-based reexec.  
  3. `src/fs.rs` implementing the filesystem trait (`lookup`/`getattr`/`readdir`/`readdirplus`/`open`/`read`/`readlink`).  
  4. `src/inode.rs` providing inode mapping and collision tracking.  
  5. `src/state.rs` for (de)serializing the collision table to a file or memfd.  
  6. `src/upgrade.rs` providing helpers to clear CLOEXEC and exec the new binary with the inherited fd.  
  7. `README.md` with usage examples and caveats.

Quality bar:
  • Compile on stable Rust.  
  • Unit tests for inode mapping (type tags, truncation) and collision detection.  
  • A small integration test that mounts a tiny test repo and reads a known file through `/commits/<id>`.  
  • Clear comments at each kernel/userspace boundary and for any unsafe blocks.

## Design Summary

### Goals

Expose a Git repository as a read-only filesystem:
- One directory per commit (`/commits/<full-hex-commit-id>`), containing the snapshot of that commit.  
- `branches/`, `tags/`, and `HEAD` are symlinks to the corresponding commit directories.  
- Same Git object id → same inode (64-bit), using truncated object id. (Later investigate sparing 4-bits for a type tag.)  
- No repo-wide scan at startup; everything is on-demand and cacheable.  
- Daemon can exec-upgrade without unmounting; file handles and directory offsets do not require in-process state to be valid. But we need to be careful of keeping the fuse fd open across exec (or fork/exec).

### Key Policies

- **Inode mapping**: `inode = (low_40_bits(object_id)` (later try `inode = (low_60_bits(object_id) | (type_tag << 60))`).  
- **Collision handling**:  
  - Ignore for the first version.  
  - Later lazy detection per touched object. First object “wins”; subsequent conflicting objects return `-EUCLEAN`. Optionally expose `/collisions/<ino>` for diagnostics.  
- **Read-only**: any VFS modifying operation returns `-EROFS`.  
- **Cached** in kernel: use as much of FUSE’s caches as possible. Don’t do our own caching (unless profiling later suggests otherwise).  
- **Times**: use commit’s committer time for entries beneath `/commits/<id>`; dirs `0555`; files `0444/0555`; symlinks `0777`. (But we can review this. Do whatever is simplest first.)

### Freshness

- We can notify the kernel when `/branches/*`, `/tags/*`, and `/HEAD` change on our side. The contents under a commit id are immutable, so no need to invalidate those.  
- Use FUSE’s attr and entry TTLs to control staleness window.  
- Optional inotify watcher on `.git/HEAD`, `.git/refs/**`, `.git/packed-refs` to issue FUSE notifications to invalidate dentries/inodes when ref targets change.

### Hot Upgrade via exec

- Mount with `/dev/fuse` fd that has `FD_CLOEXEC` cleared.  
- On upgrade trigger, `execve()` the new binary with env `GITSNAPFS_FUSE_FD=<fd>` and (optionally) `GITSNAPFS_STATE=<path_or_fd>`.  
- On startup, if `GITSNAPFS_FUSE_FD` is present, adopt the existing mount instead of remounting.  
- Make file handles and dir handles stateless (use `fh = ino`, deterministic readdir offsets).  
- Check if we need to hand over any state to the new binary. If yes, serialize and pass via memfd or temp file. (Depends on handling inflight requests during the upgrade; read-only semantics help here.)  
- Take inspiration from how XMonad does hot upgrades.

### Minimal API Surface

- `lookup`, `getattr`, `readdir`/`readdirplus`, `open` (RO), `read`, `readlink` (and whatever else FUSE requires for a minimal FS that only supports reading).  
- Reject all others with `-EROFS`.

### Data Structures

- We need some metadata, but most of it can be lazily loaded from the Git repo on demand.

### Testing Checklist

- Inode mapping tests (type tag isolation; truncation correctness). Do this later, if at all, if/when we implement type tags.  
- Collision test: inject two fake oids that collide in low 60 bits; ensure first works, second fails with `EUCLEAN`. Do this later.  
- Git symlink behavior (mode 120000 → readlink target equals blob content).  
- Readdir determinism: offsets persist across restarts/exec (sorted entries; index+1). We can probably rely on Git’s tree ordering for this.  
- Hot upgrade: mount, open, trigger exec, continue reading using same fd.

### CLI Examples

```
gitsnapfs --repo /home/matthias/project/.git \
          --mountpoint /mnt/gitfs
```

Decide how to trigger hot upgrades. E.g., by writing the new executable to a specific path that we expose in the fs. This would be the only writeable file. (Need to see how to do this atomically. Closing the fake “file” after writing it might be sufficient. Then we can write the new binary somewhere the kernel can use for exec, or use memfd.)

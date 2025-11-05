# GitSnapFS — Design Summary (for the code assistant)

## Goals
Expose a Git repository as a read-only filesystem:
- One directory per commit (`/commits/<full-hex-commit-id>`), containing the snapshot of that commit.
- `branches/`, `tags/`, and `HEAD` are symlinks to the corresponding commit directories.
- Same Git object id → same inode (64-bit), using truncated object id plus a 4-bit type tag.
- No repo-wide scan at startup; everything is on-demand and cacheable.
- Daemon can exec-upgrade without unmounting; file handles and directory offsets do not require in-process state to be valid.

## Key Policies
- **Inode mapping**: `inode = (low_60_bits(object_id) | (type_tag << 60))`.
- **Collision handling**: lazy detection per touched object. First object “wins”; subsequent conflicting objects return `-EUCLEAN` on lookup/open/getattr/read. Optionally expose `/collisions/<ino>` for diagnostics.
- **Read-only**: any VFS modifying operation returns `-EROFS`.
- **Times**: use commit’s committer time for entries beneath `/commits/<id>`; dirs 0555; files 0444/0555; symlinks 0777.

## Freshness
- `/branches/*`, `/tags/*`, and `/HEAD` resolve on each lookup or under a small TTL (e.g., 2s).
- Optional inotify watcher on `.git/HEAD`, `.git/refs/**`, `.git/packed-refs` to issue FUSE notifications to invalidate dentries/inodes when ref targets change.

## Hot Upgrade via exec
- Mount with `/dev/fuse` fd that has `FD_CLOEXEC` cleared.
- On upgrade trigger, `execve()` the new binary with env `GITSNAPFS_FUSE_FD=<fd>` and (optionally) `GITSNAPFS_STATE=<path_or_fd>`.
- On startup, if `GITSNAPFS_FUSE_FD` is present, adopt the existing mount instead of remounting.
- Make file handles and dir handles stateless (use `fh = ino`, deterministic readdir offsets).

## Minimal API Surface
- `lookup`, `getattr`, `readdir`/`readdirplus`, `open` (RO), `read`, `readlink`.
- Reject all others with `-EROFS`.

## Data Structures
- `InoBook { used: HashMap<u64, (Oid, Type)>, clash: HashSet<u64> }` with (de)serialization to a small file/memfd.
- Tree LRU: `HashMap<Oid, Arc<Tree>>` (bounded).
- Optional small-blob cache by size.

## Testing Checklist
- Inode mapping tests (type tag isolation; truncation correctness).
- Collision test: inject two fake oids that collide in low 60 bits; ensure first works, second fails with EUCLEAN.
- Git symlink behavior (mode 120000 → readlink target equals blob content).
- Readdir determinism: offsets persist across restarts/exec (sorted entries; index+1).
- Hot upgrade: mount, open, trigger exec, continue reading using same fd.

## CLI Examples
```
gitsnapfs --repo /home/matthias/project/.git \
          --mountpoint /mnt/gitfs \
          --allow-other \
          --attr-ttl 300 --entry-ttl 300 \
          --ref-ttl 2 \
          --tree-cache 4096 --blob-small-cache 134217728

# Hot upgrade signal (example)
kill --signal SIGUSR2 "$(cat /run/user/$UID/gitsnapfs.pid)"
```

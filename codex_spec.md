# GitSnapFS — Design Summary (for the code assistant)

## Goals
Expose a Git repository as a read-only filesystem:
- One directory per commit (`/commits/<full-hex-commit-id>`), containing the snapshot of that commit.
- `branches/`, `tags/`, and `HEAD` are symlinks to the corresponding commit directories.
- Same Git object id → same inode (64-bit), using truncated object id.  (Later investigate sparing 4-bits for a type tag.)
- No repo-wide scan at startup; everything is on-demand and cacheable.
- Daemon can exec-upgrade without unmounting; file handles and directory offsets do not require in-process state to be valid.  But we need to be careful of keeping the fuse fd open across exec (or fork/exec).

## Key Policies
- **Inode mapping**: `inode = (low_40_bits(object_id) ` (later try `inode = (low_60_bits(object_id) | (type_tag << 60))`.)
- **Collision handling**: 
  - Ignore for the first version.
  - Later lazy detection per touched object. First object “wins”; subsequent conflicting objects return `-EUCLEAN` on lookup/open/getattr/read. Optionally expose `/collisions/<ino>` for diagnostics.
- **Read-only**: any VFS modifying operation returns `-EROFS`.
- **Cached** in kernel: use as much of fuses's caches as possible.  Don't do our own caching.  (Unless later profiling reveals that we could benefit from our own cache.)
- **Times**: use commit’s committer time for entries beneath `/commits/<id>`; dirs 0555; files 0444/0555; symlinks 0777. (But we can review this.  Do whatever is simplest first.)

## Freshness
- We can notify the kernel when `/branches/*`, `/tags/*`, and `/HEAD` change on our side.  The contents under a commit id are immutable, so no need to invalidate those.
- Use FUSE’s attr and entry TTLs to control staleness window.
- Optional inotify watcher on `.git/HEAD`, `.git/refs/**`, `.git/packed-refs` to issue FUSE notifications to invalidate dentries/inodes when ref targets change.

## Hot Upgrade via exec
- Mount with `/dev/fuse` fd that has `FD_CLOEXEC` cleared.
- On upgrade trigger, `execve()` the new binary with env `GITSNAPFS_FUSE_FD=<fd>` and (optionally) `GITSNAPFS_STATE=<path_or_fd>`.
- On startup, if `GITSNAPFS_FUSE_FD` is present, adopt the existing mount instead of remounting.
- Make file handles and dir handles stateless (use `fh = ino`, deterministic readdir offsets).
- Check if we need to hand over any state to the new binary.  If yes, decide how we want to do that.  Probably serialialise and pass via memfd or temp file.  (It might depend on what we do for inflight requests during the upgrade.  Or whether that's a problem at all.  Going read-only from the fs side helps  lot here.)
- Take inspiration from how XMonad does hot upgrades.

## Minimal API Surface
- `lookup`, `getattr`, `readdir`/`readdirplus`, `open` (RO), `read`, `readlink` (and what ever else FUSE requires for a minimal FS that only supports reading).
- Reject all others with `-EROFS`.

## Data Structures
- We need some metadata, but most of it can be lazily loaded from the Git repo on demand.

## Testing Checklist
- Inode mapping tests (type tag isolation; truncation correctness).  Do this later, if at all, if/when we implement type tags.
- Collision test: inject two fake oids that collide in low 60 bits; ensure first works, second fails with EUCLEAN.  Do this later.
- Git symlink behavior (mode 120000 → readlink target equals blob content).
- Readdir determinism: offsets persist across restarts/exec (sorted entries; index+1).  We can probably rely on Git's tree ordering for this.  I suspect the git hash / id already fixes the order.
- Hot upgrade: mount, open, trigger exec, continue reading using same fd.

## CLI Examples
```
gitsnapfs --repo /home/matthias/project/.git \
          --mountpoint /mnt/gitfs
```

Decide how to trigger hot upgrades.  Eg by writing the new executable to a specific path that we expose in the fs.  This would be the only writeable file.  (We need to see how we can do this atomically.  But I suspect closing the fake 'file' after writing it is sufficient.  It's an inode we control.  Then we can write the new binary to somewhere where the kernel can take it for exec or so.  Or we can directly do it via memfd?)

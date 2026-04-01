# hashtree-fuse

FUSE filesystem mount for hashtree content-addressed trees.

Exposes a hashtree merkle tree as a local filesystem via FUSE. Supports read/write operations — writes update the merkle root and optionally publish it.

## Usage

```rust
use hashtree_fuse::HashtreeFuse;

let fs = HashtreeFuse::new(tree, root_cid, Some(publisher));
// Mount with fuser
```

## Features

- Read files and directories from a merkle tree
- Write support: create, rename, remove files/dirs
- Root publishing on write (optional `RootPublisher` trait)
- Inode-based lookup with path caching

Requires the `fuse` feature flag for the FUSE backend (`fuser` + `libc`).

Part of [hashtree](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).

# hashtree-fs

Filesystem-based content-addressed blob storage for hashtree.

Simple storage backend that stores blobs as files on disk, organized by hash prefix for efficient lookup.

## Usage

```rust
use hashtree_fs::FsStore;
use hashtree_core::Store;

let store = FsStore::new("/path/to/data")?;

// Store a blob
store.put(&hash, &data)?;

// Retrieve a blob
let data = store.get(&hash)?;
```

## Storage Layout

Blobs are stored in a directory structure based on hash prefix:
```
data/
  ab/
    cd/
      ef1234...
  cd/
    ef/
      5678...
```

Part of [hashtree-rs](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).

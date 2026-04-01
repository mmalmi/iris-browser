# hashtree-index

Content-addressed B-tree indexes for hashtree.

The crate builds deterministic index trees on top of `hashtree-core`, so index
roots can be stored, replicated, and compared the same way as file trees.

Current uses include:

- key/value lookup over immutable tree roots
- link indexes that point directly at content-addressed blobs
- cross-language index interoperability with the TypeScript implementation

Part of [hashtree](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).

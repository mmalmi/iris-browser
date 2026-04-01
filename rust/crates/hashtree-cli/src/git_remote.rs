//! Git remote helper binary - thin wrapper around git-remote-htree crate
//!
//! Included by default. Disable with `--no-default-features` to exclude.

fn main() {
    git_remote_htree::main_entry();
}

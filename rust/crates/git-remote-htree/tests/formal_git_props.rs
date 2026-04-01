use std::collections::HashMap;
use std::process::Command;

use git_remote_htree::git::object::ObjectId;
use git_remote_htree::git::refs::{validate_ref_name, Ref};
use git_remote_htree::git::storage::GitStorage;
use tempfile::TempDir;

fn run_git(cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("failed to run git command")
}

fn git_stdout(cwd: &std::path::Path, args: &[&str]) -> String {
    let output = run_git(cwd, args);
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn test_non_fast_forward_condition_detected_by_merge_base() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    assert!(run_git(repo, &["init", "-q"]).status.success());
    assert!(
        run_git(repo, &["config", "user.email", "formal@example.com"])
            .status
            .success()
    );
    assert!(run_git(repo, &["config", "user.name", "Formal Tester"])
        .status
        .success());

    std::fs::write(repo.join("file.txt"), "base\n").unwrap();
    assert!(run_git(repo, &["add", "file.txt"]).status.success());
    assert!(run_git(repo, &["commit", "-qm", "base"]).status.success());
    let base_branch = git_stdout(repo, &["rev-parse", "--abbrev-ref", "HEAD"]);

    assert!(run_git(repo, &["checkout", "-qb", "feature"])
        .status
        .success());
    std::fs::write(repo.join("file.txt"), "feature\n").unwrap();
    assert!(run_git(repo, &["commit", "-am", "feature", "-q"])
        .status
        .success());
    let feature_tip = git_stdout(repo, &["rev-parse", "HEAD"]);

    assert!(run_git(repo, &["checkout", "-q", &base_branch])
        .status
        .success());
    std::fs::write(repo.join("file.txt"), "master\n").unwrap();
    assert!(run_git(repo, &["commit", "-am", "master", "-q"])
        .status
        .success());
    let master_tip = git_stdout(repo, &["rev-parse", "HEAD"]);

    // This mirrors helper non-fast-forward check input:
    // is remote tip (master_tip) an ancestor of local tip (feature_tip)?
    let output = run_git(
        repo,
        &["merge-base", "--is-ancestor", &master_tip, &feature_tip],
    );
    assert_eq!(output.status.code(), Some(1), "expected not-ancestor");
}

#[test]
fn test_ref_deletion_affects_only_target() {
    let temp_dir = TempDir::new().unwrap();
    let storage = GitStorage::open(temp_dir.path()).unwrap();

    storage.import_ref("refs/heads/main", "sha_main").unwrap();
    storage.import_ref("refs/heads/dev", "sha_dev").unwrap();
    storage.import_ref("refs/tags/v1", "sha_tag").unwrap();

    let deleted = storage.delete_ref("refs/heads/dev").unwrap();
    assert!(deleted);

    let refs = storage.list_refs().unwrap();
    assert!(refs.contains_key("refs/heads/main"));
    assert!(!refs.contains_key("refs/heads/dev"));
    assert!(refs.contains_key("refs/tags/v1"));
}

#[test]
fn test_ref_name_validation_constraints_hold() {
    let valid = [
        "refs/heads/main",
        "refs/heads/feature/test",
        "refs/tags/v1.0.0",
        "HEAD",
    ];
    for name in valid {
        assert!(validate_ref_name(name).is_ok(), "{name} should be valid");
    }

    let invalid = [
        "",
        "/refs/heads/main",
        "refs/heads/main/",
        "refs//heads",
        "refs/heads/..",
        "refs/heads/test.lock",
        "refs/heads/te st",
        "@",
    ];
    for name in invalid {
        assert!(validate_ref_name(name).is_err(), "{name} should be invalid");
    }
}

#[test]
fn test_untouched_refs_preserved_when_updating_single_ref() {
    let temp_dir = TempDir::new().unwrap();
    let storage = GitStorage::open(temp_dir.path()).unwrap();

    storage.import_ref("refs/heads/main", "old_main").unwrap();
    storage.import_ref("refs/heads/dev", "sha_dev").unwrap();
    storage.import_ref("refs/tags/v1", "sha_tag").unwrap();

    let new_oid = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
    storage
        .write_ref("refs/heads/main", &Ref::Direct(new_oid))
        .unwrap();

    let refs: HashMap<String, String> = storage.list_refs().unwrap();
    assert_eq!(
        refs.get("refs/heads/main"),
        Some(&"0123456789abcdef0123456789abcdef01234567".to_string())
    );
    assert_eq!(refs.get("refs/heads/dev"), Some(&"sha_dev".to_string()));
    assert_eq!(refs.get("refs/tags/v1"), Some(&"sha_tag".to_string()));
}

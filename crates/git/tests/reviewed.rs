use std::path::Path;
use std::process::Command;

use mergepatrol_git::{Git, RebaseStep};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn commit(dir: &Path, file: &str, content: &str) -> String {
    std::fs::write(dir.join(file), content).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", "change"]);
    git(dir, &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn a_net_update_keeps_a_test_added_only_in_a_merge_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    commit(dir, "code.ts", "original\n");
    git(dir, &["checkout", "-q", "-b", "pr"]);
    commit(dir, "code.ts", "feature\n");
    git(dir, &["checkout", "-q", "main"]);
    commit(dir, "base.txt", "base change\n");
    git(dir, &["checkout", "-q", "pr"]);
    git(dir, &["merge", "--no-commit", "--no-ff", "main"]);
    commit(
        dir,
        "code.test.ts",
        "it('keeps the reviewed regression', () => expect(feature()).toBe(true));\n",
    );
    let reviewed = git(dir, &["rev-parse", "HEAD"]);
    assert_eq!(
        git(dir, &["rev-list", "--parents", "-1", "HEAD"])
            .split_whitespace()
            .count(),
        3
    );
    git(dir, &["checkout", "-q", "main"]);
    let trunk = commit(dir, "later.txt", "main advanced again\n");
    git(dir, &["checkout", "-q", "--detach", &reviewed]);
    assert_eq!(
        Git.rebase(dir, &trunk, None).await.unwrap(),
        RebaseStep::Done
    );
    assert!(
        !dir.join("code.test.ts").exists(),
        "the old replay reproduces the loss"
    );
    let error = Git
        .preserve_reviewed_content(dir, &trunk, &reviewed, "HEAD", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("code.test.ts"), "{error}");
    git(dir, &["checkout", "-q", "--detach", &reviewed]);
    assert_eq!(
        Git.rebase_reviewed(dir, &trunk, None).await.unwrap(),
        RebaseStep::Done
    );
    assert_eq!(git(dir, &["rev-parse", "HEAD~1"]), trunk);
    assert_eq!(
        git(dir, &["show", "HEAD:code.test.ts"]),
        git(dir, &["show", &format!("{reviewed}:code.test.ts")])
    );
    Git.preserve_reviewed_content(dir, &trunk, &reviewed, "HEAD", None)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_net_update_keeps_a_conflict_fix_committed_only_in_a_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    commit(dir, "code.ts", "original\n");
    git(dir, &["checkout", "-q", "-b", "pr"]);
    commit(dir, "code.ts", "feature\n");
    git(dir, &["checkout", "-q", "main"]);
    commit(dir, "code.ts", "main change\n");
    git(dir, &["checkout", "-q", "pr"]);
    assert!(
        !Command::new("git")
            .current_dir(dir)
            .args(["merge", "--no-commit", "main"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let reviewed = commit(dir, "code.ts", "feature and main integrated correctly\n");
    git(dir, &["checkout", "-q", "main"]);
    let trunk = commit(dir, "later.txt", "main advanced\n");
    git(dir, &["checkout", "-q", "--detach", &reviewed]);
    assert_eq!(
        Git.rebase_reviewed(dir, &trunk, None).await.unwrap(),
        RebaseStep::Done
    );
    assert_eq!(
        git(dir, &["show", "HEAD:code.ts"]),
        "feature and main integrated correctly"
    );
    Git.preserve_reviewed_content(dir, &trunk, &reviewed, "HEAD", None)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_guard_checks_modifications_deletions_modes_and_unusual_paths_but_exempts_base_changes()
{
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    commit(dir, "modified.txt", "original\n");
    let root = commit(dir, "deleted.txt", "remove me\n");
    commit(dir, "modified.txt", "reviewed\n");
    commit(dir, "added.txt", "reviewed new file\n");
    commit(dir, "tab\tand\nnewline.txt", "reviewed unusual path\n");
    git(dir, &["rm", "-q", "deleted.txt"]);
    git(dir, &["commit", "-q", "-m", "reviewed deletion"]);
    let reviewed = git(dir, &["rev-parse", "HEAD"]);
    git(dir, &["checkout", "-q", "--detach", &root]);
    let trunk = commit(dir, "unrelated.txt", "main\n");
    git(dir, &["checkout", "-q", "--detach", &reviewed]);
    Git.rebase_reviewed(dir, &trunk, None).await.unwrap();
    Git.preserve_reviewed_content(dir, &trunk, &reviewed, "HEAD", None)
        .await
        .unwrap();
    let good = git(dir, &["rev-parse", "HEAD"]);
    for path in ["added.txt", "tab\tand\nnewline.txt"] {
        git(dir, &["rm", "-q", path]);
    }
    commit(dir, "modified.txt", "damaged\n");
    commit(dir, "deleted.txt", "restored without review\n");
    let error = Git
        .preserve_reviewed_content(dir, &trunk, &reviewed, "HEAD", None)
        .await
        .unwrap_err()
        .to_string();
    for path in [
        "added.txt",
        "modified.txt",
        "deleted.txt",
        "tab\\tand\\nnewline.txt",
    ] {
        assert!(error.contains(path), "{error}");
    }
    git(dir, &["checkout", "-q", "--detach", &good]);
    git(dir, &["update-index", "--chmod=+x", "added.txt"]);
    git(dir, &["commit", "-q", "-m", "changed mode"]);
    git(dir, &["reset", "--hard", "HEAD"]);
    assert!(
        Git.preserve_reviewed_content(dir, &trunk, &reviewed, "HEAD", None)
            .await
            .unwrap_err()
            .to_string()
            .contains("added.txt")
    );
    git(dir, &["checkout", "-q", "--detach", &root]);
    let overlapping = commit(dir, "modified.txt", "different main content\n");
    git(dir, &["checkout", "-q", "--detach", &good]);
    let result = commit(dir, "modified.txt", "agent resolution\n");
    Git.preserve_reviewed_content(dir, &overlapping, &reviewed, &result, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn content_now_on_the_base_and_stack_layer_boundaries_are_not_lost() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    let root = commit(dir, "base.txt", "base\n");
    let lower = commit(dir, "lower.txt", "lower layer\n");
    let reviewed = commit(dir, "upper.txt", "upper layer\n");
    git(dir, &["checkout", "-q", "--detach", &root]);
    commit(dir, "lower.txt", "lower layer updated on base\n");
    let trunk = commit(dir, "other.txt", "main\n");
    git(dir, &["checkout", "-q", "--detach", &reviewed]);
    assert_eq!(
        Git.rebase_reviewed(dir, &trunk, Some(&lower))
            .await
            .unwrap(),
        RebaseStep::Done
    );
    assert_eq!(
        git(dir, &["show", "HEAD:lower.txt"]),
        "lower layer updated on base"
    );
    assert_eq!(git(dir, &["show", "HEAD:upper.txt"]), "upper layer");
    Git.preserve_reviewed_content(dir, &trunk, &reviewed, "HEAD", Some(&lower))
        .await
        .unwrap();
    git(dir, &["checkout", "-q", "--detach", &root]);
    commit(dir, "lower.txt", "lower layer\n");
    let squashed = commit(dir, "other.txt", "main\n");
    git(dir, &["checkout", "-q", "--detach", &reviewed]);
    Git.rebase_reviewed(dir, &squashed, None).await.unwrap();
    Git.preserve_reviewed_content(dir, &squashed, &reviewed, "HEAD", None)
        .await
        .unwrap();
}

#[tokio::test]
async fn net_updates_keep_fix_budgets_across_repeated_updates_and_an_author_push_resets_them() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    let root = commit(dir, "base.txt", "base\n");
    commit(dir, "feature.txt", "feature\n");
    for n in 1..=3 {
        git(
            dir,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("fix {n}\n\n{}", mergepatrol_core::FIXED_BY),
            ],
        );
    }
    let reviewed = git(dir, &["rev-parse", "HEAD"]);
    git(dir, &["checkout", "-q", "--detach", &root]);
    let trunk = commit(dir, "main.txt", "main\n");
    git(dir, &["checkout", "-q", "--detach", &reviewed]);
    Git.rebase_reviewed(dir, &trunk, None).await.unwrap();
    let first = git(dir, &["rev-parse", "HEAD"]);
    let message = Git.commit_message(dir, "HEAD").await.unwrap();
    assert_eq!(mergepatrol_core::fixes_used([message.as_str()]), 3);
    git(dir, &["checkout", "-q", "--detach", &trunk]);
    let next_trunk = commit(dir, "main.txt", "main advanced\n");
    git(dir, &["checkout", "-q", "--detach", &first]);
    Git.rebase_reviewed(dir, &next_trunk, None).await.unwrap();
    let message = Git.commit_message(dir, "HEAD").await.unwrap();
    assert_eq!(mergepatrol_core::fixes_used([message.as_str()]), 3);
    let authored = commit(dir, "feature.txt", "author's new code\n");
    git(dir, &["checkout", "-q", "--detach", &next_trunk]);
    let next_trunk = commit(dir, "main.txt", "main advanced again\n");
    git(dir, &["checkout", "-q", "--detach", &authored]);
    Git.rebase_reviewed(dir, &next_trunk, None).await.unwrap();
    let message = Git.commit_message(dir, "HEAD").await.unwrap();
    assert_eq!(mergepatrol_core::fixes_used([message.as_str()]), 0);
}

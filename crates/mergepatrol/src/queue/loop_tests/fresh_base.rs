use super::*;

#[tokio::test(start_paused = true)]
async fn old_green_behind_main_waits_for_rebased_checks() {
    for prep in [false, true] {
        let mut run = Run::on_the_lane(Fake::new(|s| {
            behind_and_green(s);
            s.config = Some("test_current_main: true\nfix_ci: { enabled: false }\n".into());
            s.required = vec!["ci".into()];
            s.checks_on.insert(
                "pr-1-r1".into(),
                [("ci".into(), CheckState::Pending)].into(),
            );
        }));
        run.ctx.config.prep = prep;
        let cut = tokio::time::timeout(Duration::from_secs(120), run.tick()).await;
        assert!(cut.is_err(), "the old green head must not merge");
        assert!(run.merged().is_empty());
        assert_eq!(run.rebase.count(), 1);
        assert_eq!(run.gh.with(|s| s.head(1)), "pr-1-r1");
        assert!(!run.kinds(1).iter().any(|k| k == "rebase-skipped"));
        run.gh.with_mut(|s| {
            s.checks_on.insert(
                "pr-1-r1".into(),
                [("ci".into(), CheckState::Failed)].into(),
            );
        });
        run.rebase.rebases_once = true;
        run.tick().await;
        assert!(run.merged().is_empty(), "new failures must not reuse old green checks");
        assert_eq!(run.kinds(1).last().unwrap(), "ejected");
    }
}

#[tokio::test(start_paused = true)]
async fn main_moving_as_single_checks_pass_restarts_before_merge() {
    let run = Run::on_the_lane(Fake::new(|s| {
        s.add_pr(1);
        s.config = Some("test_current_main: true\n".into());
    }));
    run.gh.then(|s| s.trunk = "outside".into());
    run.tick().await;
    assert_eq!(run.rebase.count(), 2);
    assert_eq!(run.merged(), [1]);
    assert_eq!(run.kinds(1), ["enqueued", "rebasing", "published", "base-moved", "rebasing", "published", "merged"]);
}

#[tokio::test(start_paused = true)]
async fn main_moving_as_batch_checks_pass_rebuilds_before_merge() {
    let run = Run::batching_on_the_lane(Fake::new(|s| {
        prs(&[1, 2])(s);
        s.config = Some("test_current_main: true\n".into());
    }));
    run.gh.then(|_| {});
    run.gh.then(|_| {});
    run.gh.then(|s| s.trunk = "outside".into());
    run.tick().await;
    assert!(run.merged().is_empty());
    assert_eq!(run.kinds(1), ["enqueued", "batch", "batch-stopped"]);
    run.tick().await;
    assert_eq!(run.merged(), [1, 2]);
    assert_eq!(run.gh.with(|s| s.drafts.len()), 2);
}

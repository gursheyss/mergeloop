use super::*;

#[tokio::test(start_paused = true)]
async fn a_batch_uses_live_main_even_when_the_snapshot_never_updates() {
    let run = Run::batching_on_the_lane(Fake::new(|s| {
        prs(&[1, 2])(s);
        s.config = Some("test_current_main: true\n".into());
        s.snapshot_trunk = Some(s.trunk.clone());
        s.trunk = "outside".into();
        s.trees.insert("outside".into(), "t-9".into());
    }));
    run.tick().await;
    assert_eq!(run.merged(), [1, 2]);
    assert_eq!(run.gh.with(|s| s.tree(&s.trunk)), "t-9-1-2");
    assert_eq!(run.gh.with(|s| s.drafts.len()), 1);
}

#[tokio::test(start_paused = true)]
async fn a_pending_batch_stops_on_live_main_without_a_push_webhook() {
    let run = Run::batching_on_the_lane(Fake::new(|s| {
        prs(&[1, 2])(s);
        s.config = Some("test_current_main: true\n".into());
        s.snapshot_trunk = Some(s.trunk.clone());
        let head = "mergepatrol/batch-1-2@t-1-2";
        s.checks_on
            .insert(head.into(), [("ci".into(), CheckState::Pending)].into());
        s.trunk_on_checks.insert(head.into(), "outside".into());
    }));
    assert!(
        tokio::time::timeout(Duration::from_secs(120), run.tick())
            .await
            .is_ok()
    );
    assert!(run.merged().is_empty());
    assert_eq!(run.kinds(1).last().unwrap(), "batch-stopped");
    run.gh.with_mut(|s| s.checks_on.clear());
    run.tick().await;
    assert_eq!(run.merged(), [1, 2]);
    assert_eq!(run.gh.with(|s| s.drafts.len()), 2);
}

#[tokio::test(start_paused = true)]
async fn current_main_keeps_five_pr_batches_and_internal_merge_progress() {
    let run = Run::batching_on_the_lane(Fake::new(|s| {
        prs(&[1, 2, 3, 4, 5])(s);
        s.config = Some("test_current_main: true\nbatch_size: 5\nfix_ci: { enabled: true, attempts: 1 }\nverify: ['git diff --check origin/main...HEAD']\n".into());
    }));
    run.tick().await;
    assert_eq!(run.merged(), [1, 2, 3, 4, 5]);
    assert_eq!(run.rebase.count(), 0);
    assert_eq!(run.gh.with(|s| s.drafts.len()), 1);
    assert_eq!(run.gh.with(|s| s.tree(&s.trunk)), "t-1-2-3-4-5");
}

#[tokio::test(start_paused = true)]
async fn current_main_resumes_pending_and_already_landed_merges() {
    for batching in [false, true] {
        for hang in [MergeHang::Pending, MergeHang::Merged] {
            let gh = Fake::new(|s| {
                s.add_pr(1);
                if batching {
                    s.add_pr(2);
                }
                s.config = Some("test_current_main: true\n".into());
            });
            let run = if batching {
                Run::batching_on_the_lane(gh)
            } else {
                Run::on_the_lane(gh)
            };
            cut_short_while_merging(&run, hang).await;
            run.drain().await;
            assert_eq!(run.merged(), if batching { vec![1, 2] } else { vec![1] });
            assert_eq!(run.gh.with(|s| s.merges), if batching { 2 } else { 1 });
            assert_eq!(
                run.exits()
                    .iter()
                    .filter(|(number, outcome, _)| *number == 1 && outcome == "merged")
                    .count(),
                1
            );
            assert!(run.lane().is_empty());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn current_main_resumes_a_partially_merged_batch() {
    for hang in [MergeHang::Pending, MergeHang::Merged] {
        let run = Run::batching_on_the_lane(Fake::new(|s| {
            prs(&[1, 2, 3])(s);
            s.config = Some("test_current_main: true\n".into());
            s.merge_hangs_at = Some(2);
        }));
        cut_short_while_merging(&run, hang).await;
        assert_eq!(
            run.store
                .lanes
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .merging
                .as_ref()
                .unwrap()
                .base
                .as_deref(),
            Some("merge-1")
        );
        run.drain().await;
        assert_eq!(run.merged(), [1, 2, 3]);
        assert_eq!(run.gh.with(|s| s.merges), 3);
        assert_eq!(run.exits().len(), 3);
        assert!(run.lane().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn current_main_restarts_stale_pending_merges_but_records_landed_ones() {
    for hang in [MergeHang::Pending, MergeHang::Merged] {
        let run = Run::on_the_lane(Fake::new(|s| {
            s.add_pr(1);
            s.config = Some("test_current_main: true\n".into());
        }));
        cut_short_while_merging(&run, hang).await;
        run.gh.with_mut(|s| s.trunk = "outside".into());
        run.tick().await;
        assert_eq!(run.merged(), [1]);
        assert_eq!(
            run.rebase.count(),
            match hang {
                MergeHang::Pending => 2,
                MergeHang::Merged => 1,
            }
        );
        assert_eq!(run.gh.with(|s| s.merges), 1);
        assert_eq!(run.exits().len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn current_main_retests_legacy_pending_merges_without_a_tested_base() {
    let run = Run::on_the_lane(Fake::new(|s| {
        s.add_pr(1);
        s.config = Some("test_current_main: true\n".into());
    }));
    cut_short_while_merging(&run, MergeHang::Pending).await;
    for lane in run.store.lanes.lock().unwrap().values_mut() {
        lane.merging.as_mut().unwrap().base = None;
    }
    run.tick().await;
    assert_eq!(run.merged(), [1]);
    assert_eq!(run.rebase.count(), 2);
}

#[tokio::test(start_paused = true)]
async fn current_main_does_not_skip_fork_edit_restrictions_for_old_green() {
    let run = Run::on_the_lane(Fake::new(|s| {
        fork_pr(s);
        s.prs.get_mut(&1).unwrap().maintainer_can_modify = false;
        s.behind.push("pr-1-v0".into());
        s.config = Some("test_current_main: true\n".into());
    }));
    run.tick().await;
    assert!(run.merged().is_empty());
    assert_eq!(run.rebase.count(), 0);
    assert!(run.lane()[0].1.contains("ForkEdits"));
}

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
            s.checks_on
                .insert("pr-1-r1".into(), [("ci".into(), CheckState::Failed)].into());
        });
        run.rebase.rebases_once = true;
        run.tick().await;
        assert!(
            run.merged().is_empty(),
            "new failures must not reuse old green checks"
        );
        assert_eq!(run.kinds(1).last().unwrap(), "ejected");
    }
}

#[tokio::test(start_paused = true)]
async fn main_moving_as_single_checks_pass_restarts_before_merge() {
    let run = Run::on_the_lane(Fake::new(|s| {
        s.add_pr(1);
        s.config = Some("test_current_main: true\n".into());
        s.required = vec!["ci".into()];
        s.trunk_on_checks.insert("pr-1-r1".into(), "outside".into());
    }));
    run.tick().await;
    assert_eq!(run.rebase.count(), 2);
    assert_eq!(run.merged(), [1]);
    assert_eq!(
        run.kinds(1),
        [
            "enqueued",
            "rebasing",
            "published",
            "base-moved",
            "rebasing",
            "published",
            "merged"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn main_moving_as_batch_checks_pass_rebuilds_before_merge() {
    let run = Run::batching_on_the_lane(Fake::new(|s| {
        prs(&[1, 2])(s);
        s.config = Some("test_current_main: true\n".into());
        s.required = vec!["ci".into()];
        s.trunk_on_checks
            .insert("mergepatrol/batch-1-2@t-1-2".into(), "outside".into());
    }));
    run.tick().await;
    assert!(run.merged().is_empty());
    assert_eq!(run.kinds(1), ["enqueued", "batch", "batch-stopped"]);
    run.tick().await;
    assert_eq!(run.merged(), [1, 2]);
    assert_eq!(run.gh.with(|s| s.drafts.len()), 2);
}

#[tokio::test(start_paused = true)]
async fn old_green_behind_main_merges_only_after_new_green() {
    let mut run = Run::on_the_lane(Fake::new(|s| {
        behind_and_green(s);
        s.config = Some("test_current_main: true\nfix_ci: { enabled: false }\n".into());
        s.required = vec!["ci".into()];
        s.checks_on.insert(
            "pr-1-r1".into(),
            [("ci".into(), CheckState::Pending)].into(),
        );
    }));
    run.ctx.config.prep = false;
    assert!(
        tokio::time::timeout(Duration::from_secs(120), run.tick())
            .await
            .is_err()
    );
    assert!(run.merged().is_empty());
    assert_eq!(run.gh.with(|s| s.head(1)), "pr-1-r1");
    run.rebase.rebases_once = true;
    run.gh.with_mut(|s| {
        s.checks_on
            .insert("pr-1-r1".into(), [("ci".into(), CheckState::Passed)].into());
    });
    run.tick().await;
    assert_eq!(run.merged(), [1]);
    assert!(!run.kinds(1).iter().any(|k| k == "rebase-skipped"));
}

#[tokio::test(start_paused = true)]
async fn outside_merge_between_batch_members_stops_the_remainder() {
    let run = Run::batching_on_the_lane(Fake::new(|s| {
        prs(&[1, 2])(s);
        s.config = Some("test_current_main: true\n".into());
        s.trunk_after_merge.insert(1, "outside".into());
    }));
    run.tick().await;
    assert_eq!(run.merged(), [1]);
    assert_eq!(run.kinds(2).last().unwrap(), "batch-stopped");
    run.drain().await;
    assert_eq!(run.merged(), [1, 2]);
}

#[tokio::test(start_paused = true)]
async fn outside_merge_during_batch_retry_stops_before_retrying() {
    let run = Run::batching_on_the_lane(Fake::new(|s| {
        prs(&[1, 2])(s);
        s.config = Some("test_current_main: true\n".into());
        s.rejects_once.push(1);
        s.trunk_after_merge.insert(1, "outside".into());
    }));
    run.tick().await;
    assert!(run.merged().is_empty());
    assert_eq!(run.kinds(1).last().unwrap(), "batch-stopped");
    run.drain().await;
    assert_eq!(run.merged(), [1, 2]);
}

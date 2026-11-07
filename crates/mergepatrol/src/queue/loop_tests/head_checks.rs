use super::*;
use crate::config::HeadCheck;

const REVIEW: &str = "review";

/// \`review\` must pass on each PR's own head, beside the required \`ci\`.
fn reviewed(mut run: Run) -> Run {
    run.ctx.config.head_checks = vec![HeadCheck {
        repo: repo(),
        name: REVIEW.into(),
    }];
    run
}

/// \`ci\` and \`review\` as given, on \`sha\`.
fn on(s: &mut State, sha: &str, review: CheckState) {
    let checks = BTreeMap::from([
        ("ci".to_owned(), CheckState::Passed),
        (REVIEW.to_owned(), review),
    ]);
    s.checks_on.insert(sha.into(), checks);
}

/// halo#4226: \`review\` passed on the head that was queued and failed on the head the rebase
/// published, which then merged once \`ci\` passed.
fn red_once_rebased(s: &mut State) {
    s.add_pr(1);
    s.required = vec!["ci".into()];
    on(s, "pr-1-v0", CheckState::Passed);
    on(s, "pr-1-r1", CheckState::Failed);
}

fn last_comment(run: &Run, number: PrNumber) -> String {
    run.gh.with(|s| {
        s.comments
            .iter()
            .rev()
            .find(|(n, _)| *n == number)
            .map(|(_, c)| c.clone())
            .unwrap_or_default()
    })
}

#[tokio::test(start_paused = true)]
async fn without_head_checks_a_red_review_on_the_rebased_head_merges() {
    let run = Run::on_the_lane(Fake::new(red_once_rebased));
    run.tick().await;
    assert_eq!(run.merged(), [1]);
}

#[tokio::test(start_paused = true)]
async fn a_red_review_on_the_rebased_head_ejects_naming_it_instead_of_merging() {
    let run = reviewed(Run::on_the_lane(Fake::new(red_once_rebased)));
    run.tick().await;
    assert!(run.merged().is_empty());
    assert_eq!(
        run.kinds(1),
        ["enqueued", "rebasing", "published", "ejected"]
    );
    assert_eq!(run.labels(1), [FAILED]);
    assert!(run.rebase.fixed().is_empty(), "no fix for a review");
    let comment = last_comment(&run, 1);
    assert!(
        comment.contains("Ejected: review failed on pr-1-r"),
        "{comment}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_review_still_running_on_the_head_that_merges_is_waited_for() {
    let run = reviewed(Run::on_the_lane(Fake::new(|s| {
        s.add_pr(1);
        s.required = vec!["ci".into()];
        on(s, "pr-1-v0", CheckState::Passed);
        on(s, "pr-1-r1", CheckState::Pending);
    })));
    for _ in 0..4 {
        run.gh.then(|_| {});
    }
    run.gh.then(|s| on(s, "pr-1-r1", CheckState::Passed));
    let start = tokio::time::Instant::now();
    run.tick().await;
    assert_eq!(run.merged(), [1]);
    assert!(start.elapsed() >= CHECKS_POLL, "it waited for the review");
}

#[tokio::test(start_paused = true)]
async fn a_red_review_on_the_queued_head_ejects_before_any_rebase() {
    let run = reviewed(Run::new(Fake::new(|s| {
        s.add_pr(1);
        s.required = vec!["ci".into()];
        on(s, "pr-1-v0", CheckState::Failed);
    })));
    run.tick().await;
    assert!(run.merged().is_empty());
    assert_eq!(run.rebase.count(), 0);
    assert_eq!(run.kinds(1), ["enqueued", "ejected"]);
    let comment = last_comment(&run, 1);
    assert!(
        comment.contains("review failed on pr-1-v0, and this repo needs it to pass"),
        "{comment}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_review_that_never_reports_ejects_once_ci_is_done() {
    let run = reviewed(Run::on_the_lane(Fake::new(|s| {
        s.add_pr(1);
        s.required = vec!["ci".into()];
    })));
    let start = tokio::time::Instant::now();
    run.tick().await;
    assert!(run.merged().is_empty());
    assert!(start.elapsed() >= CHECKS_MISSING_FOR);
    let comment = last_comment(&run, 1);
    assert!(comment.contains("never reported"), "{comment}");
    assert!(comment.contains(": review."), "{comment}");
}

#[tokio::test(start_paused = true)]
async fn a_pr_waits_off_the_lane_while_its_review_runs() {
    let run = reviewed(prep_run(|s| {
        s.add_pr(1);
        s.add_pr(2);
        s.required = vec!["ci".into()];
        on(s, "pr-1-v0", CheckState::Pending);
        on(s, "pr-2-v0", CheckState::Passed);
        on(s, "pr-2-r1", CheckState::Passed);
    }));
    run.tick().await;
    run.tick().await;
    assert_eq!(run.lane(), [(1, "Held(PrepChecks)".into())]);
    assert_eq!(run.merged(), [2]);
}

/// PRs whose own heads passed \`ci\` and \`review\`. The batch's draft runs \`ci\` only.
fn reviewed_prs(numbers: &[PrNumber]) -> impl FnOnce(&mut State) {
    let numbers = numbers.to_vec();
    move |s| {
        prs(&numbers)(s);
        for n in numbers {
            on(s, &format!("pr-{n}-v0"), CheckState::Passed);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_batch_needs_the_review_on_each_prs_own_head_not_on_its_draft() {
    let run = reviewed(Run::batching(Fake::new(reviewed_prs(&[1, 2]))));
    run.tick().await;
    assert_eq!(run.merged(), [1, 2]);
    assert_eq!(run.kinds(1), ["enqueued", "batch", "merged"]);
    assert_eq!(run.gh.with(|s| s.drafts.len()), 1);
}

#[tokio::test(start_paused = true)]
async fn a_pr_whose_review_is_red_stays_out_of_a_batch_and_is_ejected() {
    let run = reviewed(Run::batching(Fake::new(|s| {
        reviewed_prs(&[1, 2, 3])(s);
        on(s, "pr-2-v0", CheckState::Failed);
        on(s, "pr-1-r1", CheckState::Passed);
        on(s, "pr-3-r2", CheckState::Passed);
        on(s, "pr-3-r1", CheckState::Passed);
    })));
    run.drain().await;
    assert_eq!(run.merged(), [1, 3]);
    assert!(run.gh.with(|s| s.drafts.is_empty()), "no batch with #2");
    assert_eq!(run.kinds(2).last().unwrap(), "ejected");
    assert!(
        last_comment(&run, 2).contains("review failed on pr-2-v0"),
        "{}",
        last_comment(&run, 2)
    );
}

/// Once the batch's draft is open, \`review\` on #2's head turns \`review\`.
fn turns_once_batched(run: &Run, review: CheckState) {
    for _ in 0..40 {
        run.gh.then(move |s| {
            if !s.drafts.is_empty() {
                on(s, "pr-2-v0", review);
            }
        });
    }
}

#[tokio::test(start_paused = true)]
async fn a_review_that_turns_red_before_its_batch_merges_ejects_that_pr_and_stops_the_rest() {
    let run = reviewed(Run::batching(Fake::new(reviewed_prs(&[1, 2, 3]))));
    turns_once_batched(&run, CheckState::Failed);
    run.tick().await;
    assert_eq!(run.merged(), [1], "#3 merges untested after #2 no more");
    assert_eq!(run.kinds(2).last().unwrap(), "ejected");
    assert_eq!(run.labels(2), [FAILED]);
    assert!(run.lane().iter().any(|(n, _)| *n == 3), "#3 waits its turn");
}

#[tokio::test(start_paused = true)]
async fn a_review_running_again_before_its_batch_merges_stops_the_batch_there() {
    let run = reviewed(Run::batching(Fake::new(reviewed_prs(&[1, 2, 3]))));
    turns_once_batched(&run, CheckState::Pending);
    run.tick().await;
    assert_eq!(run.merged(), [1]);
    assert_eq!(
        messages(&run, 2, "batch-stopped"),
        ["review on #2 must pass first"]
    );
    assert!(run.labels(2).contains(&TRIGGER.to_owned()));
}

#[tokio::test(start_paused = true)]
async fn a_merge_cut_short_by_a_restart_goes_ahead_only_while_the_review_passed() {
    let run = reviewed(Run::new(Fake::new(|s| {
        s.add_pr(1);
        s.required = vec!["ci".into()];
        on(s, "pr-1-v0", CheckState::Passed);
        on(s, "pr-1-r1", CheckState::Passed);
    })));
    cut_short_while_merging(&run, MergeHang::Pending).await;
    run.gh.with_mut(|s| on(s, "pr-1-r1", CheckState::Failed));
    run.tick().await;
    assert!(run.merged().is_empty());
    assert_eq!(run.gh.with(|s| s.merges), 0);
    assert_eq!(run.kinds(1).last().unwrap(), "ejected");
}

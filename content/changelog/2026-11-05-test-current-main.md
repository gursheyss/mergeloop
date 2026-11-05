---
title: Opt in to testing PRs on current main
kind: new
---

Repos can set `test_current_main: true` in their queue settings to test behind-main PRs after rebasing, instead of merging on old green checks. It is off by default, and batches still test together.

The queue checks for outside changes before merging and restarts stale work, including after a restart. GitHub does not lock the base between that check and the merge, so a simultaneous outside merge can still race it.

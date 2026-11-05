---
title: Keep reviewed content when updating a PR
kind: fixed
---

Updating a PR keeps changes committed inside merge commits. Repos can also enable `preserve_reviewed_content: true` to eject an update or agent fix that changes a PR-touched file the target branch did not change, with the affected paths in the reason.

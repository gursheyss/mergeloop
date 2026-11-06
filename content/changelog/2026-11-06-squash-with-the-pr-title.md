---
title: Squash merges take the PR's title and description
kind: fixed
---

A squash merge's commit is now the PR's title with its number, `Title (#123)`, followed by the PR's description and the co-authors its commits credit. Before, a repo whose default worded a single-commit squash by its message got the last commit's message as the subject, and an update onto the base always leaves the PR one commit. Stacks keep GitHub's wording.

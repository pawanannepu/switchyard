# Review

Reviewing, comparing and shipping what agents produce. Back to the [index](index.md).

## Slice 5: Review and compare

### 20. Line level diff review · needs a decision
A built in diff viewer where you review, comment on, edit, accept or reject changes line by line.
**Done when:** you review a workspace's changes file by file; you can accept or reject each hunk or line, edit lines in place, and leave line comments that go back to the agent as a follow up; all of it works by keyboard; the diff updates while the agent keeps working.
- [ ] Design it (spec): `/architect line level diff review`

### 21. Editor & terminal handoff
One click to open a workspace in your own editor or a terminal at its worktree.
**Done when:** from any workspace, one action opens the worktree in your chosen editor, and another opens a terminal there; the editor is configurable and detected where possible.
- [ ] Build it: `/develop editor & terminal handoff`

### 22. Compare agents & pick a winner · needs a decision
Give the same task to several agents or models, compare their results side by side, then merge the winner.
**Done when:** one task fans out to several agents or models, each in its own worktree; you see their diffs, status and time side by side; you pick a winner and merge it; the other worktrees and branches are cleaned up after a confirmation.
- [ ] Design it (spec): `/architect compare agents & pick a winner`

## Slice 8: Pull requests and CI

### 28. Commit & pull request creation · needs a decision
Commit, push and open a pull request with an agent written description, using your own signed in tools.
**Done when:** you can commit a workspace's changes, push its branch and open a pull request; an agent drafts the title and description and you edit them before anything is sent; pushing always asks first; switchyard stores no forge tokens.
- [ ] Design it (spec): `/architect commit & pull request creation`

### 29. CI status · needs a decision
See each pull request's CI checks on its workspace and the board.
**Done when:** a workspace with an open pull request shows its CI checks as they update; a failing check opens its log; you can hand a failure back to the agent as a follow up task.
- [ ] Design it (spec): `/architect ci status`

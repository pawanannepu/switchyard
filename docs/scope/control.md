# Control

Keeping many agents safe, approved and within limits. Back to the [index](index.md).

## Slice 2: Run many safely

### 11. Worktree sandbox & destructive action guard · needs a decision
Keep each agent confined to its own worktree, and make sure nothing destructive happens to your repository without your confirmation.
**Done when:** an agent process cannot write outside its worktree; destructive actions (discarding a workspace, deleting a branch, force operations, overwriting uncommitted changes) always ask first; a killed or crashed agent can't leave the repo in a broken state; tests cover each guard with the fake agent.
- [ ] Design it (spec): `/architect worktree sandbox & destructive action guard`

### 12. Approvals inbox · needs a decision
Every agent's permission requests in one place, so you can approve across all agents without hunting through terminals.
**Done when:** permission requests from all running agents appear in one inbox with what's being asked and by which agent; you can approve or deny from there by keyboard; the answer reaches the agent through its official CLI; each project has a default permission mode.
- [ ] Design it (spec): `/architect approvals inbox`

## Slice 7: Limits and cost

### 26. Concurrency limits & queue · needs a decision
Clear limits on how many agents run at once, so a big batch doesn't overload your machine.
**Done when:** you can set a global and a per project limit on running agents; tasks over the limit wait in a visible queue and start when a slot frees; the board shows what's running and what's queued.
- [ ] Design it (spec): `/architect concurrency limits & queue`

### 27. Usage, cost & time tracking · needs a decision
See what each task, agent and project costs in time, tokens and money.
**Done when:** each task records running time, and token usage and cost when its agent's CLI reports them; totals show per task, per agent and per project; when an agent doesn't report usage, it's shown as unknown, never guessed; all of it stays local.
- [ ] Design it (spec): `/architect usage, cost & time tracking`

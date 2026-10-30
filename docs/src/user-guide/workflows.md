# Isolated concurrent workflows

A workflow is session-owned work with an objective, lifecycle, supervisor, and optional isolated Git worktree. Its state is managed independently from the currently displayed panel. Workflows are not shell processes or recurring prompts.

## Start and inspect

```text
/workflow
/workflow list
/workflow create Improve parser diagnostics
/workflow create parser-review Improve parser diagnostics
/workflow show [id|name]
```

Bare `/workflow` opens the workflow page in the terminal interface. Creation accepts an objective, optionally preceded by a name. Inspect the created workflow to obtain its identifier, name, status, worktree, branch, supervisor, and failure or integration information.

The manager owns workflow identity and status. Terminal and Web views project its snapshots; they do not infer completion from the presence of a branch or worktree.

## Lifecycle operations

`/workflow pause`, `/workflow resume`, `/workflow cancel`, `/workflow integrate`, and `/workflow remove` target the selected workflow; the parser also supports selecting a target by identifier or name. If no workflow is selected, the command requires a target rather than choosing an arbitrary workflow.

Lifecycle operations use the workflow's current generation to reject stale mutation attempts. Refresh the displayed state after a conflicting operation instead of reusing an old snapshot.

Integration is distinct from execution completion. Integration state reports either no integration, an applied result commit, or conflicts. Resolve reported conflicts explicitly; do not interpret a finished supervisor as proof that changes reached the parent workspace.

## Isolation boundaries

A workflow worktree separates concurrent file modifications from the main working tree. Tool execution still uses the configured filesystem, approval, and sandbox authority. Worktree isolation is not a replacement for process or network confinement.

Workflow state is scoped to the owning session. A session switch changes the displayed session, not permission to adopt another session's workflow identifiers. Removal and cancellation are explicit operations; closing a panel is neither.

## Operating practice

1. Give each workflow a concrete objective and exclusive file ownership.
2. Inspect its worktree and supervisor state while it runs.
3. Review the produced change and reported verification.
4. Integrate deliberately and inspect the integration result.
5. Remove only the workflow you intend to retire.

Use [Todos](todos.md) for dependency scheduling and [orchestration](orchestration.md) for child-task communication. See [sandbox isolation](../reference/sandbox-isolation.md) for the host security boundary.

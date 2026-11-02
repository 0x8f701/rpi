# Goals

A goal records an objective, lifecycle, usage budget, and pinned instructions for the active session. It is not the same as a Todo dependency graph or an isolated workflow.

## Commands

```text
/goal
/goal create --tokens 100000 Review and improve the parser
/goal pause
/goal resume
/goal complete
/goal drop
/goal pin Preserve the public parser interface
/goal pins
/goal unpin 1
```

Bare `/goal`, `show`, `get`, and `inspect` display state. `create` and `set` accept an objective; text without an operation also creates a goal. The objective must be nonempty. `--tokens` must precede the objective and take a positive integer.

Creating or resuming goal work reports whether it started, queued, or was already active. This is execution state, not a claim that the objective has been achieved.

## Lifecycle

The lifecycle is `active`, `paused`, `completed`, or `dropped`.

- Manual pause is resumable with `/goal resume`.
- A goal restored with the session-resume safety pause requires explicit `/goal resume`.
- Budget exhaustion is shown as a pause that cannot resume.
- Complete records completion; drop records abandonment of the goal.

Pins are numbered from one in the displayed list. Use `/goal pins` before removing a pin by index.

## Working safely

Give the goal a concrete outcome and review evidence before marking it complete. A token budget is an execution budget, not a price guarantee. Tool authority and project trust still apply to goal-driven work.

Use [Todos](todos.md) to express dependencies and [workflows](workflows.md) for isolated concurrent work. The [Web client](web.md) projects application state rather than creating a separate goal owner.

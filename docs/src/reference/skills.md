# Skills

Skills are Markdown files with YAML frontmatter that provide specialized instructions for specific tasks. They are listed in the system prompt and loaded on demand by the model through the `read` tool using `skill://` URIs.

For how skills differ from prompt templates, agents, and context files, see [`prompt-templates.md`](prompt-templates.md#how-templates-differ-from-skills-agents-and-context-files).

In this document `<agent-dir>` means the resolved agent configuration directory (`PI_CODING_AGENT_DIR` or the platform default), and `<workspace>` means the current working directory passed to the session.

## File format and frontmatter

A skill is usually a directory containing `SKILL.md`:

```text
<agent-dir>/skills/rust-review/
  └── SKILL.md
```

```markdown
---
name: rust-review
description: Review Rust code for idioms and safety.
globs:
  - "*.rs"
  - "src/**/*.rs"
alwaysApply: false
hidden: false
disable-model-invocation: false
---

# Rust review

Check for unnecessary clones, panic paths, and async cancellation safety.
```

Fields:

| Field | Required | Purpose |
|-------|----------|---------|
| `name` | No | Skill identifier. Defaults to the directory name. Must be lowercase `a-z`, `0-9`, and hyphens; ≤ 64 UTF-16 code units; must not start/end with a hyphen or contain `--`. |
| `description` | **Yes** | Used in the system prompt and selector. Must be non-empty and ≤ 1,024 UTF-16 code units. A skill with no description is dropped. |
| `globs` | No | Path patterns that trigger the selector when the user's request mentions matching files. YAML list or comma-separated string. |
| `alwaysApply` / `always-apply` | No | When the YAML boolean `true`, the skill is autoloaded into the context. |
| `hide` / `hidden` | No | When the YAML boolean `true`, the skill is excluded from the `<available_skills>` block and selector. |
| `disable-model-invocation` | No | When the YAML boolean `true`, the skill is hidden from the system prompt. Quoted `"true"` is treated as the string `true` and does **not** hide the skill. |

Only the plain YAML boolean `true` counts for the boolean fields; quoted values are strings.

## Discovery

Skills are discovered in these locations, in precedence order:

1. `<agent-dir>/skills/` — global user skills, always scanned.
2. `<workspace>/.pi/skills/` — project-local skills, scanned only when the project is trusted.
3. Settings `skills` and package `skills` resources.
4. Explicit `--skill PATH` arguments.

Discovery rules from `load_skills_from_dir` in [`crates/pi-coding/src/resources.rs`](../../../crates/pi-coding/src/resources.rs):

- A directory containing `SKILL.md` is a **skill root**; its children are not scanned further.
- At a root without `SKILL.md`, direct `.md` children are loaded and subdirectories are scanned recursively for `SKILL.md`.
- `node_modules`, hidden directories, and entries matched by `.gitignore`, `.ignore`, or `.fdignore` are skipped.
- Symlinks are followed, but real paths are de-duplicated to avoid loops.

Global skills are marked `source: User` and trusted. Project skills are `source: Project` and trusted only when the project is trusted. Explicit skills are `source: Explicit` and trusted. Package skills use `PackageGlobal` or `PackageProject` source and follow the package trust rules from [`packages.md`](packages.md).

During default directory scanning, the first skill with a given name wins: global skills are loaded before project skills, so a global skill shadows a project skill of the same name. After configured, package, and explicit skills are merged in, any remaining duplicate name is a hard error.

## Trust gates and CLI flags

Project-local `.pi/skills/` are loaded only when the resolved project trust decision allows project resources. Trust is resolved by [`crates/pi-coding/src/trust.rs`](../../../crates/pi-coding/src/trust.rs):

1. One-run `--approve` / `--no-approve`.
2. Persisted decision in `<agent-dir>/trust.json`.
3. `defaultProjectTrust` in settings (`ask`, `always`, `never`).
4. In headless modes, an unset decision is treated as untrusted.

If a project has no `.pi` directory, it is trusted by default.

```sh
rpi --skill ./my-skills/rust-review.md --print "review src/main.rs"
rpi --no-skills --print "hello"     # disables discovered/configured skills, explicit paths still load
rpi --no-context-files --print "hello" # also skip AGENTS.md / CLAUDE.md
```

## Safe path containment

Explicit skill paths are canonicalized and validated before loading:

- Relative paths are resolved against `--cwd` and must not escape it through symlinks.
- Paths inside a project `.pi/` directory require project trust.
- Missing paths are a hard error.

When a skill is loaded, `base_dir` is set to the directory containing `SKILL.md`. The skill resolver in [`crates/pi-coding/src/selector.rs`](../../../crates/pi-coding/src/selector.rs) (`resolve_skill_uri`) restricts `skill://<name>/<relative-path>` reads to that base directory:

- `..`, absolute paths, `?`, `#`, and backslashes are rejected.
- The resolved path is canonicalized and must be inside `base_dir`.
- The declared `file_path` itself must also be inside `base_dir`.
- Only trusted skills can be resolved.

## Selection and resolver

The deterministic selector in [`crates/pi-coding/src/selector.rs`](../../../crates/pi-coding/src/selector.rs) ranks skills for each request. Hidden skills and skills with `disable-model-invocation: true` are excluded from ranking. The score is built from:

| Signal | Points | Notes |
|--------|--------|-------|
| Exact name match | 1,600 | Request tokens equal skill name phrase. |
| Name phrase contained in request | 1,000 | e.g. request contains `"rust review"`. |
| Name token overlap | 220 each | Stopwords are ignored. |
| Description token overlap | 60 each | Stopwords are ignored. |
| Description phrase (≥ 2 tokens) | 90 per token | Longest shared phrase. |
| `alwaysApply: true` | +2,000 | Also makes the skill autoload. |
| Matching `globs` path | +800 per matched pattern | Patterns are `globset` globs matched against request paths. |
| Source precedence | +precedence | `User` 50, `Project` 40, `PackageGlobal` 30, `PackageProject` 20, `Explicit` 10. |

Only skills scoring at least `min_score` (default 60) are returned, up to `max_results` (default 5).

An optional classifier can re-rank the deterministic candidates when `selector.classifier.enabled: true` in settings. The classifier is never allowed to invent new skill names; it only reorders candidates already produced by the deterministic pass.

Selection recommendations are rendered into the system prompt as `<selection_recommendations>`. They augment model judgment; they do not replace it. The system prompt explicitly tells the model:

> Use the `read` tool with `skill://<name>` to load a skill when the task matches its description. When a skill file references a relative path, read it as `skill://<name>/<relative-path>`; the resolver confines access to that skill's base directory.

### Autoload skills

Some skills are loaded automatically:

- Skills with `alwaysApply: true` (if trusted and not disabled).
- High-scoring skills that also have `alwaysApply: true`.
- Skills named in a selected agent's `autoloadSkills` frontmatter.

Autoloaded skill bodies are read from disk and inserted into the prompt context.

## Usage in the prompt

The `<available_skills>` block is appended to the system prompt **only when the `read` tool is selected**. It contains every visible, trusted skill:

```xml
<available_skills>
  <skill>
    <name>rust-review</name>
    <description>Review Rust code for idioms and safety.</description>
    <location>skill://rust-review</location>
  </skill>
</available_skills>
```

Untrusted, hidden, and disabled skills are excluded. The model decides when to load a skill; deterministic recommendations are advisory.

## Interactive skill commands

When `enable_skill_commands` is `true` (the default), the REPL and TUI register a `/skill:<name>` command for each visible skill. Typing:

```
/skill:rust-review review src/lib.rs
```

expands the skill body into the user prompt. This is a convenience for manually loading a skill; the model can still load any skill via `read` on its own.

## Agents

Agents are defined in `<agent-dir>/agents/*.md` and, when trusted, `<workspace>/.pi/agents/*.md`. They look similar to skills but are used for a different purpose:

| | Skill | Agent |
|--|-------|-------|
| Location | `skills/` | `agents/` |
| Frontmatter | `name`, `description`, `globs`, `alwaysApply`, `disable-model-invocation` | `name`, `description`, `tools`, `autoloadSkills`, `model`, `thinkingLevel` |
| Use | Loaded by the current session via `skill://` | Spawned as a subagent by the orchestration runtime |
| System prompt | Listed in `<available_skills>` | Becomes the child agent's full system prompt |

The bundled `task` agent is always available. See [`crates/pi-coding/src/orchestration/definitions.rs`](../../../crates/pi-coding/src/orchestration/definitions.rs) for the agent definition format.

## SDK behavior

The `AgentSessionBuilder` in [`crates/pi-coding/src/sdk.rs`](../../../crates/pi-coding/src/sdk.rs) defaults to `ResourceDiscovery::Disabled`, so programmatic sessions do **not** auto-discover skills. To opt in, call `discover_resources(ResourceManagerOptions)`; project-local skills still require trust.

```rust
use pi_ai::Model;
use pi_coding::{AgentSessionBuilder, AgentSession, ResourceManagerOptions};

async fn discovery_session(model: Model) -> Result<AgentSession, Box<dyn std::error::Error>> {
    let session = AgentSessionBuilder::new(model, "<workspace>")
        .discover_resources(ResourceManagerOptions::new("<workspace>", "<agent-dir>"))?
        .build()
        .await?;
    Ok(session)
}
```

This makes skill discovery explicit and fail-closed: no project skills are loaded unless the caller opts in and the project is trusted.

## Example: create and use a skill

Create a project skill:

```sh
mkdir -p .pi/skills/rust-review
cat > .pi/skills/rust-review/SKILL.md <<'EOF'
---
name: rust-review
description: Review Rust code for safety, idioms, and clarity.
globs:
  - "*.rs"
---

# Rust review

Check for unnecessary clones, panic paths, and async cancellation safety.
EOF
```

Run with project trust:

```sh
rpi --approve --print "Review src/lib.rs using the rust-review skill"
```

Or in an interactive session type:

```
/skill:rust-review review src/lib.rs
```

The resolver ensures that any relative path inside the skill (for example a referenced checklist file) is read from `.pi/skills/rust-review/`, never from outside the project.

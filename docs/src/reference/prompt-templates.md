# Prompt templates and system prompt assembly

Prompt templates expand operator arguments into file-backed text. System prompt assembly combines host instructions, selected tool guidance, project context, skill descriptions, and workspace information. Neither mechanism grants tool authority.

## Templates

Templates are Markdown files discovered under the active agent directory's `prompts` directory and admitted project `.rpi/prompts`, plus explicit `--prompt-template` files or directories. `--no-prompt-templates` disables configured/discovered templates but preserves explicit selections subject to project admission.

A template records its name, description, optional argument hint, body, file path, and global/project/explicit scope. Frontmatter supplies description and argument metadata. Without a description, the first nonblank body line supplies a bounded preview.

Expansion supports positional arguments (`$1`), all arguments (`$ARGUMENTS` or `$@`), default forms such as `${1:-value}`, and argument-slice forms. Substitution runs once: inserted argument text containing `$1` or `$@` remains literal rather than recursively expanding.

Use quoting in the template invocation when one argument contains spaces. Template expansion is text substitution, not shell evaluation.

## System prompt

`--system-prompt TEXT_OR_PATH` supplies a custom prompt. `--append-system-prompt TEXT_OR_PATH` adds instructions and can be repeated. A custom prompt still receives appended instructions, admitted context, skill descriptions when `read` is available, workspace information, and the working-directory footer.

Without a custom prompt, the builder describes selected tools using their snippets and deduplicates guidelines in order. It adds admitted context and resource information. Tool filtering and prompt assembly must agree; a described path or tool name is not executable authority.

The current generated guidance prefers the dedicated Grep tool when it is available alongside bash. This is an instruction preference, not a bash parser prohibition: the bash tool itself does not block shell `grep` or `rg`. Unsupervised background/detached shell work is still rejected.

## Context boundaries

Project `AGENTS.md` and `CLAUDE.md` candidates are bounded UTF-8 resources. Project context files cannot follow symlinks, even in a trusted project. Managed global resource links use their separate capability policy. Invalid project context produces a diagnostic rather than silently reading a symlink target.

## Safety

Keep secrets out of templates and context files. Environment-expanded settings and prompt text can expose data to the selected model; a resource's successful discovery is not a privacy guarantee. See [settings/trust](settings-trust.md) and [skills](skills.md).

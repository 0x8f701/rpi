//! System prompt assembly. Port of `coding/systemprompt.go`.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::resources::{format_skills_for_prompt, Skill};

/// A project context file injected into the system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

/// Configures `build_system_prompt`.
#[derive(Debug, Clone, Default)]
pub struct BuildSystemPromptOptions {
    pub custom_prompt: String,
    pub selected_tools: Vec<String>,
    pub tool_snippets: HashMap<String, String>,
    pub prompt_guidelines: Vec<String>,
    pub append_system_prompt: String,
    pub cwd: String,
    /// Canonical workspace roots after the primary cwd, in user-selected order.
    pub additional_workspace_roots: Vec<String>,
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
    /// Absolute rpi documentation paths; empty values fall back to
    /// `readme_path()`/`docs_path()`/`examples_path()`.
    pub readme_path: String,
    pub docs_path: String,
    pub examples_path: String,
}

/// Constructs the coding agent system prompt, including tools, guidelines,
/// project context, and footer. Port of `buildSystemPrompt`.
pub fn build_system_prompt(opts: BuildSystemPromptOptions) -> String {
    let prompt_cwd = opts.cwd.replace('\\', "/");
    let workspace_section = build_workspace_section(&opts.additional_workspace_roots);

    let append_section = if !opts.append_system_prompt.is_empty() {
        format!("\n\n{}", opts.append_system_prompt)
    } else {
        String::new()
    };

    let context_section = build_context_section(&opts.context_files);

    let tools = if opts.selected_tools.is_empty() {
        vec!["read".to_string(), "bash".to_string(), "edit".to_string(), "write".to_string()]
    } else {
        opts.selected_tools.clone()
    };
    let has_read = tools.iter().any(|t| t == "read");
    let skills_section = if has_read {
        format_skills_for_prompt(&opts.skills)
    } else {
        String::new()
    };

    if !opts.custom_prompt.is_empty() {
        let mut prompt = format!(
            "{}{append_section}{context_section}{skills_section}{workspace_section}",
            opts.custom_prompt
        );
        if !prompt.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("\nCurrent working directory: ");
        prompt.push_str(&prompt_cwd);
        return prompt;
    }

    let mut visible: Vec<String> = Vec::new();
    for name in &tools {
        if let Some(snippet) = opts.tool_snippets.get(name) {
            if !snippet.is_empty() {
                visible.push(format!("- {name}: {snippet}"));
            }
        }
    }
    let tools_list = if visible.is_empty() { "(none)".to_string() } else { visible.join("\n") };

    // Guidelines: dedup preserving order, with the bash-for-file-ops guideline
    // added when bash is present without grep/find/ls.
    let mut guidelines: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut add = |g: String, guidelines: &mut Vec<String>, seen: &mut Vec<String>| {
        if g.is_empty() || seen.contains(&g) {
            return;
        }
        seen.push(g.clone());
        guidelines.push(g);
    };
    let has = |name: &str| tools.iter().any(|t| t == name);
    if has("bash") && !has("grep") && !has("find") && !has("ls") {
        add("Use bash for file operations like ls, rg, find".to_string(), &mut guidelines, &mut seen);
    }
    for g in &opts.prompt_guidelines {
        add(g.trim().to_string(), &mut guidelines, &mut seen);
    }
    add("Be concise in your responses".to_string(), &mut guidelines, &mut seen);
    add("Show file paths clearly when working with files".to_string(), &mut guidelines, &mut seen);

    let mut gb = String::new();
    for (i, g) in guidelines.iter().enumerate() {
        if i > 0 {
            gb.push('\n');
        }
        let _ = write!(gb, "- {g}");
    }

    let readme_path = if opts.readme_path.is_empty() {
        crate::resources::readme_path()
    } else {
        opts.readme_path.clone()
    };
    let docs_path = if opts.docs_path.is_empty() {
        crate::resources::docs_path()
    } else {
        opts.docs_path.clone()
    };
    let examples_path = if opts.examples_path.is_empty() {
        crate::resources::examples_path()
    } else {
        opts.examples_path.clone()
    };

    let prompt = format!(
        "You are an expert coding assistant operating inside rpi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\
\n\
Available tools:\n\
{tools_list}\n\
\n\
In addition to the tools above, you may have access to other custom tools depending on the project.\n\
\n\
Guidelines:\n\
{gb}\n\
\n\
rpi documentation (read only when the user asks about rpi itself, its SDK, extensions, themes, skills, or TUI):\n\
- Main documentation: {readme_path}\n\
- Additional docs: {docs_path}\n\
- Examples: {examples_path} (extensions, custom tools, SDK)\n\
- When reading rpi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory\n\
- When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), rpi packages (docs/packages.md), environment variables (docs/environment-variables.md)\n\
- When working on rpi topics, read the docs and examples, and follow .md cross-references before implementing\n\
- Always read rpi .md files completely and follow links to related docs (e.g., tui.md for TUI API details)",
    );

    let mut prompt = format!(
        "{prompt}{append_section}{context_section}{skills_section}{workspace_section}"
    );
    prompt.push_str("\nCurrent working directory: ");
    prompt.push_str(&prompt_cwd);
    prompt
}

fn build_workspace_section(additional_roots: &[String]) -> String {
    const MAX_ROOTS: usize = 64;
    const MAX_PATH_CHARS: usize = 4_096;

    if additional_roots.is_empty() {
        return String::new();
    }
    let mut section = String::from(
        "\n\n<workspace_roots>\nThese additional roots extend scoped ls/find/grep/glob tools and @file expansion beyond the current working directory. They do not restrict normal coding-tool read, write, or edit paths, which may use other filesystem locations:\n",
    );
    for root in additional_roots.iter().take(MAX_ROOTS) {
        let normalized = root.replace('\\', "/");
        let bounded = normalized.chars().take(MAX_PATH_CHARS).collect::<String>();
        let _ = writeln!(section, "- {bounded}");
    }
    if additional_roots.len() > MAX_ROOTS {
        let _ = writeln!(
            section,
            "- ... ({} additional roots omitted)",
            additional_roots.len() - MAX_ROOTS
        );
    }
    section.push_str("</workspace_roots>");
    section
}

fn build_context_section(context_files: &[ContextFile]) -> String {
    if context_files.is_empty() {
        return String::new();
    }
    let mut b = String::new();
    b.push_str("\n\n<project_context>\n\n");
    b.push_str("Project-specific instructions and guidelines:\n\n");
    for cf in context_files {
        // pi interpolates the raw path into the attribute — no quoting/escaping.
        let _ = write!(b, "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n", cf.path, cf.content);
    }
    b.push_str("</project_context>\n");
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::Skill;

    fn snippet(name: &str, desc: &str) -> (String, String) {
        (name.to_string(), desc.to_string())
    }

    fn base_opts() -> BuildSystemPromptOptions {
        let mut tool_snippets = HashMap::new();
        tool_snippets.extend([
            snippet("read", "Read file contents"),
            snippet("bash", "Execute finite foreground shell commands, or supervised long-running commands with background=true"),
            snippet("edit", "Make precise file edits with exact text replacement, including multiple disjoint edits in one call"),
            snippet("write", "Create or overwrite files"),
        ]);
        BuildSystemPromptOptions {
            selected_tools: vec!["read", "bash", "edit", "write"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            tool_snippets,
            cwd: "/proj".to_string(),
            readme_path: "/pkg/README.md".to_string(),
            docs_path: "/pkg/docs".to_string(),
            examples_path: "/pkg/examples".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn default_prompt_golden_shape() {
        let opts = base_opts();
        let got = build_system_prompt(opts);
        assert!(got.starts_with("You are an expert coding assistant operating inside rpi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\nAvailable tools:\n- read: Read file contents\n- bash: Execute finite foreground shell commands, or supervised long-running commands with background=true\n- edit: Make precise file edits with exact text replacement, including multiple disjoint edits in one call\n- write: Create or overwrite files\n\nIn addition to the tools above, you may have access to other custom tools depending on the project.\n\nGuidelines:\n"));
        assert!(got.contains("rpi documentation (read only when the user asks about rpi itself, its SDK, extensions, themes, skills, or TUI):"));
        assert!(got.contains("- Main documentation: /pkg/README.md"));
        assert!(got.contains("- Additional docs: /pkg/docs"));
        assert!(got.contains("- Examples: /pkg/examples (extensions, custom tools, SDK)"));
        assert!(got.contains("- Use bash for file operations like ls, rg, find"));
        assert!(got.contains("- Be concise in your responses"));
        assert!(got.contains("- Show file paths clearly when working with files"));
        assert!(got.ends_with("Current working directory: /proj"));
        assert!(!got.contains("<available_skills>"));
        assert!(!got.contains("<project_context>"));
    }

    #[test]
    fn additional_workspace_roots_are_bounded_and_absent_by_default() {
        let default_prompt = build_system_prompt(base_opts());
        assert!(!default_prompt.contains("<workspace_roots>"));

        let mut opts = base_opts();
        opts.additional_workspace_roots = vec![
            "/tmp/agent-loader".to_owned(),
            "x".repeat(4_106),
        ];
        let prompt = build_system_prompt(opts);
        assert!(prompt.contains("<workspace_roots>"));
        assert!(prompt.contains("They do not restrict normal coding-tool read, write, or edit paths"));
        assert!(prompt.contains("- /tmp/agent-loader"));
        assert!(!prompt.contains(&"x".repeat(4_097)));
    }

    #[test]
    fn custom_prompt_assembly_golden() {
        let opts = BuildSystemPromptOptions {
            custom_prompt: "You are a custom agent.".to_string(),
            append_system_prompt: "Appended instructions.".to_string(),
            selected_tools: vec!["read".to_string(), "bash".to_string()],
            cwd: "/proj".to_string(),
            context_files: vec![ContextFile {
                path: "/proj/AGENTS.md".to_string(),
                content: "follow the rules".to_string(),
            }],
            skills: vec![Skill {
                name: "demo".to_string(),
                description: "d".to_string(),
                file_path: "/proj/.pi/skills/demo/SKILL.md".to_string(),
                base_dir: String::new(),
                globs: Vec::new(),
                always_apply: false,
                hidden: false,
                disable_model_invocation: false,
                source: crate::SkillSource::User,
                trusted: true,
            }],
            ..Default::default()
        };
        let got = build_system_prompt(opts);
        let want = "You are a custom agent.\n\nAppended instructions.\n\n<project_context>\n\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"/proj/AGENTS.md\">\nfollow the rules\n</project_instructions>\n\n</project_context>\n\n\nThe following skills provide specialized instructions for specific tasks.\nUse the read tool with skill://<name> to load a skill when the task matches its description. Skill choice remains model prompt policy; deterministic recommendations may transparently augment this list.\nWhen a skill file references a relative path, read it as skill://<name>/<relative-path>; the resolver confines access to that skill's base directory.\n\n<available_skills>\n  <skill>\n    <name>demo</name>\n    <description>d</description>\n    <location>skill://demo</location>\n  </skill>\n</available_skills>\n\nCurrent working directory: /proj";
        assert_eq!(got, want, "custom assembly drift:\n--- got ---\n{got}\n--- want ---\n{want}");
    }

    #[test]
    fn includes_context_and_skills() {
        let mut tool_snippets = HashMap::new();
        tool_snippets.insert("read".to_string(), "Read file contents".to_string());
        tool_snippets.insert("bash".to_string(), "Execute bash commands".to_string());
        let p = build_system_prompt(BuildSystemPromptOptions {
            selected_tools: vec!["read".to_string(), "bash".to_string()],
            tool_snippets,
            cwd: "/proj".to_string(),
            context_files: vec![ContextFile {
                path: "/proj/AGENTS.md".to_string(),
                content: "follow the rules".to_string(),
            }],
            skills: vec![Skill {
                name: "demo".to_string(),
                description: "d".to_string(),
                file_path: "/proj/.pi/skills/demo/SKILL.md".to_string(),
                base_dir: String::new(),
                globs: Vec::new(),
                always_apply: false,
                hidden: false,
                disable_model_invocation: false,
                source: crate::SkillSource::User,
                trusted: true,
            }],
            ..Default::default()
        });
        assert!(p.contains("<project_instructions path=\"/proj/AGENTS.md\">"), "missing context: {p}");
        assert!(p.contains("<name>demo</name>"), "missing skills: {p}");
    }

    #[test]
    fn skills_excluded_without_read_tool() {
        let mut tool_snippets = HashMap::new();
        tool_snippets.insert("bash".to_string(), "Execute bash commands".to_string());
        let p = build_system_prompt(BuildSystemPromptOptions {
            selected_tools: vec!["bash".to_string()],
            tool_snippets,
            cwd: "/proj".to_string(),
            skills: vec![Skill {
                name: "demo".to_string(),
                description: "d".to_string(),
                file_path: "x".to_string(),
                base_dir: String::new(),
                globs: Vec::new(),
                always_apply: false,
                hidden: false,
                disable_model_invocation: false,
                source: crate::SkillSource::User,
                trusted: true,
            }],
            ..Default::default()
        });
        assert!(!p.contains("available_skills"), "skills excluded without read tool: {p}");
    }

    #[test]
    fn bash_guideline_suppressed_when_ls_present() {
        let mut tool_snippets = HashMap::new();
        tool_snippets.insert("bash".to_string(), "x".to_string());
        tool_snippets.insert("ls".to_string(), "x".to_string());
        let p = build_system_prompt(BuildSystemPromptOptions {
            selected_tools: vec!["bash".to_string(), "ls".to_string()],
            tool_snippets,
            cwd: "/proj".to_string(),
            ..Default::default()
        });
        assert!(!p.contains("Use bash for file operations like ls, rg, find"));
    }
}
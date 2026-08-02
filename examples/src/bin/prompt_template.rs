//! Build a coding-agent system prompt from a custom template, context files, and skills.
//!
//! Run from the repo root:
//!
//! ```sh
//! cargo run --bin prompt_template
//! ```

use pi_coding::{build_system_prompt, BuildSystemPromptOptions, ContextFile, Skill, SkillSource};
use std::collections::HashMap;

fn main() {
    let mut tool_snippets = HashMap::new();
    tool_snippets.insert("read".to_owned(), "Read file contents".to_owned());
    tool_snippets.insert("bash".to_owned(), "Execute shell commands".to_owned());

    let prompt = build_system_prompt(BuildSystemPromptOptions {
        custom_prompt: "You are a Rust code reviewer.".to_owned(),
        append_system_prompt: "Prefer small, focused functions.".to_owned(),
        selected_tools: vec!["read".to_owned(), "bash".to_owned()],
        tool_snippets,
        prompt_guidelines: vec![
            "Explain the *why*, not just the *what*.".to_owned(),
        ],
        cwd: "<workspace>".to_owned(),
        additional_workspace_roots: Vec::new(),
        context_files: vec![ContextFile {
            path: "AGENTS.md".to_owned(),
            content: "Be concise; cite file paths.".to_owned(),
        }],
        skills: vec![Skill {
            name: "rust-review".to_owned(),
            description: "Review Rust code for idioms and safety.".to_owned(),
            file_path: "<workspace>/.pi/skills/rust-review/SKILL.md".to_owned(),
            base_dir: "<workspace>/.pi/skills/rust-review".to_owned(),
            globs: Vec::new(),
            always_apply: false,
            hidden: false,
            disable_model_invocation: false,
            source: SkillSource::Explicit,
            trusted: true,
        }],
        readme_path: "<pkg>/README.md".to_owned(),
        docs_path: "<pkg>/docs".to_owned(),
        examples_path: "<pkg>/examples".to_owned(),
    });

    println!("{}", prompt);
}

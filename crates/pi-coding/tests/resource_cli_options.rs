use anyhow::Result;
use pi_coding::{ResourceManager, ResourceManagerOptions};

#[test]
fn explicit_project_system_prompt_is_trust_gated_before_read() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let agent_dir = tempfile::tempdir()?;
    let project_dir = cwd.path().join(".pi");
    std::fs::create_dir_all(&project_dir)?;
    let prompt = project_dir.join("system.txt");
    std::fs::write(&prompt, "trusted prompt")?;

    let mut denied = ResourceManagerOptions::new(cwd.path());
    denied.agent_dir = agent_dir.path().to_path_buf();
    denied.headless = true;
    denied.system_prompt = Some(String::new());
    denied.system_prompt_path = Some(prompt.clone());
    let error = ResourceManager::new(denied)
        .err()
        .expect("untrusted project prompt must fail");
    assert!(error.to_string().contains("requires project trust"));

    let mut allowed = ResourceManagerOptions::new(cwd.path());
    allowed.agent_dir = agent_dir.path().to_path_buf();
    allowed.headless = true;
    allowed.project_trust_override = Some(true);
    allowed.system_prompt = Some(String::new());
    allowed.system_prompt_path = Some(prompt);
    let manager = ResourceManager::new(allowed)?;
    assert_eq!(manager.snapshot().system_prompt.as_deref(), Some("trusted prompt"));
    Ok(())
}

#[test]
fn append_system_prompt_preserves_literal_and_file_order() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let agent_dir = tempfile::tempdir()?;
    let prompt = cwd.path().join("append.txt");
    std::fs::write(&prompt, "from file")?;

    let mut options = ResourceManagerOptions::new(cwd.path());
    options.agent_dir = agent_dir.path().to_path_buf();
    options.headless = true;
    options.project_trust_override = Some(true);
    options.append_system_prompt = vec!["literal".to_owned(), String::new()];
    options.append_system_prompt_paths = vec![None, Some(prompt)];
    let manager = ResourceManager::new(options)?;
    assert_eq!(manager.snapshot().append_system_prompt, ["literal", "from file"]);
    Ok(())
}

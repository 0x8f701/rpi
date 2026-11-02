use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, bail};
use pi_agent::{AgentTool, AgentToolResult, ToolCapability};
use pi_ai::Schema;
use serde_json::Value;

use crate::fs::{
    CapabilityAccess, CapabilityEntry, CapabilityRootLease, CapabilityRootPurpose, FileMode,
    FsError, RootCreation,
};

const SKILLS_DIR: &str = "skills";
const SKILL_FILE: &str = "SKILL.md";
const MAX_NAME: usize = 64;
const MAX_DESCRIPTION_UTF16: usize = 1024;
const MAX_FILE: u64 = 256 * 1024;
const MAX_COUNT: usize = 32;
const MAX_AGGREGATE: usize = 1024 * 1024;

// Count and aggregate checks are serialized within one process. Cross-process
// same-name creation still fails safely because both the directory and file
// are created exclusively.
static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct PersonaSkill {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) content: String,
}

#[derive(Clone)]
#[doc(hidden)]
pub struct PersonaSkillStore {
    persona: CapabilityRootLease,
}

impl std::fmt::Debug for PersonaSkillStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PersonaSkillStore { .. }")
    }
}

impl PersonaSkillStore {
    pub(crate) fn new(persona: CapabilityRootLease) -> Result<Self> {
        validate_lease(&persona)?;
        Ok(Self { persona })
    }

    pub(crate) fn learn(
        &self,
        name: &str,
        description: &str,
        instructions: &str,
    ) -> Result<PersonaSkill> {
        validate_name(name)?;
        validate_description(description)?;
        validate_instructions(instructions)?;
        let content = serialize(name, description, instructions)?;
        if content.len() as u64 > MAX_FILE {
            bail!("persona skill file exceeds {MAX_FILE} bytes");
        }

        let _guard = lock()?;
        let current = load_unlocked(&self.persona)?;
        if current.iter().any(|skill| skill.name == name) {
            bail!("persona skill {name:?} already exists");
        }
        if current.len() >= MAX_COUNT {
            bail!("persona skill count exceeds limit of {MAX_COUNT}");
        }
        let total = current
            .iter()
            .try_fold(content.len(), |sum, skill| {
                sum.checked_add(skill.content.len())
            })
            .ok_or_else(|| anyhow!("persona skill aggregate size overflow"))?;
        if total > MAX_AGGREGATE {
            bail!("persona skills exceed aggregate limit of {MAX_AGGREGATE} bytes");
        }

        let skills = ensure_skills_directory(&self.persona)?;
        let skill_dir = skills
            .create_directory_exclusive(Path::new(name), CapabilityAccess::PrivateReadWrite)
            .with_context(|| format!("creating persona skill {name:?}"))?;
        let created = (|| -> Result<()> {
            let mut file = skill_dir.create_new(Path::new(SKILL_FILE), FileMode::Private)?;
            file.write_all(content.as_bytes())?;
            let _ = file.finish()?;
            Ok(())
        })();
        if let Err(error) = created {
            // CapabilityWritableFile removes an unfinished inode on drop. This
            // lease owns the exclusively created directory, so cleanup cannot
            // delete preexisting or concurrently published skill bytes.
            let cleanup = skills.remove_directory(Path::new(name));
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(error.context(format!(
                    "failed to clean incomplete persona skill: {cleanup}"
                ))),
            };
        }
        parse(&content, name)
    }

    pub(crate) fn load_persona_skills(&self) -> Result<Vec<PersonaSkill>> {
        load_persona_skills(&self.persona)
    }

    pub(crate) fn resolve_persona_skill(&self, name: &str, requested: &str) -> Result<String> {
        resolve_persona_skill(&self.persona, name, requested)
    }

    pub(crate) fn learn_skill_tool(&self) -> AgentTool {
        learn_skill_tool(self.persona.clone())
    }
}

fn ensure_skills_directory(persona: &CapabilityRootLease) -> Result<CapabilityRootLease> {
    match persona.lease_directory(
        Path::new(SKILLS_DIR),
        CapabilityAccess::PrivateReadWrite,
        RootCreation::Existing,
    ) {
        Ok(lease) => Ok(lease),
        Err(error) if not_found(&error) => persona
            .create_new_directory(Path::new(SKILLS_DIR), CapabilityAccess::PrivateReadWrite)
            .or_else(|create_error| {
                // A concurrent process may have created the shared skills
                // container. Reopen it strictly; links and non-directories fail.
                match persona.lease_directory(
                    Path::new(SKILLS_DIR),
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::Existing,
                ) {
                    Ok(lease) => Ok(lease),
                    Err(_) => Err(create_error),
                }
            }),
        Err(error) => Err(error),
    }
    .context("opening persona skills directory")
}

pub(crate) fn load_persona_skills(persona: &CapabilityRootLease) -> Result<Vec<PersonaSkill>> {
    validate_lease(persona)?;
    let _guard = lock()?;
    load_unlocked(persona)
}

fn load_unlocked(persona: &CapabilityRootLease) -> Result<Vec<PersonaSkill>> {
    let skills = match persona.lease_directory(
        Path::new(SKILLS_DIR),
        CapabilityAccess::ReadOnly,
        RootCreation::Existing,
    ) {
        Ok(lease) => lease,
        Err(error) if not_found(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error).context("opening persona skills directory"),
    };
    let names = skills.read_dir_names_strict(Path::new(""))?;
    if names.len() > MAX_COUNT {
        bail!("persona skill count exceeds limit of {MAX_COUNT}");
    }

    let mut result = Vec::with_capacity(names.len());
    let mut total = 0usize;
    for raw_name in names {
        let name = raw_name
            .to_str()
            .ok_or_else(|| anyhow!("persona skill name is not UTF-8"))?;
        validate_name(name)?;
        match skills.open_entry_exact(Path::new(name))? {
            CapabilityEntry::Directory(_) => {}
            CapabilityEntry::File(_) => bail!("persona skill entry {name:?} is not a directory"),
        }
        let skill_dir = skills.lease_directory(
            Path::new(name),
            CapabilityAccess::ReadOnly,
            RootCreation::Existing,
        )?;
        let children = skill_dir.read_dir_names_strict(Path::new(""))?;
        if children.len() != 1 || children[0] != SKILL_FILE {
            bail!("persona skill directory {name:?} must contain only {SKILL_FILE}");
        }
        let file = match skill_dir.open_entry_exact(Path::new(SKILL_FILE))? {
            CapabilityEntry::File(file) => file,
            CapabilityEntry::Directory(_) => bail!("persona skill file {name:?} is not regular"),
        };
        let bytes = file.read_bounded(MAX_FILE)?;
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| anyhow!("persona skill aggregate size overflow"))?;
        if total > MAX_AGGREGATE {
            bail!("persona skills exceed aggregate limit of {MAX_AGGREGATE} bytes");
        }
        let content = String::from_utf8(bytes)
            .with_context(|| format!("persona skill {name:?} is not UTF-8"))?;
        result.push(parse(&content, name)?);
    }
    result.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(result)
}

pub(crate) fn resolve_persona_skill(
    persona: &CapabilityRootLease,
    name: &str,
    requested: &str,
) -> Result<String> {
    validate_name(name)?;
    if !requested.is_empty() {
        bail!("persona skills expose only their main {SKILL_FILE}");
    }
    load_persona_skills(persona)?
        .into_iter()
        .find(|skill| skill.name == name)
        .map(|skill| skill.content)
        .ok_or_else(|| anyhow!("persona skill {name:?} was not found"))
}

pub(crate) fn learn_skill_tool(persona: CapabilityRootLease) -> AgentTool {
    let parameters = Schema {
        additional_properties: Some(Value::Bool(false)),
        ..Schema::object_ordered(vec![
            (
                "name".into(),
                Schema {
                    min_length: Some(1),
                    max_length: Some(MAX_NAME),
                    pattern: Some("^[a-z0-9]+(?:-[a-z0-9]+)*$".into()),
                    ..Schema::string()
                },
                true,
            ),
            (
                "description".into(),
                Schema {
                    min_length: Some(1),
                    ..Schema::string()
                },
                true,
            ),
            (
                "instructions".into(),
                Schema {
                    min_length: Some(1),
                    ..Schema::string()
                },
                true,
            ),
        ])
    };
    AgentTool::new(
        "learn_skill",
        "Create a durable persona-local skill. It becomes available on the next spawn or revival.",
        parameters,
        move |context| {
            let persona = persona.clone();
            async move {
                crate::tools::check_aborted(&context.abort)?;
                let skill = PersonaSkillStore::new(persona)?.learn(
                    arg(&context.arguments, "name")?,
                    arg(&context.arguments, "description")?,
                    arg(&context.arguments, "instructions")?,
                )?;
                Ok(AgentToolResult::text(format!(
                    "Learned persona skill `{}`. It will be available beginning with the next spawn or revival.",
                    skill.name
                )))
            }
        },
    )
    .with_capability(ToolCapability::Write)
}

fn arg<'a>(arguments: &'a Value, name: &str) -> Result<&'a str> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{name} is required and must be a string"))
}

fn validate_lease(lease: &CapabilityRootLease) -> Result<()> {
    if lease.purpose() != CapabilityRootPurpose::PersonaStore
        || lease.access() != CapabilityAccess::PrivateReadWrite
    {
        bail!("persona skills require a private persona-store lease");
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("persona skill name is required");
    }
    if name.len() > MAX_NAME {
        bail!("persona skill name exceeds {MAX_NAME} characters");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("persona skill name contains invalid characters");
    }
    if name.starts_with('-') || name.ends_with('-') {
        bail!("persona skill name must not start or end with a hyphen");
    }
    if name.contains("--") {
        bail!("persona skill name must not contain consecutive hyphens");
    }
    Ok(())
}

fn validate_description(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("persona skill description is required");
    }
    if value.encode_utf16().count() > MAX_DESCRIPTION_UTF16 {
        bail!("persona skill description exceeds {MAX_DESCRIPTION_UTF16} UTF-16 code units");
    }
    if value.chars().any(char::is_control) {
        bail!("persona skill description contains control characters");
    }
    if crate::redact::redact_secrets(value) != value {
        bail!("persona skill description contains secret-shaped material");
    }
    Ok(())
}
fn validate_instructions(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("persona skill instructions are required");
    }
    if value.chars().any(|character| {
        character == '\r' || (character.is_control() && character != '\n' && character != '\t')
    }) {
        bail!("persona skill instructions contain unsupported control characters");
    }
    if crate::redact::redact_secrets(value) != value {
        bail!("persona skill instructions contain secret-shaped material");
    }
    Ok(())
}

fn serialize(name: &str, description: &str, instructions: &str) -> Result<String> {
    Ok(format!(
        "---\nname: {}\ndescription: {}\n---\n\n{}\n",
        serde_json::to_string(name)?,
        serde_json::to_string(description)?,
        instructions.trim()
    ))
}

fn parse(content: &str, expected_name: &str) -> Result<PersonaSkill> {
    if content.len() as u64 > MAX_FILE {
        bail!("persona skill file exceeds {MAX_FILE} bytes");
    }
    if content.contains('\r') {
        bail!("persona skill uses noncanonical line endings");
    }
    let rest = content
        .strip_prefix("---\n")
        .ok_or_else(|| anyhow!("missing persona skill frontmatter"))?;
    let (header, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| anyhow!("unterminated persona skill frontmatter"))?;
    let mut fields = BTreeMap::new();
    for line in header.lines() {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("malformed persona skill frontmatter"))?;
        let key = key.trim();
        if !matches!(key, "name" | "description") {
            bail!("unsupported persona skill frontmatter field {key:?}");
        }
        let value = parse_frontmatter_value(value.trim())?;
        if fields.insert(key, value).is_some() {
            bail!("duplicate persona skill frontmatter field {key:?}");
        }
    }
    if fields.len() != 2 {
        bail!("persona skill frontmatter must contain exactly name and description");
    }
    let name = fields
        .remove("name")
        .ok_or_else(|| anyhow!("persona skill frontmatter is missing name"))?;
    let description = fields
        .remove("description")
        .ok_or_else(|| anyhow!("persona skill frontmatter is missing description"))?;
    validate_name(&name)?;
    if name != expected_name {
        bail!("persona skill name does not match directory");
    }
    validate_description(&description)?;
    validate_instructions(body)?;
    Ok(PersonaSkill {
        name,
        description,
        content: content.into(),
    })
}
fn parse_frontmatter_value(value: &str) -> Result<String> {
    if value.starts_with('"') {
        return serde_json::from_str(value)
            .context("persona skill frontmatter contains an invalid quoted string");
    }
    if value.is_empty() || value.chars().any(char::is_control) {
        bail!("persona skill frontmatter value is invalid");
    }
    Ok(value.to_owned())
}

fn lock() -> Result<MutexGuard<'static, ()>> {
    LOCK.lock()
        .map_err(|_| anyhow!("persona skill lock is poisoned"))
}

fn not_found(error: &FsError) -> bool {
    matches!(
        error,
        FsError::System {
            kind: io::ErrorKind::NotFound,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use pi_agent::{AbortController, AgentToolResult, ToolCallContext, ToolUpdateFn};
    use pi_sandbox::AbsolutePath;
    use serde_json::json;

    use super::*;
    use crate::fs::CapabilityFs;

    fn lease(
        path: &Path,
        purpose: CapabilityRootPurpose,
        access: CapabilityAccess,
    ) -> CapabilityRootLease {
        fs::create_dir_all(path).unwrap();
        CapabilityFs::new()
            .unwrap()
            .open_root(
                purpose,
                AbsolutePath::parse(path.to_path_buf()).unwrap(),
                access,
                RootCreation::Existing,
            )
            .unwrap()
    }

    fn persona(path: &Path) -> CapabilityRootLease {
        lease(
            path,
            CapabilityRootPurpose::PersonaStore,
            CapabilityAccess::PrivateReadWrite,
        )
    }

    fn raw(root: &Path, name: &str, body: &str) {
        let directory = root.join(SKILLS_DIR).join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join(SKILL_FILE),
            serialize(name, "description", body).unwrap(),
        )
        .unwrap();
    }

    fn context(arguments: Value) -> ToolCallContext {
        let (controller, abort) = AbortController::new();
        std::mem::forget(controller);
        let update: ToolUpdateFn = Arc::new(|_: AgentToolResult| {});
        ToolCallContext {
            tool_call_id: "test".into(),
            arguments,
            on_update: update,
            abort,
            model: None,
            approval_token: None,
        }
    }

    #[test]
    fn valid_create_and_deterministic_load() {
        let root = tempfile::tempdir().unwrap();
        let store = PersonaSkillStore::new(persona(root.path())).unwrap();
        let skill = store
            .learn("zeta", "A useful summary", "Read evidence.")
            .unwrap();
        assert!(skill.content.contains("Read evidence."));
        raw(root.path(), "alpha", "Start first.");
        let loaded = store.load_persona_skills().unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root.path().join("skills/zeta/SKILL.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn invalid_names_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let store = PersonaSkillStore::new(persona(root.path())).unwrap();
        let long = "a".repeat(65);
        for name in [
            "", "Upper", "a_b", "a b", "-a", "a-", "a--b", ".", "..", "a/b", "a\\b", "é", &long,
        ] {
            assert!(
                store.learn(name, "description", "body").is_err(),
                "{name:?}"
            );
        }
        assert!(!root.path().join(SKILLS_DIR).exists());
    }

    #[test]
    fn invalid_description_and_instructions_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let store = PersonaSkillStore::new(persona(root.path())).unwrap();
        assert!(store.learn("empty-description", " ", "body").is_err());
        assert!(
            store
                .learn("long-description", &"😀".repeat(513), "body")
                .is_err()
        );
        assert!(store.learn("newline-description", "a\nb", "body").is_err());
        assert!(store.learn("empty-body", "description", " \n").is_err());
        let secret = format!("sk-{}", "a".repeat(20));
        assert!(store.learn("secret-body", "description", &secret).is_err());
    }

    #[test]
    fn collision_and_preexisting_directories_preserve_bytes() {
        let root = tempfile::tempdir().unwrap();
        let store = PersonaSkillStore::new(persona(root.path())).unwrap();
        store.learn("stable", "description", "first").unwrap();
        let path = root.path().join("skills/stable/SKILL.md");
        let before = fs::read(&path).unwrap();
        assert!(store.learn("stable", "description", "second").is_err());
        assert_eq!(fs::read(path).unwrap(), before);

        fs::create_dir(root.path().join("skills/empty")).unwrap();
        assert!(store.learn("empty", "description", "body").is_err());
        assert!(root.path().join("skills/empty").is_dir());
    }

    #[test]
    fn exclusive_leaf_create_failure_preserves_published_skill() {
        let root = tempfile::tempdir().unwrap();
        let store = PersonaSkillStore::new(persona(root.path())).unwrap();
        raw(root.path(), "alpha", "winner");
        let path = root.path().join("skills/alpha/SKILL.md");
        let before = fs::read(&path).unwrap();

        let skills = ensure_skills_directory(&store.persona).unwrap();
        let error = skills
            .create_directory_exclusive(Path::new("alpha"), CapabilityAccess::PrivateReadWrite)
            .expect_err("an existing skill leaf must never be claimed exclusively");
        assert!(
            matches!(
                error,
                FsError::System {
                    kind: std::io::ErrorKind::AlreadyExists,
                    ..
                }
            ),
            "expected AlreadyExists, got {error:?}"
        );

        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            store
                .load_persona_skills()
                .unwrap()
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha"]
        );
    }

    #[test]
    fn exclusive_create_failure_never_deletes_a_concurrent_leaf() {
        let root = tempfile::tempdir().unwrap();
        let store = PersonaSkillStore::new(persona(root.path())).unwrap();
        let skills = ensure_skills_directory(&store.persona).unwrap();
        let leaf = root.path().join("skills/alpha");

        // The idempotent mkdir -p claims an existing leaf, and the previous
        // failure cleanup then removed it — deleting a directory this call
        // never created.
        skills
            .create_new_directory(Path::new("alpha"), CapabilityAccess::PrivateReadWrite)
            .expect("mkdir -p claims an existing leaf");
        skills
            .remove_directory(Path::new("alpha"))
            .expect("mkdir -p plus remove deletes it");
        assert!(!leaf.exists());

        std::fs::create_dir(&leaf).unwrap();
        skills
            .create_directory_exclusive(Path::new("alpha"), CapabilityAccess::PrivateReadWrite)
            .expect_err("a leaf created elsewhere must not be claimed");
        assert!(leaf.is_dir());
    }

    #[test]
    fn repeated_learn_keeps_the_first_skill_bytes() {
        let root = tempfile::tempdir().unwrap();
        let store = PersonaSkillStore::new(persona(root.path())).unwrap();
        store.learn("alpha", "description", "first").unwrap();
        let path = root.path().join("skills/alpha/SKILL.md");
        let before = fs::read(&path).unwrap();

        let error = store.learn("alpha", "description", "second").unwrap_err();
        assert!(
            format!("{error:#}").contains("already exists"),
            "unexpected error: {error:#}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn all_symlink_positions_reject_and_cannot_escape() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join(SKILLS_DIR)).unwrap();
        let store = PersonaSkillStore::new(persona(root.path())).unwrap();
        assert!(store.learn("escape", "description", "body").is_err());
        assert!(store.load_persona_skills().is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);

        fs::remove_file(root.path().join(SKILLS_DIR)).unwrap();
        fs::create_dir(root.path().join(SKILLS_DIR)).unwrap();
        symlink(outside.path(), root.path().join("skills/linked")).unwrap();
        assert!(store.load_persona_skills().is_err());

        fs::remove_file(root.path().join("skills/linked")).unwrap();
        fs::create_dir(root.path().join("skills/linked")).unwrap();
        let target = outside.path().join(SKILL_FILE);
        fs::write(&target, serialize("linked", "description", "body").unwrap()).unwrap();
        symlink(&target, root.path().join("skills/linked/SKILL.md")).unwrap();
        assert!(store.load_persona_skills().is_err());
        assert!(target.exists());
    }

    #[test]
    fn count_file_and_aggregate_limits() {
        let count = tempfile::tempdir().unwrap();
        for index in 0..MAX_COUNT {
            raw(count.path(), &format!("skill-{index}"), "body");
        }
        let store = PersonaSkillStore::new(persona(count.path())).unwrap();
        assert_eq!(store.load_persona_skills().unwrap().len(), MAX_COUNT);
        assert!(store.learn("one-more", "description", "body").is_err());
        raw(count.path(), "extra", "body");
        assert!(store.load_persona_skills().is_err());

        let file = tempfile::tempdir().unwrap();
        let directory = file.path().join("skills/huge");
        fs::create_dir_all(&directory).unwrap();

        fs::write(
            directory.join(SKILL_FILE),
            vec![b'x'; MAX_FILE as usize + 1],
        )
        .unwrap();
        assert!(load_persona_skills(&persona(file.path())).is_err());

        let aggregate = tempfile::tempdir().unwrap();
        for index in 0..5 {
            raw(
                aggregate.path(),
                &format!("large-{index}"),
                &"x".repeat(220 * 1024),
            );
        }
        assert!(load_persona_skills(&persona(aggregate.path())).is_err());
    }
    #[test]
    fn serialized_file_limit_is_exact() {
        let root = tempfile::tempdir().unwrap();
        let store = PersonaSkillStore::new(persona(root.path())).unwrap();
        let empty = serialize("exact-size", "description", "x").unwrap().len() - 1;
        let body = "x".repeat(MAX_FILE as usize - empty);
        let learned = store
            .learn("exact-size", "description", &body)
            .expect("exactly 256 KiB is allowed");
        assert_eq!(learned.content.len(), MAX_FILE as usize);
        assert!(
            store
                .learn("too-large", "description", &"x".repeat(MAX_FILE as usize))
                .is_err()
        );
    }

    #[test]
    fn wrong_purpose_and_access_reject() {
        let wrong = tempfile::tempdir().unwrap();
        assert!(
            PersonaSkillStore::new(lease(
                wrong.path(),
                CapabilityRootPurpose::AgentStore,
                CapabilityAccess::PrivateReadWrite,
            ))
            .is_err()
        );
        let read_only = tempfile::tempdir().unwrap();
        assert!(
            PersonaSkillStore::new(lease(
                read_only.path(),
                CapabilityRootPurpose::PersonaStore,
                CapabilityAccess::ReadOnly,
            ))
            .is_err()
        );
    }

    #[test]
    fn exact_resolver_rejects_auxiliary_and_traversal() {
        let root = tempfile::tempdir().unwrap();
        let store = PersonaSkillStore::new(persona(root.path())).unwrap();
        let skill = store.learn("exact", "description", "body").unwrap();
        assert_eq!(
            store.resolve_persona_skill("exact", "").unwrap(),
            skill.content
        );
        for requested in [
            SKILL_FILE,
            "notes.md",
            "../SKILL.md",
            "../../outside",
            "/absolute",
        ] {
            assert!(store.resolve_persona_skill("exact", requested).is_err());
        }
        assert!(store.resolve_persona_skill("../exact", "").is_err());
    }

    #[test]
    fn malformed_entries_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("skills/broken");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(SKILL_FILE), "not frontmatter").unwrap();
        assert!(load_persona_skills(&persona(root.path())).is_err());
    }

    #[tokio::test]
    async fn tool_contract() {
        let root = tempfile::tempdir().unwrap();
        let store = PersonaSkillStore::new(persona(root.path())).unwrap();
        let tool = store.learn_skill_tool();
        assert_eq!(tool.capability, ToolCapability::Write);
        assert_eq!(
            tool.parameters.required,
            ["name", "description", "instructions"]
        );
        let result = (tool.execute)(context(json!({
            "name": "tool-skill",
            "description": "description",
            "instructions": "body"
        })))
        .await
        .unwrap();
        let pi_ai::ContentBlock::Text { text, .. } = &result.content[0] else {
            panic!()
        };
        assert!(text.contains("next spawn or revival"));
    }
}

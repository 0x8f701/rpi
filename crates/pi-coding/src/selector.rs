use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use globset::GlobBuilder;
use pi_agent::{AbortSignal, StreamFn};
use pi_ai::{Context, Message, Model, SimpleStreamOptions, StopReason};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::orchestration::{AgentDefinition, AgentDefinitionSource};
use crate::{Skill, SkillSource};

const DEFAULT_MAX_RESULTS: usize = 5;
const DEFAULT_MIN_SCORE: i32 = 60;
const DEFAULT_AUTOLOAD_THRESHOLD: i32 = 900;
const DEFAULT_AUTO_SELECT_THRESHOLD: i32 = 1_600;
const DEFAULT_CONFIDENCE_MARGIN: i32 = 200;
const MAX_CLASSIFIER_TOKENS: i64 = 256;
const DEFAULT_CLASSIFIER_TIMEOUT_MS: u64 = 4_000;
const MAX_AUTOLOAD_SKILL_BYTES: usize = crate::MAX_RESOURCE_SNAPSHOT_BYTES;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SelectorSettings {
    pub enabled: bool,
    pub max_results: usize,
    pub min_score: i32,
    pub autoload_threshold: i32,
    pub auto_select_threshold: i32,
    pub confidence_margin: i32,
    pub classifier: SelectorClassifierSettings,
    /// Automatic interaction-mode classification for user prompts
    /// (`selector.autoMode`): `off` disables it, `suggest` surfaces a status
    /// hint after a detected code task / long-running goal, `auto` additionally
    /// creates a todo DAG for detected code tasks (bounded by orchestration
    /// being enabled and the todo list being empty).
    pub auto_mode: AutoMode,
}

impl Default for SelectorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_results: DEFAULT_MAX_RESULTS,
            min_score: DEFAULT_MIN_SCORE,
            autoload_threshold: DEFAULT_AUTOLOAD_THRESHOLD,
            auto_select_threshold: DEFAULT_AUTO_SELECT_THRESHOLD,
            confidence_margin: DEFAULT_CONFIDENCE_MARGIN,
            classifier: SelectorClassifierSettings::default(),
            auto_mode: AutoMode::Suggest,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SelectorClassifierSettings {
    pub enabled: bool,
    pub model: Option<String>,
    pub max_tokens: i64,
    pub timeout_ms: u64,
}

impl Default for SelectorClassifierSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            max_tokens: MAX_CLASSIFIER_TOKENS,
            timeout_ms: DEFAULT_CLASSIFIER_TIMEOUT_MS,
        }
    }
}

/// Interaction mode the deterministic auto-mode classifier detects for a user
/// prompt. The classification is advisory: `Question` keeps the normal
/// plain-answer flow, `CodeTask` suggests (or auto-runs) a todo DAG, and
/// `Goal` suggests tracking the request as a long-running goal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptMode {
    /// Code task: implement/build/fix/refactor with code evidence.
    CodeTask,
    /// Plain question answered directly without orchestration.
    #[default]
    Question,
    /// Long-running goal the user wants tracked and driven.
    Goal,
}

impl PromptMode {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CodeTask => "code task",
            Self::Question => "question",
            Self::Goal => "long-running goal",
        }
    }
}

/// Behavior of the automatic interaction-mode classifier
/// (`selector.autoMode`): off, status hint only, or auto-create the todo DAG
/// for detected code tasks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoMode {
    /// Classifier disabled; no hints and no auto todos.
    Off,
    /// Classify and show a status hint after the prompt (`Detected: ...`).
    #[default]
    Suggest,
    /// `Suggest` plus auto-create (and start) a todo DAG for code tasks when
    /// orchestration is enabled and no todo list exists yet.
    Auto,
}

impl AutoMode {
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    #[must_use]
    pub const fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// Strong code-task verbs that alone (without file-path or code-domain
/// evidence) mark a prompt as a code task.
const STRONG_CODE_TASK_VERBS: &[&str] = &[
    "implement", "build", "refactor", "rewrite", "code", "program", "develop", "port",
    "migrate", "debug", "fix", "patch",
];

/// General task-tool-style verbs; a match needs additional code evidence.
const CODE_TASK_VERBS: &[&str] = &[
    "add", "update", "modify", "remove", "delete", "optimize", "write", "create", "change",
    "integrate", "wire", "extract", "split", "rename", "upgrade", "deprecate", "resolve",
    "address", "revert", "reproduce", "scaffold", "configure", "implement", "build",
    "refactor", "rewrite", "code", "program", "develop", "port", "migrate", "debug", "fix",
    "patch",
];

/// File-path evidence: extensions and path prefixes commonly pointing at code.
const CODE_PATH_MARKERS: &[&str] = &[
    ".rs", ".ts", ".js", ".jsx", ".tsx", ".py", ".go", ".c", ".h", ".cpp", ".hpp", ".java",
    ".rb", ".kt", ".swift", ".php", ".sh", ".sql", ".toml", ".json", ".yaml", ".yml",
    ".html", ".css", ".scss", ".vue", ".svelte",
];

const CODE_PATH_PREFIXES: &[&str] = &[
    "src/", "crates/", "lib/", "tests/", "test/", "app/", "components/", "packages/",
    "modules/", "cmd/", "internal/", "pkg/", "bin/", "core/", "api/", "scripts/",
];

/// Code-domain vocabulary reinforcing a task verb.
const CODE_DOMAIN_TOKENS: &[&str] = &[
    "function", "struct", "impl", "trait", "class", "interface", "api", "endpoint", "tool",
    "crate", "module", "cli", "command", "handler", "service", "component", "compiler",
    "lint", "codebase", "library", "sdk", "schema", "database", "migration", "bug", "error",
    "crash", "panic", "build", "test", "tests", "type", "variable", "loop", "callback",
    "async", "worker", "daemon", "plugin", "extension", "config", "parser", "compiler",
    "binary", "executable", "runtime", "framework", "dependency",
];

/// Interrogative starters that mark a prompt as a question even when it also
/// carries task verbs or code evidence ("what is the best way to implement...").
const QUESTION_STARTERS: &[&str] = &[
    "what is", "what's", "what are", "what does", "what do", "whats", "why", "how does",
    "how do", "how can", "how to", "when", "where", "who", "which", "is it", "is there",
    "are there", "can you explain", "explain", "compare", "difference between", "does it",
    "should i", "should we", "do you", "can you", "could you", "what is the",
];

/// Explicit goal phrasing that outranks every other heuristic.
const GOAL_PHRASES: &[&str] = &[
    "goal:", "my goal is", "the goal is", "long-running", "long running", "ongoing",
    "over the next", "in the coming", "i want to achieve", "i'd like to achieve",
    "i would like to achieve", "track this goal", "set a goal", "continuous effort",
];

/// Deterministic auto-mode classifier (MVP, no model calls).
///
/// Precedence: explicit goal phrasing → interrogative question → code task
/// (task verb plus strong verb, file-path, or code-domain evidence) → default
/// question. The result only drives advisory hints / bounded todo creation;
/// the session itself always runs the model as usual.
#[must_use]
pub fn classify_prompt(prompt: &str) -> PromptMode {
    let normalized = prompt.trim().to_lowercase();
    if normalized.is_empty() {
        return PromptMode::Question;
    }
    if GOAL_PHRASES
        .iter()
        .any(|phrase| normalized.contains(phrase))
    {
        return PromptMode::Goal;
    }
    let trimmed_end = normalized.trim_end_matches(['?', ' ', '.']);
    if QUESTION_STARTERS
        .iter()
        .any(|starter| trimmed_end.starts_with(starter))
        || normalized.trim_end().ends_with('?')
    {
        return PromptMode::Question;
    }
    let words = normalized.split_whitespace().collect::<Vec<_>>();
    let has_task_verb = CODE_TASK_VERBS
        .iter()
        .any(|verb| words.iter().any(|word| word_matches_verb(word, verb)));
    if !has_task_verb {
        return PromptMode::Question;
    }
    let has_strong_verb = STRONG_CODE_TASK_VERBS
        .iter()
        .any(|verb| words.iter().any(|word| word_matches_verb(word, verb)));
    let has_path = CODE_PATH_MARKERS
        .iter()
        .any(|marker| normalized.contains(marker))
        || CODE_PATH_PREFIXES
            .iter()
            .any(|prefix| normalized.contains(prefix));
    let has_code_token = CODE_DOMAIN_TOKENS
        .iter()
        .any(|token| normalized.contains(token));
    if has_strong_verb || has_path || has_code_token {
        PromptMode::CodeTask
    } else {
        PromptMode::Question
    }
}

/// Exact-word or inflected-form verb match (`implement`, `implements`,
/// `implementing`, `implemented`).
fn word_matches_verb(word: &str, verb: &str) -> bool {
    if word == verb {
        return true;
    }
    if let Some(stem) = word.strip_suffix('s')
        && stem == verb
    {
        return true;
    }
    if let Some(stem) = word.strip_suffix("ing")
        && stem == verb
    {
        return true;
    }
    if let Some(stem) = word.strip_suffix("ed")
        && stem == verb
    {
        return true;
    }
    false
}

/// Status-hint text for a detected mode, or `None` when no hint applies
/// (plain questions keep the default flow).
#[must_use]
pub const fn mode_hint(mode: PromptMode) -> Option<&'static str> {
    match mode {
        PromptMode::CodeTask => Some("Detected: code task — /todo to plan"),
        PromptMode::Goal => Some("Detected: long-running goal — /goal create to track"),
        PromptMode::Question => None,
    }
}

/// Minimal todo DAG seeded for an auto-detected code task: one phase whose
/// single task is the user prompt. `prepare_todo_phases` assigns the task id
/// and readiness, so the DAG is immediately executable.
#[must_use]
pub fn auto_create_todo_phases(prompt: &str) -> Vec<crate::TodoPhase> {
    vec![crate::TodoPhase {
        name: "Plan".to_owned(),
        tasks: vec![crate::TodoItem {
            id: String::new(),
            content: prompt.trim().to_owned(),
            status: crate::TodoStatus::Pending,
            depends_on: Vec::new(),
            ready: true,
            blocked_by: Vec::new(),
            agent: None,
        }],
    }]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SelectionKind {
    Skill,
    Agent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SelectionSource {
    Deterministic,
    Classifier,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionHit {
    pub kind: SelectionKind,
    pub name: String,
    pub description: String,
    pub score: i32,
    pub source: SelectionSource,
    pub reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub trusted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionPlan {
    pub request: String,
    pub skills: Vec<SelectionHit>,
    pub agents: Vec<SelectionHit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub autoload_skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_agent: Option<String>,
    pub classifier_used: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

impl SelectionPlan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
            && self.agents.is_empty()
            && self.autoload_skills.is_empty()
            && self.selected_agent.is_none()
    }
}

pub struct SelectionInput<'a> {
    pub request: &'a str,
    pub skills: &'a [Skill],
    pub agents: &'a [AgentDefinition],
    pub settings: &'a SelectorSettings,
}

#[derive(Clone)]
pub struct ProviderClassifier {
    pub model: Model,
    pub stream: StreamFn,
    pub options: SimpleStreamOptions,
    pub abort: AbortSignal,
}

#[derive(Clone, Debug)]
struct CandidateScore {
    index: usize,
    hit: SelectionHit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExactAgentMention {
    None,
    Unique(String),
    Ambiguous(Vec<String>),
}

impl ExactAgentMention {
    pub(crate) fn ambiguity_message(&self) -> Option<String> {
        let Self::Ambiguous(names) = self else {
            return None;
        };
        Some(format!(
            "exact agent mention is ambiguous between {}; pass the intended name in task.agent or rename agents so their normalized names are unique",
            names
                .iter()
                .map(|name| format!("{name:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

#[must_use]
pub(crate) fn exact_agent_mention(
    request: &str,
    agents: &[AgentDefinition],
) -> ExactAgentMention {
    let request_tokens = normalized_tokens(request);
    exact_agent_mention_for_phrase(&request_tokens.join(" "), agents)
}

fn exact_agent_mention_for_phrase(
    request_phrase: &str,
    agents: &[AgentDefinition],
) -> ExactAgentMention {
    let names = agents
        .iter()
        .filter(|agent| agent.trusted)
        .filter_map(|agent| {
            let name_phrase = normalized_tokens(&agent.name).join(" ");
            (!name_phrase.is_empty() && contains_phrase(request_phrase, &name_phrase))
                .then(|| agent.name.clone())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    match names.as_slice() {
        [] => ExactAgentMention::None,
        [name] => ExactAgentMention::Unique(name.clone()),
        _ => ExactAgentMention::Ambiguous(names),
    }
}

/// Ordered, deduplicated exact trusted agent names mentioned in `request`.
///
/// Unlike [`exact_agent_mention`] — which scans the normalized token stream
/// and is the single-target contract for task-tool/default selection — this
/// scan ALSO matches the raw request text with char-boundary semantics, so
/// CJK conjunctions without whitespace (`你让glm和grok一起调研这个仓库`)
/// keep ASCII names as separate runs instead of being glued into one token.
/// Names are returned in first-occurrence order, deduplicated, trusted-only.
#[must_use]
pub(crate) fn exact_agent_mentions(request: &str, agents: &[AgentDefinition]) -> Vec<String> {
    let raw_request = request
        .nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let token_request = normalized_tokens(request).join(" ");
    let mut found = Vec::new();
    for agent in agents.iter().filter(|agent| agent.trusted) {
        let name_phrase = normalized_tokens(&agent.name).join(" ");
        if name_phrase.is_empty() {
            continue;
        }
        // Raw-text position wins (the CJK-conjunction fix). The exact
        // original name is tried FIRST (NFKC-lowercased like the request, so
        // fullwidth forms normalize identically, and hyphens preserved) so
        // hyphenated forms like `research-agent` anchor at their true text
        // position instead of falling to the token stream and sorting after
        // every raw match. Token-stream-only matches (stopword-dropped forms
        // like `research the agent`) sort after all raw matches.
        let exact_name = agent
            .name
            .nfkc()
            .flat_map(char::to_lowercase)
            .collect::<String>();
        let raw_exact = phrase_first_pos(&raw_request, &exact_name)
            .map(|position| (exact_name.len(), position));
        let raw_phrase = phrase_first_pos(&raw_request, &name_phrase)
            .map(|position| (name_phrase.len(), position));
        let token_phrase = phrase_first_pos(&token_request, &name_phrase)
            .map(|position| (name_phrase.len(), position));
        let Some((space, position, span)) = raw_exact
            .or(raw_phrase)
            .map(|(span, position)| (0usize, position, span))
            .or_else(|| token_phrase.map(|(span, position)| (1usize, position, span)))
        else {
            continue;
        };
        found.push((space, position, span, agent.name.clone()));
    }
    // One token may match several catalog names (`grok` inside `grok-agent`):
    // the longest exact raw occurrence wins, so a single mention never fans
    // out to prefix-colliding definitions. Identical spans (distinct names
    // matching the same phrase, e.g. `Research-Agent` vs `research-agent`)
    // are kept — the caller reports them as ambiguous.
    let mut resolved = Vec::new();
    for entry in &found {
        let contained = found.iter().any(|other| {
            other.0 == entry.0
                && (other.1, other.1 + other.2) != (entry.1, entry.1 + entry.2)
                && other.1 <= entry.1
                && other.1 + other.2 >= entry.1 + entry.2
        });
        if !contained {
            resolved.push(entry.clone());
        }
    }
    // Position within each anchor space, with the name as a deterministic
    // tie-break (never catalog order).
    resolved.sort_by(|left, right| (left.0, left.1, &left.3).cmp(&(right.0, right.1, &right.3)));
    let mut seen = std::collections::BTreeSet::new();
    let mut mentions = Vec::new();
    for (_, _, _, name) in resolved {
        if seen.insert(name.clone()) {
            mentions.push(name);
        }
    }
    mentions
}

/// When two or more distinct catalog definitions in `mentions` normalize to
/// the same name phrase (e.g. `Research-Agent` vs `research-agent`), a single
/// textual mention cannot be attributed to one of them, so multi-target
/// fan-out is unsafe and the single-target ambiguity contract applies.
/// Returns the names of the first colliding normalized group, if any.
#[must_use]
pub(crate) fn exact_agent_mention_collisions(mentions: &[String]) -> Option<Vec<String>> {
    let mut by_phrase: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in mentions {
        by_phrase
            .entry(normalized_tokens(name).join(" "))
            .or_default()
            .push(name.clone());
    }
    by_phrase
        .into_iter()
        .find(|(_, names)| names.len() >= 2)
        .map(|(_, names)| names)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExactSkillMention {
    None,
    Unique(String),
    Ambiguous(Vec<String>),
}

/// Token-boundary match of a visible trusted skill's full normalized name.
///
/// Cross-kind task routing uses a unique exact skill mention to avoid promoting
/// an overlapping agent via description ranking. Exact agent mentions still win.
#[must_use]
pub(crate) fn exact_skill_mention(request: &str, skills: &[Skill]) -> ExactSkillMention {
    let request_phrase = normalized_tokens(request).join(" ");
    let names = skills
        .iter()
        .filter(|skill| skill.trusted && !skill.hidden && !skill.disable_model_invocation)
        .filter_map(|skill| {
            let name_phrase = normalized_tokens(&skill.name).join(" ");
            (!name_phrase.is_empty() && contains_phrase(&request_phrase, &name_phrase))
                .then(|| skill.name.clone())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    match names.as_slice() {
        [] => ExactSkillMention::None,
        [name] => ExactSkillMention::Unique(name.clone()),
        _ => ExactSkillMention::Ambiguous(names),
    }
}


#[must_use]
pub fn select_deterministic(input: SelectionInput<'_>) -> SelectionPlan {
    let exact_agent_mention = if input.settings.enabled {
        exact_agent_mention(input.request, input.agents)
    } else {
        ExactAgentMention::None
    };
    select_deterministic_with_exact_agent(input, exact_agent_mention)
}

fn select_deterministic_with_exact_agent(
    input: SelectionInput<'_>,
    exact_agent_mention: ExactAgentMention,
) -> SelectionPlan {
    if !input.settings.enabled {
        return SelectionPlan {
            request: input.request.to_owned(),
            fallback_reason: Some("selector disabled".to_owned()),
            ..SelectionPlan::default()
        };
    }
    let request_tokens = normalized_tokens(input.request);
    if request_tokens.is_empty() {
        return SelectionPlan {
            request: input.request.to_owned(),
            fallback_reason: Some("empty request".to_owned()),
            ..SelectionPlan::default()
        };
    }
    let request_phrase = request_tokens.join(" ");
    let paths = request_paths(input.request);
    let skills = rank_skills(
        &request_tokens,
        &request_phrase,
        &paths,
        input.skills,
        input.settings,
    );
    let mut agents = rank_agents_with_tokens(
        &request_tokens,
        &request_phrase,
        input.agents,
        input.settings,
    );
    // An explicit agent-name mention is a direct instruction: the mentioned
    // agent must outrank every skill suggestion, even when a same-named (or
    // always-applied) skill also matches the prompt. The skill is still
    // suggested and autoloaded, it just never wins the recommendation.
    if let ExactAgentMention::Unique(name) = &exact_agent_mention
        && let Some(agent_hit) = agents.iter_mut().find(|hit| hit.name == *name)
    {
        let skill_ceiling = skills.iter().map(|hit| hit.score).max().unwrap_or(0);
        if agent_hit.score <= skill_ceiling {
            agent_hit.score = skill_ceiling + 1;
            agent_hit.reasons.push(
                "explicit agent mention outranks skill recommendations".to_owned(),
            );
        }
    }
    let selected_agent = match &exact_agent_mention {
        ExactAgentMention::Unique(name) => Some(name.clone()),
        ExactAgentMention::Ambiguous(_) => None,
        ExactAgentMention::None => match exact_skill_mention(input.request, input.skills) {
            // Unique/ambiguous exact skill names must not auto-select an agent
            // via description ranking (e.g. skill `research` vs agent `researcher`).
            ExactSkillMention::Unique(_) | ExactSkillMention::Ambiguous(_) => None,
            ExactSkillMention::None => select_default_agent(&agents, input.settings),
        },
    };
    let mut autoload_skills = Vec::new();
    let mut seen_autoload_skills = BTreeSet::new();
    for skill in input.skills.iter().filter(|skill| {
        skill.trusted && skill.always_apply && !skill.disable_model_invocation
    }) {
        if seen_autoload_skills.insert(skill.name.clone()) {
            autoload_skills.push(skill.name.clone());
        }
    }
    if matches!(exact_agent_mention, ExactAgentMention::None) {
        for hit in skills.iter().filter(|hit| {
            hit.score >= input.settings.autoload_threshold
                && input.skills.iter().any(|skill| {
                    skill.name == hit.name && skill.trusted && !skill.disable_model_invocation
                })
        }) {
            if seen_autoload_skills.insert(hit.name.clone()) {
                autoload_skills.push(hit.name.clone());
            }
        }
    }
    if let Some(name) = selected_agent.as_deref()
        && let Some(agent) = input
            .agents
            .iter()
            .find(|agent| agent.trusted && agent.name == name)
    {
        for skill_name in &agent.autoload_skills {
            if input.skills.iter().any(|skill| {
                skill.trusted
                    && !skill.disable_model_invocation
                    && skill.name == *skill_name
            }) && seen_autoload_skills.insert(skill_name.clone())
            {
                autoload_skills.push(skill_name.clone());
            }
        }
    }
    let fallback_reason = exact_agent_mention.ambiguity_message().or_else(|| {
        (skills.is_empty() && agents.is_empty())
            .then(|| "no metadata match met the threshold".to_owned())
    });
    SelectionPlan {
        request: input.request.to_owned(),
        skills,
        agents,
        autoload_skills,
        selected_agent,
        classifier_used: false,
        fallback_reason,
    }
}

pub async fn select(
    input: SelectionInput<'_>,
    classifier: Option<&ProviderClassifier>,
) -> SelectionPlan {
    let classifier_enabled = input.settings.enabled && input.settings.classifier.enabled;
    let classifier_settings = input.settings.classifier.clone();
    let exact_agent_mention = exact_agent_mention(input.request, input.agents);
    let ambiguous_exact_agent = matches!(exact_agent_mention, ExactAgentMention::Ambiguous(_));
    let mut plan = select_deterministic_with_exact_agent(input, exact_agent_mention);
    if ambiguous_exact_agent {
        return plan;
    }
    if !classifier_enabled {
        return plan;
    }
    let Some(classifier) = classifier else {
        plan.fallback_reason = Some("classifier enabled but unavailable".to_owned());
        return plan;
    };
    if plan.skills.is_empty() && plan.agents.is_empty() {
        plan.fallback_reason = Some(
            "classifier skipped because deterministic ranking found no candidates".to_owned(),
        );
        return plan;
    }
    match classify(&plan, classifier, &classifier_settings).await {
        Ok(order) => {
            apply_classifier_order(&mut plan.skills, &order, SelectionKind::Skill);
            apply_classifier_order(&mut plan.agents, &order, SelectionKind::Agent);
            plan.classifier_used = true;
            plan.fallback_reason = None;
        }
        Err(error) => {
            plan.fallback_reason = Some(format!("classifier fallback: {error}"));
        }
    }
    plan
}

#[must_use]
pub fn rank_agents(
    request: &str,
    agents: &[AgentDefinition],
    settings: &SelectorSettings,
) -> Vec<SelectionHit> {
    if !settings.enabled {
        return Vec::new();
    }
    let tokens = normalized_tokens(request);
    if tokens.is_empty() {
        return Vec::new();
    }
    rank_agents_with_tokens(&tokens, &tokens.join(" "), agents, settings)
}

#[must_use]
pub fn select_default_agent(
    agents: &[SelectionHit],
    settings: &SelectorSettings,
) -> Option<String> {
    let first = agents.first()?;
    if !first.trusted || first.score < settings.auto_select_threshold {
        return None;
    }
    let runner_up = agents.get(1).map_or(0, |hit| hit.score);
    (first.score - runner_up >= settings.confidence_margin).then(|| first.name.clone())
}

fn rank_skills(
    request_tokens: &[String],
    request_phrase: &str,
    paths: &[String],
    skills: &[Skill],
    settings: &SelectorSettings,
) -> Vec<SelectionHit> {
    let mut candidates = Vec::new();
    for (index, skill) in skills.iter().enumerate() {
        if !skill.trusted || skill.hidden || skill.disable_model_invocation {
            continue;
        }
        let (mut score, mut reasons) = score_metadata(
            request_tokens,
            request_phrase,
            &skill.name,
            &skill.description,
        );
        if skill.always_apply {
            score += 2_000;
            reasons.push("alwaysApply requires this skill".to_owned());
        }
        for pattern in &skill.globs {
            let matcher = GlobBuilder::new(pattern)
                .literal_separator(false)
                .build()
                .ok()
                .map(|glob| glob.compile_matcher());
            if matcher
                .as_ref()
                .is_some_and(|matcher| paths.iter().any(|path| matcher.is_match(path)))
            {
                score += 800;
                reasons.push(format!("request path matches glob {pattern}"));
            }
        }
        if score > 0 {
            score += i32::from(skill.source.precedence());
            reasons.push(format!("{} source precedence", skill_source_name(skill.source)));
        }
        if score >= settings.min_score {
            candidates.push(CandidateScore {
                index,
                hit: SelectionHit {
                    kind: SelectionKind::Skill,
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    score,
                    source: SelectionSource::Deterministic,
                    reasons,
                    location: Some(format!("skill://{}", skill.name)),
                    trusted: skill.trusted,
                },
            });
        }
    }
    finish_ranking(candidates, settings.max_results.clamp(1, 20))
}

fn rank_agents_with_tokens(
    request_tokens: &[String],
    request_phrase: &str,
    agents: &[AgentDefinition],
    settings: &SelectorSettings,
) -> Vec<SelectionHit> {
    let mut candidates = Vec::new();
    for (index, agent) in agents.iter().enumerate() {
        if !agent.trusted {
            continue;
        }
        let (mut score, mut reasons) = score_metadata(
            request_tokens,
            request_phrase,
            &agent.name,
            &agent.description,
        );
        if score > 0 {
            score += match agent.source {
                AgentDefinitionSource::Project => 50,
                AgentDefinitionSource::User => 40,
                AgentDefinitionSource::Bundled => 30,
            };
            reasons.push(format!(
                "{} agent source precedence",
                agent_source_name(agent.source)
            ));
        }
        if score >= settings.min_score {
            candidates.push(CandidateScore {
                index,
                hit: SelectionHit {
                    kind: SelectionKind::Agent,
                    name: agent.name.clone(),
                    description: agent.description.clone(),
                    score,
                    source: SelectionSource::Deterministic,
                    reasons,
                    location: agent
                        .path
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                    trusted: agent.trusted,
                },
            });
        }
    }
    finish_ranking(candidates, settings.max_results.clamp(1, 20))
}

fn score_metadata(
    request_tokens: &[String],
    request_phrase: &str,
    name: &str,
    description: &str,
) -> (i32, Vec<String>) {
    let request_set = request_tokens.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let name_tokens = normalized_tokens(name);
    let description_tokens = normalized_tokens(description);
    let name_phrase = name_tokens.join(" ");
    let mut score = 0;
    let mut reasons = Vec::new();
    if !name_phrase.is_empty() && request_phrase == name_phrase {
        score += 1_600;
        reasons.push("request exactly matches the name".to_owned());
    } else if !name_phrase.is_empty() && contains_phrase(request_phrase, &name_phrase) {
        score += 1_000;
        reasons.push(format!("exact name phrase {name_phrase:?}"));
    }
    let matched_name = unique_overlap(&request_set, &name_tokens);
    if !matched_name.is_empty() {
        score += i32::try_from(matched_name.len()).unwrap_or(i32::MAX) * 220;
        reasons.push(format!("name tokens: {}", matched_name.join(", ")));
    }
    let matched_description = unique_overlap(&request_set, &description_tokens);
    if !matched_description.is_empty() {
        score += i32::try_from(matched_description.len()).unwrap_or(i32::MAX) * 60;
        reasons.push(format!(
            "description tokens: {}",
            matched_description.join(", ")
        ));
    }
    if let Some(phrase) = longest_shared_phrase(request_tokens, &description_tokens)
        && phrase.len() >= 2
    {
        let phrase_text = phrase.join(" ");
        score += i32::try_from(phrase.len()).unwrap_or(i32::MAX) * 90;
        reasons.push(format!("description phrase: {phrase_text}"));
    }
    (score, reasons)
}

fn finish_ranking(mut candidates: Vec<CandidateScore>, max_results: usize) -> Vec<SelectionHit> {
    candidates.sort_by(|left, right| {
        right
            .hit
            .score
            .cmp(&left.hit.score)
            .then_with(|| left.index.cmp(&right.index))
            .then_with(|| left.hit.name.cmp(&right.hit.name))
    });
    candidates
        .into_iter()
        .take(max_results)
        .map(|candidate| candidate.hit)
        .collect()
}

async fn classify(
    plan: &SelectionPlan,
    classifier: &ProviderClassifier,
    settings: &SelectorClassifierSettings,
) -> Result<BTreeMap<(SelectionKind, String), usize>> {
    if classifier.abort.is_aborted() {
        bail!("request aborted");
    }
    let candidates = plan
        .skills
        .iter()
        .chain(&plan.agents)
        .map(|hit| {
            serde_json::json!({
                "kind": hit.kind,
                "name": hit.name,
                "description": hit.description,
                "deterministicScore": hit.score,
            })
        })
        .collect::<Vec<_>>();
    let prompt = serde_json::json!({
        "task": plan.request,
        "candidates": candidates,
    });
    let mut options = classifier.options.clone();
    options.stream.max_tokens = Some(settings.max_tokens.clamp(1, MAX_CLASSIFIER_TOKENS));
    options.stream.timeout_ms = Some(settings.timeout_ms.clamp(1, 60_000));
    options.stream.abort_signal = Some(classifier.abort.cancellation_token());
    options.reasoning = None;
    let stream = (classifier.stream)(
        classifier.model.clone(),
        Context {
            system_prompt: "Rank only the supplied candidates for the task. Return JSON as [{\"kind\":\"skill\"|\"agent\",\"name\":\"...\"}], best first. Do not invent names. Return [] when none fit.".to_owned(),
            messages: vec![Message::user_text(prompt.to_string(), pi_ai::now_millis())],
            tools: Vec::new(),
        },
        options,
    )
    .await;
    while stream.next().await.is_some() {}
    let response = stream
        .result()
        .await
        .ok_or_else(|| anyhow!("provider returned no classifier response"))?;
    if response.stop_reason == StopReason::Error || response.stop_reason == StopReason::Aborted {
        bail!(
            "{}",
            response
                .error_message
                .unwrap_or_else(|| "provider classifier failed".to_owned())
        );
    }
    let value = pi_ai::parse_json_with_repair(&response.text())
        .ok_or_else(|| anyhow!("classifier response was not valid JSON"))?;
    let entries = value
        .as_array()
        .ok_or_else(|| anyhow!("classifier response must be an array"))?;
    let allowed = plan
        .skills
        .iter()
        .chain(&plan.agents)
        .map(|hit| ((hit.kind, hit.name.clone()), ()))
        .collect::<BTreeMap<_, _>>();
    let mut order = BTreeMap::new();
    for (position, entry) in entries.iter().enumerate() {
        let Some(object) = entry.as_object() else {
            continue;
        };
        let kind = match object.get("kind").and_then(serde_json::Value::as_str) {
            Some("skill") => SelectionKind::Skill,
            Some("agent") => SelectionKind::Agent,
            _ => continue,
        };
        let Some(name) = object.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let key = (kind, name.to_owned());
        if allowed.contains_key(&key) {
            order.entry(key).or_insert(position);
        }
    }
    Ok(order)
}

fn apply_classifier_order(
    hits: &mut [SelectionHit],
    order: &BTreeMap<(SelectionKind, String), usize>,
    kind: SelectionKind,
) {
    let original = hits
        .iter()
        .enumerate()
        .map(|(index, hit)| (hit.name.clone(), index))
        .collect::<BTreeMap<_, _>>();
    hits.sort_by_key(|hit| {
        order
            .get(&(kind, hit.name.clone()))
            .copied()
            .map_or((1, original[&hit.name]), |position| (0, position))
    });
    for hit in hits {
        if let Some(position) = order.get(&(kind, hit.name.clone())) {
            hit.source = SelectionSource::Classifier;
            hit.reasons
                .push(format!("optional classifier rank {}", position + 1));
        }
    }
}

#[must_use]
pub fn render_selection_prompt(plan: &SelectionPlan) -> String {
    if plan.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "<selection_recommendations>".to_owned(),
        "These deterministic, trust-filtered rankings augment the normal prompt policy; they do not replace model judgment. Read skill:// content only when it fits the task.".to_owned(),
    ];
    if let Some(agent) = &plan.selected_agent {
        lines.push(format!(
            "  <selected_default_agent>{}</selected_default_agent>",
            escape_xml(agent)
        ));
    }
    for hit in plan.skills.iter().chain(&plan.agents) {
        lines.push(format!(
            "  <recommendation kind=\"{}\" name=\"{}\" score=\"{}\" source=\"{}\">",
            selection_kind_name(hit.kind),
            escape_xml(&hit.name),
            hit.score,
            selection_source_name(hit.source),
        ));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&hit.description)
        ));
        lines.push(format!(
            "    <reason>{}</reason>",
            escape_xml(&hit.reasons.join("; "))
        ));
        if let Some(location) = &hit.location {
            lines.push(format!(
                "    <location>{}</location>",
                escape_xml(location)
            ));
        }
        lines.push("  </recommendation>".to_owned());
    }
    if !plan.autoload_skills.is_empty() {
        lines.push(format!(
            "  <autoload_skills>{}</autoload_skills>",
            escape_xml(&plan.autoload_skills.join(","))
        ));
    }
    if let Some(reason) = &plan.fallback_reason {
        lines.push(format!("  <fallback>{}</fallback>", escape_xml(reason)));
    }
    lines.push("</selection_recommendations>".to_owned());
    lines.join("\n")
}

pub fn resolve_skill_uri(uri: &str, skills: &[Skill]) -> Result<PathBuf> {
    let rest = uri
        .strip_prefix("skill://")
        .ok_or_else(|| anyhow!("skill URI must start with skill://"))?;
    if rest.is_empty() || rest.contains(['?', '#', '\\']) {
        bail!("invalid skill URI: {uri}");
    }
    let (name, relative) = rest.split_once('/').unwrap_or((rest, ""));
    if name.is_empty() {
        bail!("skill URI is missing a skill name");
    }
    let skill = skills
        .iter()
        .find(|skill| skill.trusted && skill.name == name)
        .ok_or_else(|| anyhow!("unknown or untrusted skill: {name}"))?;
    let base = std::fs::canonicalize(&skill.base_dir)
        .map_err(|error| anyhow!("resolving skill base {}: {error}", skill.base_dir))?;
    let declared_file = std::fs::canonicalize(&skill.file_path)
        .map_err(|error| anyhow!("resolving skill file {}: {error}", skill.file_path))?;
    if !declared_file.starts_with(&base) {
        bail!("skill file escapes its base directory: {}", skill.file_path);
    }
    let candidate = if relative.is_empty() {
        declared_file
    } else {
        let relative_path = Path::new(relative);
        if relative_path.is_absolute()
            || relative_path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            bail!("skill URI escapes its base directory: {uri}");
        }
        base.join(relative_path)
    };
    let resolved = std::fs::canonicalize(&candidate)
        .map_err(|error| anyhow!("resolving skill URI {uri}: {error}"))?;
    if !resolved.starts_with(&base) {
        bail!("skill URI escapes its base directory: {uri}");
    }
    Ok(resolved)
}

#[must_use]
pub fn load_autoload_skill_bodies(
    plan: &SelectionPlan,
    skills: &[Skill],
) -> Vec<(String, String)> {
    let mut total_bytes = 0usize;
    let mut bodies = Vec::new();
    for name in &plan.autoload_skills {
        let Some(skill) = skills.iter().find(|skill| {
            skill.name == *name && skill.trusted && !skill.disable_model_invocation
        }) else {
            continue;
        };
        let Ok(path) = resolve_skill_uri(&format!("skill://{}", skill.name), skills) else {
            continue;
        };
        let Ok(body) = crate::read_resource_text(&path, "autoload skill") else {
            continue;
        };
        let body = crate::resources::strip_skill_frontmatter(&body);
        let Some(next_total) = total_bytes.checked_add(body.len()) else {
            break;
        };
        if next_total > MAX_AUTOLOAD_SKILL_BYTES {
            break;
        }
        total_bytes = next_total;
        bodies.push((skill.name.clone(), body));
    }
    bodies
}

fn normalized_tokens(text: &str) -> Vec<String> {
    let normalized = text.nfkc().flat_map(char::to_lowercase).collect::<String>();
    let mut tokens = Vec::new();
    let mut current = String::new();
    for character in normalized.chars() {
        if character.is_alphanumeric() {
            current.push(character);
        } else if !current.is_empty() {
            let token = std::mem::take(&mut current);
            if !is_stopword(&token) {
                tokens.push(token);
            }
        }
    }
    if !current.is_empty() && !is_stopword(&current) {
        tokens.push(current);
    }
    tokens
}

fn is_stopword(token: &str) -> bool {
    matches!(
        token,
        "a" | "an" | "and" | "are" | "as" | "at" | "be" | "by" | "for" | "from"
            | "in" | "is" | "it" | "of" | "on" | "or" | "please" | "that" | "the"
            | "this" | "to" | "use" | "with"
    )
}

fn request_paths(request: &str) -> Vec<String> {
    request
        .split_whitespace()
        .map(|part| {
            part.trim_matches(|character: char| {
                matches!(
                    character,
                    ',' | ';' | ':' | '(' | ')' | '[' | ']' | '`' | '"' | '\''
                )
            })
        })
        .filter(|part| {
            !part.is_empty()
                && (part.contains('/')
                    || part.contains('\\')
                    || Path::new(part).extension().is_some())
        })
        .map(|part| part.replace('\\', "/"))
        .collect()
}

fn contains_phrase(haystack: &str, needle: &str) -> bool {
    haystack == needle
        || haystack.starts_with(&format!("{needle} "))
        || haystack.ends_with(&format!(" {needle}"))
        || haystack.contains(&format!(" {needle} "))
        || contains_embedded_ascii_word(haystack, needle)
}

/// Word-boundary substring match for ASCII names embedded in scripts that do
/// not use whitespace (CJK): `你让researcher去调查` contains the word
/// `researcher` even though tokenization produced one contiguous run. The
/// match requires the characters around `needle` to be non-ASCII-alphanumeric,
/// so `researcher` never matches inside `researchers` and `research` never
/// matches inside `researcher`.
fn contains_embedded_ascii_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || !needle.is_ascii() || haystack.len() < needle.len() {
        return false;
    }
    let mut search_from = 0;
    while let Some(relative) = haystack[search_from..].find(needle) {
        let at = search_from + relative;
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + needle.len()..].chars().next();
        let boundary_before = before.is_none_or(|character| !character.is_ascii_alphanumeric());
        let boundary_after = after.is_none_or(|character| !character.is_ascii_alphanumeric());
        if boundary_before && boundary_after {
            return true;
        }
        search_from = at + needle.len();
    }
    false
}

/// Byte position of the first word-boundary occurrence of `needle` in
/// `haystack`, mirroring [`contains_phrase`]'s match set: whole-phrase,
/// space-delimited, or an ASCII word embedded in a non-whitespace script
/// (CJK). `None` when there is no word-boundary occurrence.
fn phrase_first_pos(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    if haystack == needle {
        return Some(0);
    }
    let mut best: Option<usize> = None;
    let mut consider = |position: usize| {
        best = Some(best.map_or(position, |current| current.min(position)));
    };
    if haystack.starts_with(&format!("{needle} ")) {
        consider(0);
    }
    if let Some(prefix) = haystack.strip_suffix(&format!(" {needle}")) {
        consider(prefix.len() + 1);
    }
    if let Some(relative) = haystack.find(&format!(" {needle} ")) {
        consider(relative + 1);
    }
    // ASCII word embedded in a script without whitespace (CJK): scan every
    // occurrence and keep the first with non-ASCII-alphanumeric neighbors,
    // so `researcher` never matches inside `researchers`.
    if needle.is_ascii() {
        let mut search_from = 0;
        while let Some(relative) = haystack[search_from..].find(needle) {
            let at = search_from + relative;
            let before = haystack[..at].chars().next_back();
            let after = haystack[at + needle.len()..].chars().next();
            let boundary_before = before.is_none_or(|character| !character.is_ascii_alphanumeric());
            let boundary_after = after.is_none_or(|character| !character.is_ascii_alphanumeric());
            if boundary_before && boundary_after {
                consider(at);
                break;
            }
            search_from = at + needle.len();
        }
    }
    best
}

fn unique_overlap(request: &BTreeSet<&str>, candidate: &[String]) -> Vec<String> {
    candidate
        .iter()
        .filter(|token| request.contains(token.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn longest_shared_phrase(request: &[String], candidate: &[String]) -> Option<Vec<String>> {
    let mut best = Vec::new();
    for request_start in 0..request.len() {
        for candidate_start in 0..candidate.len() {
            let mut length = 0;
            while request.get(request_start + length) == candidate.get(candidate_start + length)
                && request.get(request_start + length).is_some()
            {
                length += 1;
            }
            if length > best.len() {
                best = request[request_start..request_start + length].to_vec();
            }
        }
    }
    (!best.is_empty()).then_some(best)
}


const fn skill_source_name(source: SkillSource) -> &'static str {
    match source {
        SkillSource::User => "user",
        SkillSource::Project => "project",
        SkillSource::PackageGlobal => "global package",
        SkillSource::PackageProject => "project package",
        SkillSource::Explicit => "explicit",
    }
}

const fn agent_source_name(source: AgentDefinitionSource) -> &'static str {
    match source {
        AgentDefinitionSource::Project => "project",
        AgentDefinitionSource::User => "user",
        AgentDefinitionSource::Bundled => "bundled",
    }
}

const fn selection_kind_name(kind: SelectionKind) -> &'static str {
    match kind {
        SelectionKind::Skill => "skill",
        SelectionKind::Agent => "agent",
    }
}

const fn selection_source_name(source: SelectionSource) -> &'static str {
    match source {
        SelectionSource::Deterministic => "deterministic",
        SelectionSource::Classifier => "classifier",
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures_util::FutureExt;
    use pi_agent::AbortController;
    use pi_ai::{AssistantMessage, AssistantMessageEvent, StopReason};
    use tempfile::TempDir;

    use super::*;

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.to_owned(),
            description: description.to_owned(),
            file_path: format!("/tmp/{name}/SKILL.md"),
            base_dir: format!("/tmp/{name}"),
            globs: Vec::new(),
            always_apply: false,
            hidden: false,
            disable_model_invocation: false,
            source: SkillSource::User,
            trusted: true,
        }
    }

    fn agent(name: &str, description: &str) -> AgentDefinition {
        AgentDefinition {
            name: name.to_owned(),
            description: description.to_owned(),
            system_prompt: "prompt".to_owned(),
            tools: None,
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: None,
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: crate::orchestration::AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
        }
    }

    fn deterministic(request: &str, skills: &[Skill]) -> SelectionPlan {
        select_deterministic(SelectionInput {
            request,
            skills,
            agents: &[],
            settings: &SelectorSettings::default(),
        })
    }

    #[test]
    fn exact_name_and_description_overlap_rank_explainably() {
        let skills = vec![
            skill("rust-review", "Review Rust code for memory safety"),
            skill("docs", "Write documentation for Rust projects"),
        ];
        let plan = deterministic("Please use rust-review for this safety review", &skills);
        assert_eq!(plan.skills[0].name, "rust-review");
        assert!(
            plan.skills[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("exact name"))
        );
        assert!(
            plan.skills[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("description"))
        );
        assert!(plan.skills[0].score > plan.skills[1].score);
    }

    #[test]
    fn glob_and_always_apply_rank_above_plain_overlap() {
        let mut rust = skill("rust", "Edit source files");
        rust.globs = vec!["**/*.rs".to_owned()];
        let mut policy = skill("policy", "Project instructions");
        policy.always_apply = true;
        let plan = deterministic("Update src/lib.rs", &[rust, policy]);
        assert_eq!(plan.skills[0].name, "policy");
        assert!(plan.skills.iter().any(|hit| {
            hit.name == "rust" && hit.reasons.iter().any(|reason| reason.contains("glob"))
        }));
        assert!(plan.autoload_skills.contains(&"policy".to_owned()));
    }

    #[test]
    fn stable_ties_preserve_discovery_order() {
        let skills = vec![
            skill("first", "shared token"),
            skill("second", "shared token"),
        ];
        let plan = deterministic("shared", &skills);
        assert_eq!(
            plan.skills
                .iter()
                .map(|hit| hit.name.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn untrusted_and_hidden_skills_are_excluded() {
        let mut untrusted = skill("project", "deploy application");
        untrusted.source = SkillSource::Project;
        untrusted.trusted = false;
        let mut hidden = skill("hidden", "deploy application");
        hidden.hidden = true;
        let plan = deterministic("deploy application", &[untrusted, hidden]);
        assert!(plan.skills.is_empty());
    }

    #[test]
    fn empty_and_no_match_fall_back_without_hits() {
        let skills = vec![skill("rust", "review code")];
        assert_eq!(
            deterministic("", &skills).fallback_reason.as_deref(),
            Some("empty request")
        );
        let plan = deterministic("translate a poem", &skills);
        assert!(plan.skills.is_empty());
        assert_eq!(
            plan.fallback_reason.as_deref(),
            Some("no metadata match met the threshold")
        );
    }

    #[test]
    fn agent_autoload_skills_require_confident_trusted_default() {
        let skills = vec![skill("security", "security review")];
        let mut reviewer = agent("security-reviewer", "security reviewer for audits");
        reviewer.autoload_skills = vec!["security".to_owned(), "missing".to_owned()];
        let settings = SelectorSettings {
            confidence_margin: 0,
            ..SelectorSettings::default()
        };
        let plan = select_deterministic(SelectionInput {
            request: "security-reviewer",
            skills: &skills,
            agents: &[reviewer],
            settings: &settings,
        });
        assert_eq!(plan.selected_agent.as_deref(), Some("security-reviewer"));
        assert_eq!(plan.autoload_skills, vec!["security"]);
    }

    #[test]
    fn exact_agent_mention_suppresses_ranked_skills_but_keeps_agent_declarations() {
        let rust = skill("rust-review", "Review Rust code for memory safety");
        let security = skill("security-audit", "Audit security vulnerabilities");
        let mut reviewer = agent("reviewer", "Review Rust security patches");
        reviewer.autoload_skills = vec!["rust-review".to_owned()];
        let settings = SelectorSettings {
            autoload_threshold: 1,
            auto_select_threshold: 1,
            confidence_margin: 0,
            min_score: 0,
            ..SelectorSettings::default()
        };
        let plan = select_deterministic(SelectionInput {
            request: "reviewer audit security vulnerabilities in Rust code",
            skills: &[rust, security],
            agents: &[reviewer],
            settings: &settings,
        });
        assert_eq!(plan.selected_agent.as_deref(), Some("reviewer"));
        assert_eq!(plan.autoload_skills, vec!["rust-review"]);
    }

    #[test]
    fn generic_ranked_skills_autoload_in_rank_order_and_dedupe_agent_declarations() {
        let rust = skill("rust-review", "Review Rust code for memory safety");
        let security = skill("security-audit", "Audit security vulnerabilities");
        let mut reviewer = agent("reviewer", "Review Rust security patches");
        reviewer.autoload_skills = vec!["rust-review".to_owned()];
        let settings = SelectorSettings {
            autoload_threshold: 1,
            auto_select_threshold: 1,
            confidence_margin: 0,
            min_score: 0,
            ..SelectorSettings::default()
        };
        let plan = select_deterministic(SelectionInput {
            request: "audit security vulnerabilities in Rust code",
            skills: &[rust, security],
            agents: &[reviewer],
            settings: &settings,
        });
        assert_eq!(plan.selected_agent.as_deref(), Some("reviewer"));
        assert_eq!(plan.autoload_skills, vec!["security-audit", "rust-review"]);
    }

    #[test]
    fn exact_agent_mention_wins_over_overlapping_skill_and_suppresses_ranked_autoload() {
        let research = skill("research", "Research topics for a researcher study");
        let policy = {
            let mut skill = skill("policy", "Always applied policy");
            skill.always_apply = true;
            skill
        };
        let mut researcher = agent("researcher", "Research and study assigned topics");
        researcher.autoload_skills = vec!["policy".to_owned()];
        let settings = SelectorSettings {
            autoload_threshold: 1,
            auto_select_threshold: 1,
            confidence_margin: 0,
            min_score: 0,
            ..SelectorSettings::default()
        };
        let plan = select_deterministic(SelectionInput {
            request: "Have researcher study this",
            skills: &[research, policy],
            agents: &[researcher],
            settings: &settings,
        });
        assert_eq!(plan.selected_agent.as_deref(), Some("researcher"));
        assert_eq!(plan.autoload_skills, vec!["policy"]);
        assert!(plan.skills.iter().any(|hit| hit.name == "research"));
    }

    #[test]
    fn exact_skill_name_still_autoloads_without_an_exact_agent_mention() {
        let settings = SelectorSettings {
            autoload_threshold: 1,
            min_score: 0,
            ..SelectorSettings::default()
        };
        let research = skill("research", "Research assigned topics");
        let plan = select_deterministic(SelectionInput {
            request: "Use research for this",
            skills: &[research],
            agents: &[agent("researcher", "Study assigned topics")],
            settings: &settings,
        });
        assert_eq!(plan.skills[0].name, "research");
        assert_eq!(plan.autoload_skills, vec!["research"]);
        assert!(plan.selected_agent.is_none());
    }

    #[test]
    fn exact_skill_mention_suppresses_overlapping_agent_auto_select() {
        let settings = SelectorSettings {
            autoload_threshold: 1,
            auto_select_threshold: 1,
            confidence_margin: 0,
            min_score: 0,
            ..SelectorSettings::default()
        };
        let research = skill("research", "Research topics for a researcher study");
        let researcher = agent("researcher", "Research and study assigned topics");
        assert!(matches!(
            exact_skill_mention("Use research for this", &[research.clone()]),
            ExactSkillMention::Unique(name) if name == "research"
        ));
        assert!(matches!(
            exact_agent_mention("Use research for this", &[researcher.clone()]),
            ExactAgentMention::None
        ));
        let plan = select_deterministic(SelectionInput {
            request: "Use research for this",
            skills: &[research],
            agents: &[researcher],
            settings: &settings,
        });
        assert_eq!(plan.skills[0].name, "research");
        assert!(
            plan.selected_agent.is_none(),
            "exact skill mention must not auto-select overlapping researcher: {:?}",
            plan.selected_agent
        );
        assert_eq!(plan.autoload_skills, vec!["research"]);
    }


    #[test]
    fn ambiguous_exact_normalized_agent_names_fail_without_silent_selection() {
        let settings = SelectorSettings {
            auto_select_threshold: 1,
            confidence_margin: 0,
            min_score: 0,
            ..SelectorSettings::default()
        };
        let plan = select_deterministic(SelectionInput {
            request: "Have Research Agent study this",
            skills: &[],
            agents: &[
                agent("Research-Agent", "First researcher"),
                agent("research-agent", "Second researcher"),
            ],
            settings: &settings,
        });
        assert!(plan.selected_agent.is_none());
        let message = plan.fallback_reason.expect("actionable ambiguity error");
        assert!(message.contains("ambiguous"), "{message}");
        assert!(message.contains("Research-Agent"), "{message}");
        assert!(message.contains("research-agent"), "{message}");
        assert!(message.contains("task.agent"), "{message}");
    }

    #[test]
    fn exact_agent_mention_detects_cjk_embedded_and_ascii_word_boundaries() {
        let researcher = agent("researcher", "Research and study assigned topics");
        // CJK scripts do not use whitespace: the ASCII agent name sits inside
        // a contiguous alphanumeric run and must still count as a word
        // mention, in every casing/whitespace arrangement.
        for request in [
            "你让researcher去调查这个项目",
            "你让Researcher去调查这个项目",
            "让researcher去调查",
            "researcher去调查",
            "researcher 去调查这个项目",
            "去调查researcher",
        ] {
            assert!(
                matches!(
                    exact_agent_mention(request, &[researcher.clone()]),
                    ExactAgentMention::Unique(name) if name == "researcher"
                ),
                "expected a unique researcher mention in {request:?}"
            );
        }
        // Partial words and unrelated prompts must not match: `researcher`
        // never matches inside `researchers` or `researcherx`, and CJK-only
        // text matches nothing.
        for request in [
            "researchers are studying this",
            "researcherx 去调查",
            "请研究一下这个项目",
            "调查一下技术栈",
        ] {
            assert!(
                matches!(
                    exact_agent_mention(request, &[researcher.clone()]),
                    ExactAgentMention::None
                ),
                "expected no agent mention in {request:?}"
            );
        }
    }

    #[test]
    fn exact_agent_mentions_finds_plural_cjk_and_english_names_in_mention_order() {
        let glm = agent("glm", "General language model agent");
        let grok = agent("grok", "Research assistant agent");
        let agents = [glm, grok.clone()];
        // CJK conjunctions without whitespace keep ASCII names as separate
        // runs; the single-target tokenizer would glue them into one token.
        assert_eq!(
            exact_agent_mentions("你让glm和grok一起调研这个仓库", &agents),
            vec!["glm".to_owned(), "grok".to_owned()]
        );
        // Spaced and fullwidth-conjunction variants and English lists.
        for request in [
            "你让glm 和 grok一起调研这个仓库",
            "你让glm、grok一起调研这个仓库",
            "Have glm and grok study this",
        ] {
            assert_eq!(
                exact_agent_mentions(request, &agents),
                vec!["glm".to_owned(), "grok".to_owned()],
                "{request}"
            );
        }
        // Mention order follows the request, not the catalog.
        assert_eq!(
            exact_agent_mentions("Have grok and glm study this", &agents),
            vec!["grok".to_owned(), "glm".to_owned()]
        );
        // Repeated mentions dedupe to one entry per agent.
        assert_eq!(
            exact_agent_mentions("Have glm, grok and glm study this", &agents),
            vec!["glm".to_owned(), "grok".to_owned()]
        );
        // Single-name control.
        assert_eq!(
            exact_agent_mentions("你让grok调研这个仓库", &agents),
            vec!["grok".to_owned()]
        );
        // Partial-word negatives and name-free prompts match nothing.
        assert!(
            exact_agent_mentions("你让glmx和grokx一起调研这个仓库", &agents).is_empty(),
            "partial words must not match"
        );
        assert!(exact_agent_mentions("请研究一下这个项目", &agents).is_empty());
        // A hyphenated name mentioned BEFORE a raw name keeps its text order:
        // the exact-name raw scan anchors `research-agent` at its true
        // position instead of demoting it to the token stream.
        let research_agent = agent("research-agent", "Hyphenated researcher");
        assert_eq!(
            exact_agent_mentions(
                "Have research-agent and grok study this",
                &[research_agent, grok],
            ),
            vec!["research-agent".to_owned(), "grok".to_owned()]
        );
    }

    #[test]
    fn exact_agent_mentions_resolve_prefix_collisions_to_longest_name() {
        let grok = agent("grok", "Research assistant agent");
        let grok_agent = agent("grok-agent", "Specialized researcher");
        let agents = [grok.clone(), grok_agent.clone()];
        // One token naming the longer agent never fans out to its prefix
        // (`grok` inside `grok-agent` is a substring of an identifier, not a
        // separate mention).
        assert_eq!(
            exact_agent_mentions("Have grok-agent study this", &agents),
            vec!["grok-agent".to_owned()]
        );
        assert_eq!(
            exact_agent_mentions("Have grok study this", &agents),
            vec!["grok".to_owned()]
        );
        // Both explicitly named: both mention, in text order.
        assert_eq!(
            exact_agent_mentions("Have grok and grok-agent study this", &agents),
            vec!["grok".to_owned(), "grok-agent".to_owned()]
        );
    }

    #[test]
    fn exact_agent_mentions_normalize_fullwidth_forms() {
        // NFKC parity: a fullwidth agent name matches a fullwidth request,
        // and a fullwidth request matches an ASCII name.
        let fullwidth = agent("ｇｌｍ", "Fullwidth general language model agent");
        assert_eq!(
            exact_agent_mentions("让ｇｌｍ调研这个仓库", &[fullwidth.clone()]),
            vec!["ｇｌｍ".to_owned()]
        );
        let glm = agent("glm", "General language model agent");
        assert_eq!(
            exact_agent_mentions("让ｇｌｍ调研这个仓库", &[glm]),
            vec!["glm".to_owned()]
        );
    }

    #[test]
    fn exact_agent_mention_keeps_single_target_contract() {
        let glm = agent("glm", "General language model agent");
        let grok = agent("grok", "Research assistant agent");
        // The single-target scan keeps its normalized-token contract: the
        // task tool still sees one name (Unique) or several (Ambiguous) and
        // never multi-selects.
        assert!(matches!(
            exact_agent_mention("Have glm study this", &[glm.clone(), grok.clone()]),
            ExactAgentMention::Unique(name) if name == "glm"
        ));
        assert!(matches!(
            exact_agent_mention("Have glm and grok study this", &[glm, grok]),
            ExactAgentMention::Ambiguous(_)
        ));
    }

    #[test]
    fn exact_agent_mention_collisions_flag_normalized_duplicates_only() {
        let upper = agent("Research-Agent", "First researcher");
        let lower = agent("research-agent", "Second researcher");
        let agents = [upper.clone(), lower.clone()];
        // The hyphen-split mention matches BOTH definitions after
        // normalization, so a single textual mention stays ambiguous.
        let mentions = exact_agent_mentions("你让Research-Agent调研这个", &agents);
        assert_eq!(mentions.len(), 2, "{mentions:?}");
        let colliding = exact_agent_mention_collisions(&mentions).expect("collision");
        assert!(colliding.contains(&"Research-Agent".to_owned()), "{colliding:?}");
        assert!(colliding.contains(&"research-agent".to_owned()), "{colliding:?}");
        // Distinct names never collide: multi-target fan-out is safe.
        let glm = agent("glm", "General language model agent");
        let grok = agent("grok", "Research assistant agent");
        let mentions = exact_agent_mentions("Have glm and grok study this", &[glm, grok]);
        assert_eq!(mentions, vec!["glm".to_owned(), "grok".to_owned()]);
        assert!(exact_agent_mention_collisions(&mentions).is_none());
    }

    #[test]
    fn cjk_embedded_agent_mention_wins_over_same_named_skill() {
        let settings = SelectorSettings {
            autoload_threshold: 1,
            auto_select_threshold: 1,
            confidence_margin: 0,
            min_score: 0,
            ..SelectorSettings::default()
        };
        let research = skill("research", "Research topics for a researcher study");
        let researcher = agent("researcher", "Research and study assigned topics");
        let plan = select_deterministic(SelectionInput {
            request: "你让researcher去调查这个项目",
            skills: &[research],
            agents: &[researcher],
            settings: &settings,
        });
        assert_eq!(plan.selected_agent.as_deref(), Some("researcher"));
        let researcher_hit = plan
            .agents
            .iter()
            .find(|hit| hit.name == "researcher")
            .expect("the mentioned agent must be ranked");
        assert!(
            researcher_hit
                .reasons
                .iter()
                .any(|reason| reason.contains("exact name")),
            "agent boost must come from the exact-name mention: {:?}",
            researcher_hit.reasons
        );
        assert!(
            plan.skills.iter().all(|hit| hit.score < researcher_hit.score),
            "the same-named skill must not outrank the explicitly mentioned agent"
        );
    }

    #[test]
    fn cjk_agent_and_skill_both_mentioned_agent_wins() {
        let settings = SelectorSettings {
            autoload_threshold: 1,
            auto_select_threshold: 1,
            confidence_margin: 0,
            min_score: 0,
            ..SelectorSettings::default()
        };
        let research = skill("research", "Research topics for a researcher study");
        let researcher = agent("researcher", "Research and study assigned topics");
        let plan = select_deterministic(SelectionInput {
            request: "让researcher用research技能去调查",
            skills: &[research],
            agents: &[researcher],
            settings: &settings,
        });
        assert_eq!(plan.selected_agent.as_deref(), Some("researcher"));
        // The skill is still suggested alongside, but never wins.
        assert!(plan.skills.iter().any(|hit| hit.name == "research"));
        let researcher_hit = plan
            .agents
            .iter()
            .find(|hit| hit.name == "researcher")
            .expect("the mentioned agent must be ranked");
        assert!(plan.skills.iter().all(|hit| hit.score < researcher_hit.score));
        assert!(
            researcher_hit
                .reasons
                .iter()
                .any(|reason| reason.contains("outranks skill")),
            "mention boost must be explainable: {:?}",
            researcher_hit.reasons
        );
    }

    #[test]
    fn cjk_skill_mention_suggests_skill_when_no_agent_mentioned() {
        let settings = SelectorSettings {
            autoload_threshold: 1,
            min_score: 0,
            ..SelectorSettings::default()
        };
        let research = skill("research", "Research assigned topics");
        let researcher = agent("researcher", "Study assigned topics");
        let request = "请使用research技能来调查";
        assert!(matches!(
            exact_skill_mention(request, &[research.clone()]),
            ExactSkillMention::Unique(name) if name == "research"
        ));
        assert!(matches!(
            exact_agent_mention(request, &[researcher.clone()]),
            ExactAgentMention::None
        ));
        let plan = select_deterministic(SelectionInput {
            request,
            skills: &[research],
            agents: &[researcher],
            settings: &settings,
        });
        assert_eq!(plan.skills[0].name, "research");
        assert_eq!(plan.autoload_skills, vec!["research"]);
        assert!(plan.selected_agent.is_none());
    }

    #[test]
    fn cjk_prompt_without_agent_or_skill_mention_is_unchanged() {
        let research = skill("research", "Research assigned topics");
        let researcher = agent("researcher", "Study assigned topics");
        let plan = select_deterministic(SelectionInput {
            request: "请调查一下这个项目的技术栈",
            skills: &[research],
            agents: &[researcher],
            settings: &SelectorSettings::default(),
        });
        assert!(plan.selected_agent.is_none());
        assert!(plan.skills.is_empty());
        assert!(plan.agents.is_empty());
        assert_eq!(
            plan.fallback_reason.as_deref(),
            Some("no metadata match met the threshold")
        );
    }

    #[test]
    fn skill_uri_rejects_traversal_but_allows_hidden() {
        let root = TempDir::new().unwrap();
        let base = root.path().join("hidden");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("SKILL.md"), "body").unwrap();
        std::fs::write(base.join("asset.txt"), "asset").unwrap();
        let mut hidden = skill("hidden", "hidden skill");
        hidden.base_dir = base.to_string_lossy().into_owned();
        hidden.file_path = base.join("SKILL.md").to_string_lossy().into_owned();
        hidden.hidden = true;
        assert_eq!(
            resolve_skill_uri("skill://hidden/asset.txt", &[hidden.clone()]).unwrap(),
            base.join("asset.txt").canonicalize().unwrap()
        );
        assert!(resolve_skill_uri("skill://hidden/../secret", &[hidden.clone()]).is_err());
        assert!(resolve_skill_uri("skill:///etc/passwd", &[hidden]).is_err());
    }

    #[tokio::test]
    async fn classifier_disabled_and_error_preserve_deterministic_ranking() {
        let skills = vec![skill("rust", "review Rust code")];
        let disabled = SelectorSettings::default();
        let plan = select(
            SelectionInput {
                request: "review Rust",
                skills: &skills,
                agents: &[],
                settings: &disabled,
            },
            None,
        )
        .await;
        assert!(!plan.classifier_used);
        assert_eq!(plan.skills[0].name, "rust");

        let stream: StreamFn = Arc::new(|model, _, _| {
            async move {
                let events = pi_ai::new_assistant_message_event_stream();
                let writer = events.clone();
                tokio::spawn(async move {
                    let mut message = AssistantMessage::pending(&model);
                    message.stop_reason = StopReason::Error;
                    message.error_message = Some("classifier unavailable".to_owned());
                    writer
                        .push(AssistantMessageEvent::Error {
                            reason: StopReason::Error,
                            error: message.clone(),
                        })
                        .await;
                    writer.end(Some(message)).await;
                });
                events
            }
            .boxed()
        });
        let (_, abort) = AbortController::new();
        let enabled = SelectorSettings {
            classifier: SelectorClassifierSettings {
                enabled: true,
                ..SelectorClassifierSettings::default()
            },
            ..SelectorSettings::default()
        };
        let classifier = ProviderClassifier {
            model: Model::default(),
            stream,
            options: SimpleStreamOptions::default(),
            abort,
        };
        let fallback = select(
            SelectionInput {
                request: "review Rust",
                skills: &skills,
                agents: &[],
                settings: &enabled,
            },
            Some(&classifier),
        )
        .await;
        assert!(!fallback.classifier_used);
        assert_eq!(fallback.skills[0].name, "rust");
        assert!(fallback.fallback_reason.as_deref().is_some_and(|reason| {
            reason.contains("classifier unavailable")
        }));
    }


    #[tokio::test]
    async fn classifier_reorders_only_trusted_deterministic_candidates() {
        let skills = vec![
            skill("rust", "review Rust code"),
            skill("docs", "review documentation"),
        ];
        let stream: StreamFn = Arc::new(|model, _, _| {
            async move {
                let events = pi_ai::new_assistant_message_event_stream();
                let writer = events.clone();
                tokio::spawn(async move {
                    let mut message = AssistantMessage::pending(&model);
                    message.content = vec![pi_ai::ContentBlock::text(
                        r#"[{"kind":"skill","name":"docs"},{"kind":"skill","name":"invented"}]"#,
                    )];
                    message.stop_reason = StopReason::Stop;
                    writer
                        .push(AssistantMessageEvent::Done {
                            reason: StopReason::Stop,
                            message: message.clone(),
                        })
                        .await;
                    writer.end(Some(message)).await;
                });
                events
            }
            .boxed()
        });
        let (_, abort) = AbortController::new();
        let settings = SelectorSettings {
            classifier: SelectorClassifierSettings {
                enabled: true,
                ..SelectorClassifierSettings::default()
            },
            ..SelectorSettings::default()
        };
        let classifier = ProviderClassifier {
            model: Model::default(),
            stream,
            options: SimpleStreamOptions::default(),
            abort,
        };
        let plan = select(
            SelectionInput {
                request: "review Rust documentation",
                skills: &skills,
                agents: &[],
                settings: &settings,
            },
            Some(&classifier),
        )
        .await;
        assert!(plan.classifier_used);
        assert_eq!(plan.skills[0].name, "docs");
        assert!(plan.skills.iter().all(|hit| hit.name != "invented"));
    }

    #[tokio::test]
    async fn classifier_ranking_research_first_never_overrides_exact_agent_selection() {
        let research = skill("research", "Research topics for a researcher study");
        let researcher = agent("researcher", "Research and study assigned topics");
        let stream: StreamFn = Arc::new(|model, _, _| {
            async move {
                let events = pi_ai::new_assistant_message_event_stream();
                let writer = events.clone();
                tokio::spawn(async move {
                    let mut message = AssistantMessage::pending(&model);
                    // The optional classifier ranks the overlapping SKILL
                    // first — it must only reorder the hit lists, never the
                    // deterministic exact-agent selection.
                    message.content = vec![pi_ai::ContentBlock::text(
                        r#"[{"kind":"skill","name":"research"},{"kind":"agent","name":"researcher"}]"#,
                    )];
                    message.stop_reason = StopReason::Stop;
                    writer
                        .push(AssistantMessageEvent::Done {
                            reason: StopReason::Stop,
                            message: message.clone(),
                        })
                        .await;
                    writer.end(Some(message)).await;
                });
                events
            }
            .boxed()
        });
        let (_, abort) = AbortController::new();
        let settings = SelectorSettings {
            min_score: 0,
            autoload_threshold: 1,
            auto_select_threshold: 1,
            confidence_margin: 0,
            classifier: SelectorClassifierSettings {
                enabled: true,
                ..SelectorClassifierSettings::default()
            },
            ..SelectorSettings::default()
        };
        let classifier = ProviderClassifier {
            model: Model::default(),
            stream,
            options: SimpleStreamOptions::default(),
            abort,
        };
        let plan = select(
            SelectionInput {
                request: "你让researcher去调查这个项目",
                skills: &[research],
                agents: &[researcher],
                settings: &settings,
            },
            Some(&classifier),
        )
        .await;
        assert!(plan.classifier_used);
        assert_eq!(
            plan.skills.first().map(|hit| hit.name.as_str()),
            Some("research"),
            "the classifier order applies to the skill hit list"
        );
        assert_eq!(
            plan.selected_agent.as_deref(),
            Some("researcher"),
            "the classifier must never override the deterministic exact-agent selection"
        );
        assert!(
            !plan.autoload_skills.iter().any(|name| name == "research"),
            "the exact agent mention must keep ranked skill autoload suppressed: {:?}",
            plan.autoload_skills
        );
    }
    #[test]
    fn rendered_plan_contains_event_state_explanation() {
        let plan = deterministic("review Rust", &[skill("rust", "review Rust code")]);
        let rendered = render_selection_prompt(&plan);
        assert!(rendered.contains("deterministic, trust-filtered rankings augment"));
        assert!(rendered.contains("<reason>"));
        assert!(rendered.contains("score=\""));
        assert!(rendered.contains("skill://rust"));
    }

    #[test]
    fn autoload_skill_bodies_enforce_file_and_turn_caps() {
        let root = TempDir::new().unwrap();
        let mut skills = Vec::new();
        let mut names = Vec::new();
        for index in 0..9 {
            let name = format!("skill-{index}");
            let base = root.path().join(&name);
            std::fs::create_dir_all(&base).unwrap();
            let file = base.join("SKILL.md");
            std::fs::write(&file, "x".repeat(crate::MAX_RESOURCE_FILE_BYTES as usize)).unwrap();
            let mut item = skill(&name, "autoload cap test");
            item.base_dir = base.to_string_lossy().into_owned();
            item.file_path = file.to_string_lossy().into_owned();
            skills.push(item);
            names.push(name);
        }
        let plan = SelectionPlan {
            autoload_skills: names,
            ..SelectionPlan::default()
        };
        let bodies = load_autoload_skill_bodies(&plan, &skills);
        assert!(bodies.iter().map(|(_, body)| body.len()).sum::<usize>()
            <= MAX_AUTOLOAD_SKILL_BYTES);
        assert!(bodies.len() < skills.len());

        let oversized_base = root.path().join("oversized");
        std::fs::create_dir_all(&oversized_base).unwrap();
        let oversized_file = oversized_base.join("SKILL.md");
        std::fs::write(
            &oversized_file,
            vec![b'x'; crate::MAX_RESOURCE_FILE_BYTES as usize + 1],
        )
        .unwrap();
        let mut oversized = skill("oversized", "oversized skill");
        oversized.base_dir = oversized_base.to_string_lossy().into_owned();
        oversized.file_path = oversized_file.to_string_lossy().into_owned();
        let oversized_plan = SelectionPlan {
            autoload_skills: vec!["oversized".to_owned()],
            ..SelectionPlan::default()
        };
        assert!(load_autoload_skill_bodies(&oversized_plan, &[oversized]).is_empty());
    }
}

#[cfg(test)]
mod classifier_tests {
    use crate::TodoStatus;

    use super::*;

    /// Deterministic fixture table for the auto-mode classifier. Every row
    /// pins a prompt to exactly one expected mode; changing a row means
    /// changing a user-visible classification contract.
    const FIXTURES: &[(&str, PromptMode)] = &[
        // Code tasks: task verb + strong verb, path, or code-domain evidence.
        ("implement a parser in src/lib.rs", PromptMode::CodeTask),
        ("build a web server", PromptMode::CodeTask),
        ("fix the bug in crates/pi-coding/src/session.rs", PromptMode::CodeTask),
        ("refactor the auth module", PromptMode::CodeTask),
        ("add tests for the todo tool", PromptMode::CodeTask),
        ("write documentation for the CLI", PromptMode::CodeTask),
        ("update src/main.rs to handle the new flag", PromptMode::CodeTask),
        ("debug the session run", PromptMode::CodeTask),
        ("migrate the database schema", PromptMode::CodeTask),
        ("implementing a new tool", PromptMode::CodeTask),
        ("refactored the parser into crates/pi-ai", PromptMode::CodeTask),
        ("create an API endpoint for the users table", PromptMode::CodeTask),
        ("port the CLI to Rust", PromptMode::CodeTask),
        // Questions win over code evidence.
        ("what is the best way to implement a parser", PromptMode::Question),
        ("why does the build fail in src/lib.rs", PromptMode::Question),
        ("explain how the selector works", PromptMode::Question),
        ("compare rust and go", PromptMode::Question),
        ("how to implement a parser", PromptMode::Question),
        ("can you fix the merge conflict for me", PromptMode::Question),
        ("what's the difference between steer and follow-up", PromptMode::Question),
        ("does the todo tool support dependencies", PromptMode::Question),
        ("help me understand this error", PromptMode::Question),
        ("", PromptMode::Question),
        ("hello", PromptMode::Question),
        // Goal phrasing outranks everything.
        ("goal: ship the release over the next month", PromptMode::Goal),
        ("my goal is to keep the codebase green", PromptMode::Goal),
        ("long-running: maintain the nightly pipeline", PromptMode::Goal),
        ("i want to achieve a stable API by next quarter", PromptMode::Goal),
        ("set a goal to reduce test flakiness", PromptMode::Goal),
        // Weak verb without code evidence stays a question.
        ("add this to my notes", PromptMode::Question),
        ("update me on the status", PromptMode::Question),
        ("remove the duplicate from the list", PromptMode::Question),
        // Agent-mention prompts are not code tasks: the auto-mode classifier
        // must never hijack them into a todo DAG or skill suggestion.
        ("你让researcher去调查这个项目", PromptMode::Question),
        ("researcher 去调查这个项目", PromptMode::Question),
    ];

    #[test]
    fn classify_prompt_fixture_table() {
        for (prompt, expected) in FIXTURES {
            let actual = classify_prompt(prompt);
            assert_eq!(
                actual, *expected,
                "classify_prompt({prompt:?}) = {actual:?}, expected {expected:?}"
            );
        }
    }

    #[test]
    fn classify_prompt_is_case_insensitive_and_trims() {
        assert_eq!(
            classify_prompt("  IMPLEMENT A PARSER IN SRC/LIB.RS  "),
            PromptMode::CodeTask
        );
        assert_eq!(classify_prompt("WHAT IS RUST?"), PromptMode::Question);
        assert_eq!(classify_prompt("Goal: ship"), PromptMode::Goal);
    }

    #[test]
    fn mode_hint_only_fires_for_code_task_and_goal() {
        assert_eq!(
            mode_hint(PromptMode::CodeTask),
            Some("Detected: code task — /todo to plan")
        );
        assert_eq!(
            mode_hint(PromptMode::Goal),
            Some("Detected: long-running goal — /goal create to track")
        );
        assert_eq!(mode_hint(PromptMode::Question), None);
    }

    #[test]
    fn prompt_mode_serde_uses_snake_case_names() {
        assert_eq!(serde_json::to_string(&PromptMode::CodeTask).unwrap(), "\"code_task\"");
        assert_eq!(serde_json::to_string(&PromptMode::Question).unwrap(), "\"question\"");
        assert_eq!(serde_json::to_string(&PromptMode::Goal).unwrap(), "\"goal\"");
        assert_eq!(
            serde_json::from_str::<PromptMode>("\"code_task\"").unwrap(),
            PromptMode::CodeTask
        );
    }

    #[test]
    fn auto_mode_serde_uses_lowercase_names_and_defaults_to_suggest() {
        assert_eq!(serde_json::to_string(&AutoMode::Off).unwrap(), "\"off\"");
        assert_eq!(serde_json::to_string(&AutoMode::Suggest).unwrap(), "\"suggest\"");
        assert_eq!(serde_json::to_string(&AutoMode::Auto).unwrap(), "\"auto\"");
        assert_eq!(serde_json::from_str::<AutoMode>("\"auto\"").unwrap(), AutoMode::Auto);
        assert_eq!(AutoMode::default(), AutoMode::Suggest);
        assert!(AutoMode::Suggest.is_enabled());
        assert!(!AutoMode::Suggest.is_auto());
        assert!(AutoMode::Auto.is_auto());
        assert!(!AutoMode::Off.is_enabled());
    }

    #[test]
    fn selector_settings_round_trip_through_json_keeps_auto_mode() {
        let settings = SelectorSettings::default();
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["autoMode"], "suggest");
        let restored: SelectorSettings = serde_json::from_value(json).unwrap();
        assert_eq!(restored.auto_mode, AutoMode::Suggest);

        let mut auto = settings.clone();
        auto.auto_mode = AutoMode::Auto;
        let json = serde_json::to_value(&auto).unwrap();
        assert_eq!(json["autoMode"], "auto");
        let restored: SelectorSettings = serde_json::from_value(json).unwrap();
        assert_eq!(restored.auto_mode, AutoMode::Auto);
    }

    #[test]
    fn auto_create_todo_phases_seeds_one_executable_task() {
        let phases = auto_create_todo_phases("  implement a parser in src/lib.rs  ");
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].name, "Plan");
        assert_eq!(phases[0].tasks.len(), 1);
        let task = &phases[0].tasks[0];
        assert_eq!(task.content, "implement a parser in src/lib.rs");
        assert_eq!(task.status, TodoStatus::Pending);
        assert!(task.ready);
        assert!(task.depends_on.is_empty());
    }
}

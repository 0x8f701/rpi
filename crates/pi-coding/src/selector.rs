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

#[must_use]
pub fn select_deterministic(input: SelectionInput<'_>) -> SelectionPlan {
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
    let agents = rank_agents_with_tokens(
        &request_tokens,
        &request_phrase,
        input.agents,
        input.settings,
    );
    let selected_agent = select_default_agent(&agents, input.settings);
    let mut autoload_skills = Vec::new();
    let mut seen_autoload_skills = BTreeSet::new();
    for skill in input.skills.iter().filter(|skill| {
        skill.trusted && skill.always_apply && !skill.disable_model_invocation
    }) {
        if seen_autoload_skills.insert(skill.name.clone()) {
            autoload_skills.push(skill.name.clone());
        }
    }
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
    let fallback_reason = (skills.is_empty() && agents.is_empty())
        .then(|| "no metadata match met the threshold".to_owned());
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
    let mut plan = select_deterministic(input);
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
        let body = strip_frontmatter(&body);
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

fn strip_frontmatter(content: &str) -> String {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---"))
        .map_or_else(
            || normalized.clone(),
            |(_, body)| body.trim_start_matches("\n---").trim_start().to_owned(),
        )
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
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
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
    fn ranked_skills_autoload_in_rank_order_and_dedupe_agent_declarations() {
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
        assert_eq!(plan.autoload_skills, vec!["security-audit", "rust-review"]);
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

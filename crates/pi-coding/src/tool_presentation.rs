//! ID-stable tool presentation / projection.
//!
//! Pure projection over existing [`pi_agent::AgentEvent`] shapes. Valid call IDs
//! remain unchanged, while empty IDs receive deterministic projection-local IDs.
//! `ToolExecutionEnd` + subsequent `MessageEnd(ToolResult)` reconcile into one
//! record. No event-schema changes and no TUI wiring.

use std::collections::{HashMap, HashSet};
use std::fmt;

use pi_agent::{AgentEvent, AgentToolResult};
use pi_ai::{ContentBlock, Message, ToolResultMessage};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const SYNTHETIC_TOOL_CALL_ID_PREFIX: &str = "__pi_empty_tool_call_";
const PREVIEW_REDACTION_LOOKAHEAD_CHARS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallViewStatus {
    /// `ToolExecutionStart` observed; no partial content yet.
    Running,
    /// At least one `ToolExecutionUpdate` while still open.
    Streaming,
    /// Terminal success (`is_error = false`).
    Succeeded,
    /// Terminal failure that is not a cancel.
    Failed,
    /// Terminal cancel / abort residual.
    Cancelled,
    /// Result arrived without a prior live start (history / repair path).
    OrphanRepaired,
}

impl ToolCallViewStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::OrphanRepaired
        )
    }

    #[must_use]
    pub const fn is_partial(self) -> bool {
        matches!(self, Self::Running | Self::Streaming)
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Streaming => "streaming",
            Self::Succeeded => "ok",
            Self::Failed => "error",
            Self::Cancelled => "cancelled",
            Self::OrphanRepaired => "repaired",
        }
    }
}

#[derive(Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCard {
    pub tool_call_id: String,
    pub tool_name: String,
    /// Authoritative render/source ordinal when known, otherwise first-observation order.
    pub ordinal: u64,
    /// Stable arrival ordinal retained even when durable source order repairs rendering.
    #[serde(default)]
    pub first_observation_ordinal: u64,
    pub status: ToolCallViewStatus,
    pub arguments: Value,
    pub content: Vec<ContentBlock>,
    pub details: Value,
    pub is_error: bool,
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// True after a durable ToolResult message has been merged.
    pub has_message_result: bool,
    pub has_execution_end: bool,
}

impl Serialize for ToolCard {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct RedactedToolCard<'a> {
            tool_call_id: &'a str,
            tool_name: &'a str,
            ordinal: u64,
            first_observation_ordinal: u64,
            status: ToolCallViewStatus,
            arguments: Value,
            content: Vec<ContentBlock>,
            details: Value,
            is_error: bool,
            cancelled: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            error_message: Option<String>,
            has_message_result: bool,
            has_execution_end: bool,
        }

        RedactedToolCard {
            tool_call_id: &self.tool_call_id,
            tool_name: &self.tool_name,
            ordinal: self.ordinal,
            first_observation_ordinal: self.first_observation_ordinal,
            status: self.status,
            arguments: redact_value(&self.arguments),
            content: redact_content_blocks(&self.content),
            details: redact_value(&self.details),
            is_error: self.is_error,
            cancelled: self.cancelled,
            error_message: self.error_message.as_deref().map(redact_text),
            has_message_result: self.has_message_result,
            has_execution_end: self.has_execution_end,
        }
        .serialize(serializer)
    }
}

impl fmt::Debug for ToolCard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCard")
            .field("tool_call_id", &self.tool_call_id)
            .field("tool_name", &self.tool_name)
            .field("ordinal", &self.ordinal)
            .field("first_observation_ordinal", &self.first_observation_ordinal)
            .field("status", &self.status)
            .field("arguments", &redact_value(&self.arguments))
            .field("content_preview", &content_text_preview(&self.content, 80))
            .field("details", &redact_value(&self.details))
            .field("is_error", &self.is_error)
            .field("cancelled", &self.cancelled)
            .field(
                "error_message",
                &self.error_message.as_deref().map(redact_text),
            )
            .field("has_message_result", &self.has_message_result)
            .field("has_execution_end", &self.has_execution_end)
            .finish()
    }
}

impl ToolCard {
    #[must_use]
    pub fn is_partial(&self) -> bool {
        self.status.is_partial()
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    #[must_use]
    pub fn compact_title(&self) -> String {
        let args = compact_tool_arguments(&self.arguments);
        if args.is_empty() {
            format!("{} · {}", self.tool_name, self.status.label())
        } else {
            format!("{}({}) · {}", self.tool_name, args, self.status.label())
        }
    }

    #[must_use]
    pub fn compact_view(&self) -> ToolCompactView {
        ToolCompactView {
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            ordinal: self.ordinal,
            first_observation_ordinal: self.first_observation_ordinal,
            status: self.status,
            status_label: self.status.label().to_owned(),
            arguments_summary: compact_tool_arguments(&self.arguments),
            content_preview: content_text_preview(&self.content, 120),
            is_error: self.is_error,
            cancelled: self.cancelled,
            is_partial: self.is_partial(),
            has_details: !is_empty_details(&self.details),
        }
    }

    #[must_use]
    pub fn expanded_view(&self) -> ToolExpandedView {
        ToolExpandedView {
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            ordinal: self.ordinal,
            first_observation_ordinal: self.first_observation_ordinal,
            status: self.status,
            status_label: self.status.label().to_owned(),
            arguments: redact_value(&self.arguments),
            arguments_summary: compact_tool_arguments(&self.arguments),
            content_text: redact_text(&content_blocks_text(&self.content)),
            content: redact_content_blocks(&self.content),
            details: redact_value(&self.details),
            is_error: self.is_error,
            cancelled: self.cancelled,
            error_message: self.error_message.clone().map(|m| redact_text(&m)),
            is_partial: self.is_partial(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCompactView {
    pub tool_call_id: String,
    pub tool_name: String,
    pub ordinal: u64,
    #[serde(default)]
    pub first_observation_ordinal: u64,
    pub status: ToolCallViewStatus,
    pub status_label: String,
    pub arguments_summary: String,
    pub content_preview: String,
    pub is_error: bool,
    pub cancelled: bool,
    pub is_partial: bool,
    pub has_details: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExpandedView {
    pub tool_call_id: String,
    pub tool_name: String,
    pub ordinal: u64,
    #[serde(default)]
    pub first_observation_ordinal: u64,
    pub status: ToolCallViewStatus,
    pub status_label: String,
    pub arguments: Value,
    pub arguments_summary: String,
    pub content_text: String,
    pub content: Vec<ContentBlock>,
    pub details: Value,
    pub is_error: bool,
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub is_partial: bool,
}

/// ID-stable tool presentation state.
///
/// Apply agent events in arrival order. Visible ordering uses durable source order
/// when available and falls back to first-observation order until it is repaired.
#[derive(Clone, Default)]
pub struct ToolPresentationState {
    cards: HashMap<String, ToolCard>,
    /// tool_call_id insertion order (first-observation sequence).
    order: Vec<String>,
    authoritative_ids: HashSet<String>,
    synthetic_ids: HashSet<String>,
    next_observation_ordinal: u64,
    next_source_ordinal: u64,
    next_synthetic_id: u64,
}

impl fmt::Debug for ToolPresentationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolPresentationState")
            .field("count", &self.cards.len())
            .field("order", &self.order)
            .field(
                "cards",
                &self
                    .cards_in_source_order()
                    .into_iter()
                    .map(|card| {
                        (
                            card.tool_call_id.as_str(),
                            card.tool_name.as_str(),
                            card.status,
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl ToolPresentationState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.cards.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    #[must_use]
    pub fn get(&self, tool_call_id: &str) -> Option<&ToolCard> {
        self.cards.get(tool_call_id)
    }

    #[must_use]
    pub fn contains(&self, tool_call_id: &str) -> bool {
        self.cards.contains_key(tool_call_id)
    }

    #[must_use]
    pub fn cards_in_source_order(&self) -> Vec<&ToolCard> {
        let mut cards: Vec<&ToolCard> = self
            .order
            .iter()
            .filter_map(|id| self.cards.get(id))
            .collect();
        cards.sort_by_key(|card| card.ordinal);
        cards
    }

    #[must_use]
    pub fn compact_views(&self) -> Vec<ToolCompactView> {
        self.cards_in_source_order()
            .into_iter()
            .map(ToolCard::compact_view)
            .collect()
    }

    #[must_use]
    pub fn expanded_view(&self, tool_call_id: &str) -> Option<ToolExpandedView> {
        self.cards.get(tool_call_id).map(ToolCard::expanded_view)
    }

    #[must_use]
    pub fn expanded_views(&self) -> Vec<ToolExpandedView> {
        self.cards_in_source_order()
            .into_iter()
            .map(ToolCard::expanded_view)
            .collect()
    }

    /// Apply a raw agent event. Non-tool events are ignored.
    pub fn apply_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                arguments,
            } => self.apply_start(tool_call_id, tool_name, arguments.clone()),
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                arguments,
                partial_result,
            } => self.apply_update(tool_call_id, tool_name, arguments.clone(), partial_result),
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => self.apply_end(tool_call_id, tool_name, result, *is_error),
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
            } => self.apply_assistant_calls(&message.content),
            AgentEvent::MessageEnd {
                message: Message::ToolResult(result),
            } => self.apply_tool_result(result),
            AgentEvent::TurnEnd {
                message,
                tool_results,
            } => {
                self.apply_assistant_calls(&message.content);
                for result in tool_results {
                    self.apply_tool_result(result);
                }
            }
            _ => {}
        }
    }

    /// Start a running card (or refresh args/name if the id already exists).
    pub fn apply_start(&mut self, tool_call_id: &str, tool_name: &str, arguments: Value) {
        let id = if tool_call_id.is_empty() {
            self.new_synthetic_id()
        } else {
            self.resolve_valid_id(tool_call_id)
        };

        if let Some(card) = self.cards.get_mut(&id) {
            if !tool_name.is_empty() {
                card.tool_name = tool_name.to_owned();
            }
            if !arguments.is_null() {
                card.arguments = arguments;
            }
            if !card.status.is_terminal() && card.status != ToolCallViewStatus::Streaming {
                card.status = ToolCallViewStatus::Running;
            }
        } else {
            self.insert_new(
                &id,
                tool_name,
                arguments,
                ToolCallViewStatus::Running,
                Vec::new(),
                Value::Object(Map::new()),
                false,
                false,
                None,
                false,
                false,
            );
        }
        self.assign_source_ordinal(&id);
    }

    pub fn apply_update(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        arguments: Value,
        partial_result: &AgentToolResult,
    ) {
        let id = self.resolve_live_id(tool_call_id, tool_name);
        if let Some(card) = self.cards.get_mut(&id) {
            if card.status.is_terminal() {
                return;
            }
            if !tool_name.is_empty() {
                card.tool_name = tool_name.to_owned();
            }
            if !arguments.is_null() {
                card.arguments = arguments;
            }
            card.content = partial_result.content.clone();
            if !is_empty_details(&partial_result.details) {
                card.details = partial_result.details.clone();
            }
            card.status = ToolCallViewStatus::Streaming;
            return;
        }
        // Update without start: create streaming card (defensive).
        self.insert_new(
            &id,
            tool_name,
            arguments,
            ToolCallViewStatus::Streaming,
            partial_result.content.clone(),
            partial_result.details.clone(),
            false,
            false,
            None,
            false,
            false,
        );
    }

    /// Apply `ToolExecutionEnd`. Retains prior arguments when end args are absent
    /// (the event carries result only; args stay on the card from Start/Update).
    pub fn apply_end(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        result: &AgentToolResult,
        is_error: bool,
    ) {
        let id = self.resolve_live_id(tool_call_id, tool_name);
        let cancelled = detect_cancelled(is_error, &result.content, &result.details);
        let error_message = if is_error {
            Some(content_blocks_text(&result.content))
        } else {
            None
        };
        let status = terminal_status(is_error, cancelled, false);

        if let Some(card) = self.cards.get_mut(&id) {
            if !tool_name.is_empty() {
                card.tool_name = tool_name.to_owned();
            }
            // End is authoritative: empty final details clear streaming metadata.
            card.content = result.content.clone();
            card.details = result.details.clone();
            card.is_error = is_error;
            card.cancelled = cancelled;
            card.error_message = error_message;
            card.status = status;
            card.has_execution_end = true;
            return;
        }

        self.insert_new(
            &id,
            tool_name,
            Value::Null,
            status,
            result.content.clone(),
            result.details.clone(),
            is_error,
            cancelled,
            error_message,
            true,
            false,
        );
    }

    /// Reconcile a durable ToolResult message into the same id-keyed record.
    /// Never appends a second card when End already materialized the call.
    pub fn apply_tool_result(&mut self, result: &ToolResultMessage) {
        let id = self.resolve_result_id(&result.tool_call_id, &result.tool_name);
        let details = result
            .details
            .clone()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let cancelled = detect_cancelled(result.is_error, &result.content, &details);
        let error_message = if result.is_error {
            Some(content_blocks_text(&result.content))
        } else {
            None
        };

        if let Some(card) = self.cards.get_mut(&id) {
            if !result.tool_name.is_empty() {
                card.tool_name = result.tool_name.clone();
            }
            if !result.content.is_empty() {
                card.content = result.content.clone();
            }
            // Empty durable details clear streaming metadata, but an earlier End's
            // non-empty final details remain authoritative over absent replay data.
            if !card.has_execution_end || !is_empty_details(&details) {
                card.details = details;
            }
            card.is_error = result.is_error;
            card.cancelled = cancelled || card.cancelled;
            if error_message.is_some() {
                card.error_message = error_message;
            }
            if !card.has_execution_end {
                // Orphan / history-only path: mark terminal from the result alone.
                card.status = terminal_status(result.is_error, card.cancelled, true);
            } else if card.cancelled {
                card.status = ToolCallViewStatus::Cancelled;
            } else if card.is_error {
                card.status = ToolCallViewStatus::Failed;
            } else {
                card.status = ToolCallViewStatus::Succeeded;
            }
            card.has_message_result = true;
        } else {
            // Orphan result with no prior start/end.
            self.insert_new(
                &id,
                &result.tool_name,
                Value::Null,
                terminal_status(result.is_error, cancelled, true),
                result.content.clone(),
                details,
                result.is_error,
                cancelled,
                error_message,
                false,
                true,
            );
        }
        self.assign_source_ordinal(&id);
    }

    /// Clear all cards (e.g. new agent turn boundary if the consumer wants a reset).
    pub fn clear(&mut self) {
        self.cards.clear();
        self.order.clear();
        self.authoritative_ids.clear();
        self.synthetic_ids.clear();
        self.next_observation_ordinal = 0;
        self.next_source_ordinal = 0;
        self.next_synthetic_id = 0;
    }

    fn apply_assistant_calls(&mut self, content: &[ContentBlock]) {
        for block in content {
            if let ContentBlock::ToolCall(call) = block {
                self.apply_start(&call.id, &call.name, call.arguments.clone());
            }
        }
    }

    fn assign_source_ordinal(&mut self, tool_call_id: &str) {
        if !self.cards.contains_key(tool_call_id)
            || !self.authoritative_ids.insert(tool_call_id.to_owned())
        {
            return;
        }
        let ordinal = self.next_source_ordinal;
        self.next_source_ordinal = self.next_source_ordinal.saturating_add(1);
        for card in self.cards.values_mut() {
            if !self.authoritative_ids.contains(&card.tool_call_id) {
                card.ordinal = card.ordinal.saturating_add(1);
            }
        }
        if let Some(card) = self.cards.get_mut(tool_call_id) {
            card.ordinal = ordinal;
        }
    }

    fn resolve_live_id(&mut self, tool_call_id: &str, tool_name: &str) -> String {
        if !tool_call_id.is_empty() {
            return self.resolve_valid_id(tool_call_id);
        }
        self.unique_synthetic_match(tool_name, |card| !card.status.is_terminal())
            .unwrap_or_else(|| self.new_synthetic_id())
    }

    fn resolve_result_id(&mut self, tool_call_id: &str, tool_name: &str) -> String {
        if !tool_call_id.is_empty() {
            return self.resolve_valid_id(tool_call_id);
        }
        self.unique_synthetic_match(tool_name, |card| !card.has_message_result)
            .unwrap_or_else(|| self.new_synthetic_id())
    }

    fn resolve_valid_id(&mut self, tool_call_id: &str) -> String {
        if !self.synthetic_ids.contains(tool_call_id) {
            return tool_call_id.to_owned();
        }
        let Some(mut synthetic_card) = self.cards.remove(tool_call_id) else {
            self.synthetic_ids.remove(tool_call_id);
            return tool_call_id.to_owned();
        };

        self.synthetic_ids.remove(tool_call_id);
        let replacement = self.new_synthetic_id();
        synthetic_card.tool_call_id = replacement.clone();
        self.cards.insert(replacement.clone(), synthetic_card);
        if let Some(id) = self.order.iter_mut().find(|id| id.as_str() == tool_call_id) {
            *id = replacement.clone();
        }
        if self.authoritative_ids.remove(tool_call_id) {
            self.authoritative_ids.insert(replacement);
        }
        tool_call_id.to_owned()
    }

    fn unique_synthetic_match(
        &self,
        tool_name: &str,
        predicate: impl Fn(&ToolCard) -> bool,
    ) -> Option<String> {
        let mut matches = self.synthetic_ids.iter().filter_map(|id| {
            let card = self.cards.get(id)?;
            (card.tool_name == tool_name && predicate(card)).then_some(card)
        });
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(first.tool_call_id.clone())
    }

    fn new_synthetic_id(&mut self) -> String {
        loop {
            let ordinal = self.next_synthetic_id;
            self.next_synthetic_id = self.next_synthetic_id.saturating_add(1);
            let id = format!("{SYNTHETIC_TOOL_CALL_ID_PREFIX}{ordinal}");
            if !self.cards.contains_key(&id) {
                self.synthetic_ids.insert(id.clone());
                return id;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_new(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        arguments: Value,
        status: ToolCallViewStatus,
        content: Vec<ContentBlock>,
        details: Value,
        is_error: bool,
        cancelled: bool,
        error_message: Option<String>,
        has_execution_end: bool,
        has_message_result: bool,
    ) {
        let first_observation_ordinal = self.next_observation_ordinal;
        self.next_observation_ordinal = self.next_observation_ordinal.saturating_add(1);
        let id = tool_call_id.to_owned();
        self.order.push(id.clone());
        self.cards.insert(
            id.clone(),
            ToolCard {
                tool_call_id: id,
                tool_name: tool_name.to_owned(),
                ordinal: first_observation_ordinal,
                first_observation_ordinal,
                status,
                arguments,
                content,
                details,
                is_error,
                cancelled,
                error_message,
                has_message_result,
                has_execution_end,
            },
        );
    }
}

fn terminal_status(is_error: bool, cancelled: bool, orphan: bool) -> ToolCallViewStatus {
    if orphan && !is_error && !cancelled {
        // Successful orphan is still a repaired/history path marker.
        return ToolCallViewStatus::OrphanRepaired;
    }
    if cancelled {
        ToolCallViewStatus::Cancelled
    } else if is_error {
        ToolCallViewStatus::Failed
    } else if orphan {
        ToolCallViewStatus::OrphanRepaired
    } else {
        ToolCallViewStatus::Succeeded
    }
}

fn detect_cancelled(is_error: bool, content: &[ContentBlock], details: &Value) -> bool {
    if details
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    if details
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| {
            status.eq_ignore_ascii_case("cancelled") || status.eq_ignore_ascii_case("canceled")
        })
    {
        return true;
    }
    if !is_error {
        return false;
    }
    let text = content_blocks_text(content).to_ascii_lowercase();
    text.contains("operation aborted")
        || text.contains("cancelled")
        || text.contains("canceled")
        || text.contains("tool call cancelled")
}

/// Compact one-line argument summary for titles. Secret-bearing keys are omitted.
#[must_use]
pub fn compact_tool_arguments(arguments: &Value) -> String {
    let redacted = redact_value(arguments);
    let preferred = ["command", "path", "pattern", "query", "file", "glob", "url"]
        .into_iter()
        .find_map(|key| redacted.get(key).and_then(value_as_display_str));

    let raw = if let Some(value) = preferred {
        value
    } else {
        match &redacted {
            Value::Null => String::new(),
            Value::Object(map) if map.is_empty() => String::new(),
            Value::Object(map) => {
                let mut parts = Vec::new();
                for (key, value) in map.iter().take(3) {
                    if is_sensitive_key(key) {
                        continue;
                    }
                    let rendered = value_as_display_str(value).unwrap_or_else(|| short_json(value));
                    if rendered.is_empty() {
                        continue;
                    }
                    parts.push(format!("{key}={rendered}"));
                }
                parts.join(", ")
            }
            other => short_json(other),
        }
    };

    truncate_chars(&raw, 60)
}

#[must_use]
pub fn content_blocks_text(content: &[ContentBlock]) -> String {
    content.iter().fold(String::new(), |mut output, block| {
        if let ContentBlock::Text { text, .. } = block {
            output.push_str(text);
        }
        output
    })
}

fn content_text_preview(content: &[ContentBlock], max_chars: usize) -> String {
    bounded_text_preview(
        content.iter().filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        }),
        max_chars,
    )
}

fn bounded_text_preview<'a>(parts: impl IntoIterator<Item = &'a str>, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let accumulation_limit = max_chars.saturating_add(PREVIEW_REDACTION_LOOKAHEAD_CHARS);
    let mut accumulated = String::new();
    let mut accumulated_chars = 0usize;
    'parts: for part in parts {
        for ch in part.chars() {
            if accumulated_chars == accumulation_limit {
                break 'parts;
            }
            accumulated.push(ch);
            accumulated_chars += 1;
        }
        if accumulated_chars == accumulation_limit {
            break;
        }
    }
    truncate_chars(&redact_text(&accumulated), max_chars)
}

fn is_empty_details(details: &Value) -> bool {
    match details {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

fn value_as_display_str(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn short_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_owned();
    }
    if max_chars <= 3 {
        return input.chars().take(max_chars).collect();
    }
    let prefix: String = input.chars().take(max_chars - 3).collect();
    format!("{prefix}...")
}

/// Redact secret-looking object keys for view / debug surfaces.
#[must_use]
pub fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, child) in map {
                if is_sensitive_key(key) {
                    out.insert(key.clone(), Value::String("[REDACTED]".to_owned()));
                } else {
                    out.insert(key.clone(), redact_value(child));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::String(s) => Value::String(redact_text(s)),
        other => other.clone(),
    }
}

fn redact_text(text: &str) -> String {
    // Strip common credential shapes from free text so debug/error views stay safe.
    let mut out = text.to_owned();
    for (pattern, replacement) in [
        (
            r#"(?i)\b(AWS_(?:ACCESS_KEY_ID|SECRET_ACCESS_KEY|SESSION_TOKEN))\s*[:=]\s*(?:"[^"]*"|'[^']*'|\S+)"#,
            "$1=[REDACTED]",
        ),
        (
            r"(?i)(api[_-]?key|token|secret|password|authorization)\s*[:=]\s*(?:bearer\s+)?\S+",
            "$1=[REDACTED]",
        ),
        (r"(?i)bearer\s+[a-z0-9._\-]+", "Bearer [REDACTED]"),
        (r"\bAKIA[0-9A-Z]{16}\b", "[REDACTED]"),
        // Long opaque tokens that look like API keys (sk-..., ghp_..., etc.).
        (
            r"(?i)\b(?:sk|pk|rk|ghp|gho|ghu|ghs|ghr|xox[baprs])[-_][A-Za-z0-9\-_]{8,}\b",
            "[REDACTED]",
        ),
    ] {
        if let Ok(re) = regex::Regex::new(pattern) {
            out = re.replace_all(&out, replacement).into_owned();
        }
    }
    out
}

fn redact_content_blocks(content: &[ContentBlock]) -> Vec<ContentBlock> {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text {
                text,
                text_signature,
            } => ContentBlock::Text {
                text: redact_text(text),
                text_signature: text_signature.clone(),
            },
            ContentBlock::Thinking {
                thinking,
                thinking_signature,
                redacted,
            } => ContentBlock::Thinking {
                thinking: redact_text(thinking),
                thinking_signature: thinking_signature.clone(),
                redacted: *redacted,
            },
            ContentBlock::Image { .. } => block.clone(),
            ContentBlock::ToolCall(call) => ContentBlock::ToolCall(pi_ai::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: redact_value(&call.arguments),
                thought_signature: call.thought_signature.clone(),
            }),
        })
        .collect()
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    matches!(
        normalized.as_str(),
        "password"
            | "passwd"
            | "secret"
            | "token"
            | "apikey"
            | "apisecret"
            | "accesskey"
            | "accesskeyid"
            | "awsaccesskeyid"
            | "awssecretaccesskey"
            | "awssessiontoken"
            | "secretaccesskey"
            | "sessiontoken"
            | "authorization"
            | "auth"
            | "credential"
            | "credentials"
            | "privatekey"
            | "clientsecret"
            | "refreshtoken"
            | "idtoken"
            | "cookie"
    ) || normalized.contains("password")
        || normalized.contains("secret")
        || normalized.ends_with("token")
        || normalized.ends_with("apikey")
        || normalized.contains("privatekey")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::now_millis;
    use serde_json::json;

    fn text_result(text: &str) -> AgentToolResult {
        AgentToolResult::text(text)
    }

    fn result_with_details(text: &str, details: Value) -> AgentToolResult {
        let mut result = AgentToolResult::text(text);
        result.details = details;
        result
    }

    fn tool_result_message(
        id: &str,
        name: &str,
        text: &str,
        is_error: bool,
        details: Option<Value>,
    ) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: id.to_owned(),
            tool_name: name.to_owned(),
            content: vec![ContentBlock::text(text)],
            usage: None,
            details,
            added_tool_names: Vec::new(),
            is_error,
            timestamp: now_millis(),
        }
    }

    fn start(id: &str, name: &str, args: Value) -> AgentEvent {
        AgentEvent::ToolExecutionStart {
            tool_call_id: id.to_owned(),
            tool_name: name.to_owned(),
            arguments: args,
        }
    }

    fn update(id: &str, name: &str, args: Value, partial: AgentToolResult) -> AgentEvent {
        AgentEvent::ToolExecutionUpdate {
            tool_call_id: id.to_owned(),
            tool_name: name.to_owned(),
            arguments: args,
            partial_result: partial,
        }
    }

    fn end(id: &str, name: &str, result: AgentToolResult, is_error: bool) -> AgentEvent {
        AgentEvent::ToolExecutionEnd {
            tool_call_id: id.to_owned(),
            tool_name: name.to_owned(),
            result,
            is_error,
        }
    }

    fn message_end_result(result: ToolResultMessage) -> AgentEvent {
        AgentEvent::MessageEnd {
            message: Message::ToolResult(result),
        }
    }

    #[test]
    fn two_concurrent_same_name_calls_remain_distinct() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("a", "read", json!({"path": "one.rs"})));
        state.apply_event(&start("b", "read", json!({"path": "two.rs"})));
        state.apply_event(&update(
            "a",
            "read",
            json!({"path": "one.rs"}),
            text_result("partial-a"),
        ));
        state.apply_event(&update(
            "b",
            "read",
            json!({"path": "two.rs"}),
            text_result("partial-b"),
        ));

        assert_eq!(state.len(), 2);
        let a = state.get("a").expect("card a");
        let b = state.get("b").expect("card b");
        assert_eq!(a.tool_name, "read");
        assert_eq!(b.tool_name, "read");
        assert_eq!(a.status, ToolCallViewStatus::Streaming);
        assert_eq!(b.status, ToolCallViewStatus::Streaming);
        assert_eq!(content_blocks_text(&a.content), "partial-a");
        assert_eq!(content_blocks_text(&b.content), "partial-b");
        assert_ne!(a.tool_call_id, b.tool_call_id);
    }

    #[test]
    fn out_of_order_completion_preserves_source_order_view() {
        let mut state = ToolPresentationState::new();
        // Source order: first, second.
        state.apply_event(&start("first", "bash", json!({"command": "echo 1"})));
        state.apply_event(&start("second", "bash", json!({"command": "echo 2"})));
        // Completion order reversed.
        state.apply_event(&end("second", "bash", text_result("out-2"), false));
        state.apply_event(&end("first", "bash", text_result("out-1"), false));

        let views = state.compact_views();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].tool_call_id, "first");
        assert_eq!(views[1].tool_call_id, "second");
        assert_eq!(views[0].ordinal, 0);
        assert_eq!(views[1].ordinal, 1);
        assert_eq!(views[0].status, ToolCallViewStatus::Succeeded);
        assert_eq!(views[1].status, ToolCallViewStatus::Succeeded);
        assert!(views[0].ordinal < views[1].ordinal);
    }

    #[test]
    fn multiple_updates_replace_partial_content() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("c1", "bash", json!({"command": "long"})));
        state.apply_event(&update(
            "c1",
            "bash",
            json!({"command": "long"}),
            text_result("line1\n"),
        ));
        state.apply_event(&update(
            "c1",
            "bash",
            json!({"command": "long"}),
            text_result("line1\nline2\n"),
        ));
        state.apply_event(&update(
            "c1",
            "bash",
            json!({"command": "long"}),
            text_result("line1\nline2\nline3\n"),
        ));

        let card = state.get("c1").expect("card");
        assert_eq!(card.status, ToolCallViewStatus::Streaming);
        assert_eq!(content_blocks_text(&card.content), "line1\nline2\nline3\n");
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn duplicate_end_and_message_result_reconcile_same_record() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&start(
            "x",
            "edit",
            json!({"path": "f.rs", "oldText": "a", "newText": "b"}),
        ));
        state.apply_event(&end(
            "x",
            "edit",
            result_with_details("ok", json!({"diff": "--- a\n+++ b\n", "path": "f.rs"})),
            false,
        ));
        assert_eq!(state.len(), 1);
        assert!(state.get("x").unwrap().has_execution_end);
        assert!(!state.get("x").unwrap().has_message_result);

        // Duplicate MessageEnd ToolResult must update, never append.
        state.apply_event(&message_end_result(tool_result_message(
            "x",
            "edit",
            "ok",
            false,
            Some(json!({"diff": "--- a\n+++ b\n", "path": "f.rs"})),
        )));
        assert_eq!(state.len(), 1, "must not append a second card");
        let card = state.get("x").expect("same card");
        assert!(card.has_execution_end);
        assert!(card.has_message_result);
        assert_eq!(card.status, ToolCallViewStatus::Succeeded);
        assert_eq!(content_blocks_text(&card.content), "ok");
        // Arguments retained from Start through End (End event has no args field).
        assert_eq!(card.arguments["path"], "f.rs");
    }

    #[test]
    fn end_with_null_style_absence_retains_start_arguments() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("keep", "read", json!({"path": "src/main.rs"})));
        state.apply_event(&end("keep", "read", text_result("file body"), false));
        let card = state.get("keep").unwrap();
        assert_eq!(card.arguments["path"], "src/main.rs");
        assert!(card.compact_title().contains("src/main.rs"));
    }

    #[test]
    fn error_and_cancel_are_distinct_statuses() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("err", "bash", json!({"command": "false"})));
        state.apply_event(&end(
            "err",
            "bash",
            text_result("command failed: exit 1"),
            true,
        ));
        assert_eq!(state.get("err").unwrap().status, ToolCallViewStatus::Failed);
        assert!(state.get("err").unwrap().is_error);
        assert!(!state.get("err").unwrap().cancelled);

        state.apply_event(&start("cx", "bash", json!({"command": "sleep 99"})));
        state.apply_event(&end("cx", "bash", text_result("Operation aborted"), true));
        let cancel = state.get("cx").unwrap();
        assert_eq!(cancel.status, ToolCallViewStatus::Cancelled);
        assert!(cancel.cancelled);
        assert!(cancel.is_error);

        // details.cancelled also marks cancel.
        state.apply_event(&start("cy", "read", json!({"path": "x"})));
        state.apply_event(&end(
            "cy",
            "read",
            result_with_details("stopped", json!({"cancelled": true})),
            true,
        ));
        assert_eq!(
            state.get("cy").unwrap().status,
            ToolCallViewStatus::Cancelled
        );
    }

    #[test]
    fn orphan_result_creates_terminal_repaired_card() {
        let mut state = ToolPresentationState::new();
        state.apply_tool_result(&tool_result_message(
            "orphan-1",
            "read",
            "No result provided",
            true,
            None,
        ));
        assert_eq!(state.len(), 1);
        let card = state.get("orphan-1").unwrap();
        assert!(card.has_message_result);
        assert!(!card.has_execution_end);
        assert!(card.is_error);
        assert_eq!(card.status, ToolCallViewStatus::Failed);

        state.apply_tool_result(&tool_result_message(
            "orphan-ok",
            "grep",
            "matches",
            false,
            Some(json!({"count": 2})),
        ));
        let ok = state.get("orphan-ok").unwrap();
        assert_eq!(ok.status, ToolCallViewStatus::OrphanRepaired);
        assert!(!ok.is_error);
        assert_eq!(ok.details["count"], 2);
    }

    #[test]
    fn structured_details_surface_in_expanded_view() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("d1", "bash", json!({"command": "cat big"})));
        state.apply_event(&end(
            "d1",
            "bash",
            result_with_details(
                "truncated body",
                json!({
                    "truncated": true,
                    "fullOutputPath": "/tmp/spill/out.txt",
                    "totalLines": 9000
                }),
            ),
            false,
        ));
        let expanded = state.expanded_view("d1").expect("expanded");
        assert_eq!(expanded.details["truncated"], true);
        assert_eq!(expanded.details["fullOutputPath"], "/tmp/spill/out.txt");
        assert_eq!(expanded.details["totalLines"], 9000);
        assert_eq!(expanded.content_text, "truncated body");
        let compact = state.get("d1").unwrap().compact_view();
        assert!(compact.has_details);
    }

    #[test]
    fn views_do_not_leak_secrets_or_debug_payloads() {
        let mut state = ToolPresentationState::new();
        let secret = ["s", "k-", "live-super-secret-value-do-not-leak"].concat();
        state.apply_event(&start(
            "sec",
            "http",
            json!({
                "url": "https://api.example/v1",
                "api_key": secret,
                "authorization": format!("Bearer {secret}"),
                "password": "hunter2",
                "token": secret,
            }),
        ));
        state.apply_event(&end(
            "sec",
            "http",
            result_with_details(
                &format!("auth failed for token={secret}"),
                json!({"debug": {"apiKey": secret, "trace": "internal"}}),
            ),
            true,
        ));

        let compact = state.get("sec").unwrap().compact_view();
        assert!(!compact.arguments_summary.contains(secret.as_str()));
        assert!(!compact.arguments_summary.contains("hunter2"));
        assert!(!compact.content_preview.contains(secret.as_str()));

        let expanded = state.expanded_view("sec").unwrap();
        let args_dump = serde_json::to_string(&expanded.arguments).unwrap();
        let details_dump = serde_json::to_string(&expanded.details).unwrap();
        assert!(!args_dump.contains(secret.as_str()));
        assert!(!details_dump.contains(secret.as_str()));
        assert!(args_dump.contains("[REDACTED]"));
        assert!(details_dump.contains("[REDACTED]"));
        if let Some(err) = &expanded.error_message {
            assert!(!err.contains(secret.as_str()));
        }

        let debug = format!("{:?}", state.get("sec").unwrap());
        assert!(!debug.contains(secret.as_str()), "Debug leaked secret: {debug}");
        assert!(!debug.contains("hunter2"), "Debug leaked password: {debug}");

        let state_debug = format!("{state:?}");
        assert!(!state_debug.contains(secret.as_str()));
    }

    #[test]
    fn message_end_tool_result_reconciles_not_message_start() {
        // CONTRACT: only MessageEnd(ToolResult) is durable. MessageStart is ignored
        // so a lost End cannot prematurely mark has_message_result.
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("m", "read", json!({"path": "a"})));
        state.apply_event(&end("m", "read", text_result("body"), false));

        state.apply_event(&AgentEvent::MessageStart {
            message: Message::ToolResult(tool_result_message(
                "m",
                "read",
                "start-preview",
                false,
                Some(json!({"bytes": 99})),
            )),
        });
        assert_eq!(state.len(), 1);
        let after_start = state.get("m").unwrap();
        assert!(
            !after_start.has_message_result,
            "MessageStart must not set durable has_message_result"
        );
        assert_eq!(content_blocks_text(&after_start.content), "body");
        assert!(after_start.details.get("bytes").is_none());

        state.apply_event(&message_end_result(tool_result_message(
            "m",
            "read",
            "body",
            false,
            Some(json!({"bytes": 4})),
        )));
        assert_eq!(state.len(), 1);
        let card = state.get("m").unwrap();
        assert!(card.has_message_result);
        assert_eq!(card.details["bytes"], 4);
        assert_eq!(content_blocks_text(&card.content), "body");
    }

    #[test]
    fn non_tool_events_are_ignored() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&AgentEvent::AgentStart);
        state.apply_event(&AgentEvent::TurnStart);
        assert!(state.is_empty());
    }

    #[test]
    fn same_id_start_end_start_end_overwrites_single_card() {
        // Identity is tool_call_id: a second full lifecycle reuses the card.
        // Consumers that span turns without clear() will see overwrite, not a fork.
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("reuse", "read", json!({"path": "first.rs"})));
        state.apply_event(&end("reuse", "read", text_result("body-1"), false));
        assert_eq!(state.len(), 1);
        assert_eq!(
            state.get("reuse").unwrap().status,
            ToolCallViewStatus::Succeeded
        );
        assert_eq!(state.get("reuse").unwrap().ordinal, 0);
        assert_eq!(
            content_blocks_text(&state.get("reuse").unwrap().content),
            "body-1"
        );

        // Second turn, same id, no clear(): still one card; terminal fields refresh.
        state.apply_event(&start("reuse", "read", json!({"path": "second.rs"})));
        // Late start after terminal must keep the prior terminal outcome.
        assert_eq!(
            state.get("reuse").unwrap().status,
            ToolCallViewStatus::Succeeded,
            "late Start must not reopen a terminal card"
        );
        assert_eq!(state.get("reuse").unwrap().arguments["path"], "second.rs");

        state.apply_event(&end(
            "reuse",
            "read",
            result_with_details("body-2", json!({"turn": 2})),
            false,
        ));
        assert_eq!(state.len(), 1, "same id must never fork a second card");
        let card = state.get("reuse").unwrap();
        assert_eq!(card.ordinal, 0, "ordinal stays first-observation");
        assert_eq!(card.status, ToolCallViewStatus::Succeeded);
        assert_eq!(content_blocks_text(&card.content), "body-2");
        assert_eq!(card.details["turn"], 2);
        assert_eq!(card.arguments["path"], "second.rs");
        assert!(card.has_execution_end);
    }

    #[test]
    fn update_without_start_creates_streaming_card() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&update(
            "orphan-up",
            "bash",
            json!({"command": "stream"}),
            text_result("partial-only"),
        ));
        assert_eq!(state.len(), 1);
        let card = state.get("orphan-up").unwrap();
        assert_eq!(card.status, ToolCallViewStatus::Streaming);
        assert!(card.is_partial());
        assert!(!card.is_terminal());
        assert_eq!(content_blocks_text(&card.content), "partial-only");
        assert_eq!(card.arguments["command"], "stream");
        assert_eq!(card.ordinal, 0);
    }

    #[test]
    fn empty_tool_result_content_does_not_clobber_end_body() {
        // MessageEnd may carry empty content; prior End body must remain.
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("keep-body", "bash", json!({"command": "echo hi"})));
        state.apply_event(&end(
            "keep-body",
            "bash",
            result_with_details("final body", json!({"exitCode": 0})),
            false,
        ));
        let empty = ToolResultMessage {
            tool_call_id: "keep-body".to_owned(),
            tool_name: "bash".to_owned(),
            content: Vec::new(),
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: now_millis(),
        };
        state.apply_event(&message_end_result(empty));

        let card = state.get("keep-body").unwrap();
        assert_eq!(state.len(), 1);
        assert!(card.has_message_result);
        assert_eq!(content_blocks_text(&card.content), "final body");
        assert_eq!(card.details["exitCode"], 0);
        assert_eq!(card.status, ToolCallViewStatus::Succeeded);
    }

    #[test]
    fn independent_is_partial_allows_unrelated_terminal_commit() {
        // Commit seam must be per-card: one live tool must not mark another terminal.
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("done", "read", json!({"path": "a"})));
        state.apply_event(&start("live", "read", json!({"path": "b"})));
        state.apply_event(&end("done", "read", text_result("a-body"), false));
        state.apply_event(&update(
            "live",
            "read",
            json!({"path": "b"}),
            text_result("streaming-b"),
        ));

        let views = state.compact_views();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].tool_call_id, "done");
        assert_eq!(views[1].tool_call_id, "live");
        assert!(!views[0].is_partial);
        assert!(views[1].is_partial);
        assert_eq!(views[0].status, ToolCallViewStatus::Succeeded);
        assert_eq!(views[1].status, ToolCallViewStatus::Streaming);

        let expanded = state.expanded_views();
        assert_eq!(expanded[0].tool_call_id, "done");
        assert!(!expanded[0].is_partial);
        assert!(expanded[1].is_partial);
        assert_eq!(expanded[1].content_text, "streaming-b");
    }

    #[test]
    fn details_status_canceled_marks_cancelled() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("cz", "bash", json!({"command": "sleep 1"})));
        state.apply_event(&end(
            "cz",
            "bash",
            result_with_details("stopped", json!({"status": "canceled"})),
            true,
        ));
        let card = state.get("cz").unwrap();
        assert_eq!(card.status, ToolCallViewStatus::Cancelled);
        assert!(card.cancelled);
        assert_eq!(card.status.label(), "cancelled");
        assert!(card.is_terminal());
    }

    #[test]
    fn end_without_prior_start_creates_terminal_card() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&end(
            "end-only",
            "grep",
            result_with_details("hits", json!({"count": 3})),
            false,
        ));
        assert_eq!(state.len(), 1);
        let card = state.get("end-only").unwrap();
        assert!(card.has_execution_end);
        assert!(!card.has_message_result);
        assert_eq!(card.status, ToolCallViewStatus::Succeeded);
        assert_eq!(content_blocks_text(&card.content), "hits");
        assert_eq!(card.details["count"], 3);
        assert!(card.arguments.is_null());
    }

    #[test]
    fn late_start_after_orphan_result_keeps_repaired_outcome() {
        let mut state = ToolPresentationState::new();
        state.apply_tool_result(&tool_result_message(
            "late",
            "bash",
            "already settled",
            false,
            Some(json!({"from": "history"})),
        ));
        assert_eq!(
            state.get("late").unwrap().status,
            ToolCallViewStatus::OrphanRepaired
        );

        state.apply_event(&start(
            "late",
            "bash",
            json!({"command": "echo should-not-reopen"}),
        ));
        let card = state.get("late").unwrap();
        assert_eq!(state.len(), 1);
        assert_eq!(card.status, ToolCallViewStatus::OrphanRepaired);
        assert!(card.is_terminal());
        assert!(!card.is_partial());
        assert_eq!(content_blocks_text(&card.content), "already settled");
        assert_eq!(card.arguments["command"], "echo should-not-reopen");
        assert_eq!(card.details["from"], "history");
    }

    #[test]
    fn clear_resets_cards_and_source_ordinals() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("a", "read", json!({"path": "1"})));
        state.apply_event(&start("b", "read", json!({"path": "2"})));
        assert_eq!(state.len(), 2);
        state.clear();
        assert!(state.is_empty());
        assert!(!state.contains("a"));
        // Fresh ordinals after clear — required for turn-boundary reuse.
        state.apply_event(&start("c", "read", json!({"path": "3"})));
        assert_eq!(state.get("c").unwrap().ordinal, 0);
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn github_token_shape_must_not_leak_in_views() {
        // CONTRACT: free-text GitHub PATs (ghp_/gho_ underscore form) must be
        // redacted in compact/expanded/Debug. Real PATs are not ghp-hyphen.
        let pat = ["gh", "p_", "abcdefghijklmnopqrstuvwxyz123456"].concat();
        let mut state = ToolPresentationState::new();
        state.apply_event(&start(
            "gh",
            "http",
            json!({"url": "https://api.github.com", "note": format!("token {pat}")}),
        ));
        state.apply_event(&end(
            "gh",
            "http",
            text_result(&format!("Authorization failed with {pat}")),
            true,
        ));

        let compact = state.get("gh").unwrap().compact_view();
        assert!(
            !compact.content_preview.contains(pat.as_str()),
            "compact content leaked PAT: {}",
            compact.content_preview
        );

        let expanded = state.expanded_view("gh").unwrap();
        assert!(
            !expanded.content_text.contains(pat.as_str()),
            "expanded content leaked PAT: {}",
            expanded.content_text
        );
        if let Some(err) = &expanded.error_message {
            assert!(!err.contains(pat.as_str()), "error_message leaked PAT: {err}");
        }
        let debug = format!("{:?}", state.get("gh").unwrap());
        assert!(!debug.contains(pat.as_str()), "Debug leaked PAT: {debug}");
    }

    #[test]
    fn empty_tool_call_ids_are_synthesized_as_distinct_deterministic_cards() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("", "read", json!({"path": "one.rs"})));
        state.apply_event(&start("", "read", json!({"path": "two.rs"})));

        assert_eq!(state.len(), 2);
        let cards = state.cards_in_source_order();
        assert_eq!(cards[0].tool_call_id, "__pi_empty_tool_call_0");
        assert_eq!(cards[1].tool_call_id, "__pi_empty_tool_call_1");
        assert_eq!(cards[0].arguments["path"], "one.rs");
        assert_eq!(cards[1].arguments["path"], "two.rs");
        assert_ne!(cards[0].tool_call_id, cards[1].tool_call_id);
        assert_eq!(cards[0].ordinal, 0);
        assert_eq!(cards[1].ordinal, 1);
    }

    #[test]
    fn valid_id_matching_synthetic_namespace_remains_unchanged() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("", "read", json!({"path": "empty.rs"})));
        state.apply_event(&start(
            "__pi_empty_tool_call_0",
            "read",
            json!({"path": "valid.rs"}),
        ));

        assert_eq!(state.len(), 2);
        assert_eq!(
            state.get("__pi_empty_tool_call_0").unwrap().arguments["path"],
            "valid.rs"
        );
        assert_eq!(
            state.get("__pi_empty_tool_call_1").unwrap().arguments["path"],
            "empty.rs"
        );
    }

    #[test]
    fn empty_tool_result_ids_are_synthesized_as_distinct_deterministic_cards() {
        let mut state = ToolPresentationState::new();
        state.apply_tool_result(&tool_result_message("", "read", "one", false, None));
        state.apply_tool_result(&tool_result_message("", "read", "two", false, None));

        let cards = state.cards_in_source_order();
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].tool_call_id, "__pi_empty_tool_call_0");
        assert_eq!(cards[1].tool_call_id, "__pi_empty_tool_call_1");
        assert_eq!(content_blocks_text(&cards[0].content), "one");
        assert_eq!(content_blocks_text(&cards[1].content), "two");
    }

    #[test]
    fn success_end_with_cancelled_wording_stays_succeeded() {
        // CONTRACT: content heuristics must not mark Cancelled when is_error=false
        // unless details explicitly signal cancel. Ordinary success text that
        // merely mentions "cancelled" stays Succeeded.
        let mut state = ToolPresentationState::new();
        state.apply_event(&start(
            "ok-cancel-word",
            "http",
            json!({"url": "https://example.test/order"}),
        ));
        state.apply_event(&end(
            "ok-cancel-word",
            "http",
            text_result("request cancelled by upstream was rolled back; order placed"),
            false,
        ));
        let card = state.get("ok-cancel-word").unwrap();
        assert!(!card.is_error);
        assert!(!card.cancelled);
        assert_eq!(card.status, ToolCallViewStatus::Succeeded);
        assert_eq!(card.status.label(), "ok");
    }

    #[test]
    fn error_content_cancellation_cues_still_mark_cancelled() {
        // Positive counterpart: is_error=true + cancel wording / details remain Cancelled.
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("err-abort", "bash", json!({"command": "sleep 9"})));
        state.apply_event(&end(
            "err-abort",
            "bash",
            text_result("Operation aborted by user"),
            true,
        ));
        assert_eq!(
            state.get("err-abort").unwrap().status,
            ToolCallViewStatus::Cancelled
        );
        assert!(state.get("err-abort").unwrap().cancelled);

        state.apply_event(&start("err-status", "bash", json!({"command": "x"})));
        state.apply_event(&end(
            "err-status",
            "bash",
            result_with_details("stopped", json!({"status": "cancelled"})),
            true,
        ));
        assert_eq!(
            state.get("err-status").unwrap().status,
            ToolCallViewStatus::Cancelled
        );

        // details.cancelled=true may signal cancel even alongside is_error=false
        // when the structured cue is explicit (not free-text).
        state.apply_event(&start("det-cancel", "read", json!({"path": "x"})));
        state.apply_event(&end(
            "det-cancel",
            "read",
            result_with_details("ok-ish", json!({"cancelled": true})),
            false,
        ));
        assert_eq!(
            state.get("det-cancel").unwrap().status,
            ToolCallViewStatus::Cancelled
        );
        assert!(state.get("det-cancel").unwrap().cancelled);
    }

    #[test]
    fn authorization_bearer_jwt_must_not_leak_in_views() {
        // CONTRACT: `Authorization: Bearer <jwt>` must redact the full credential,
        // not just the word Bearer. JWT absent from compact/expanded/error/Debug.
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let mut state = ToolPresentationState::new();
        state.apply_event(&start(
            "jwt",
            "http",
            json!({"url": "https://api.example/v1"}),
        ));
        state.apply_event(&end(
            "jwt",
            "http",
            text_result(&format!("Authorization: Bearer {jwt}")),
            true,
        ));

        let expanded = state.expanded_view("jwt").unwrap();
        assert!(
            !expanded.content_text.contains(jwt),
            "expanded content leaked JWT: {}",
            expanded.content_text
        );
        assert!(
            !expanded.content_text.contains("eyJ"),
            "expanded content leaked JWT header fragment: {}",
            expanded.content_text
        );
        if let Some(err) = &expanded.error_message {
            assert!(!err.contains(jwt), "error_message leaked JWT: {err}");
            assert!(
                !err.contains("eyJ"),
                "error_message leaked JWT fragment: {err}"
            );
        }
        let compact = state.get("jwt").unwrap().compact_view();
        assert!(
            !compact.content_preview.contains(jwt),
            "compact preview leaked JWT: {}",
            compact.content_preview
        );
        assert!(
            !compact.content_preview.contains("eyJ"),
            "compact preview leaked JWT fragment: {}",
            compact.content_preview
        );
        let debug = format!("{:?}", state.get("jwt").unwrap());
        assert!(!debug.contains(jwt), "Debug leaked JWT: {debug}");
        assert!(!debug.contains("eyJ"), "Debug leaked JWT fragment: {debug}");
    }

    #[test]
    fn tool_card_serialize_redacts_secrets() {
        // CONTRACT: Serialize of ToolCard must not dump raw secrets. Views already
        // redact; custom Serialize (or redacted DTO) is required for logs/RPC.
        let secret = ["s", "k-", "live-super-secret-value-do-not-leak"].concat();
        let mut state = ToolPresentationState::new();
        state.apply_event(&start(
            "ser",
            "http",
            json!({
                "url": "https://api.example/v1",
                "api_key": secret,
                "password": "hunter2",
            }),
        ));
        state.apply_event(&end(
            "ser",
            "http",
            result_with_details(&format!("token={secret}"), json!({"apiKey": secret})),
            true,
        ));

        let expanded = state.expanded_view("ser").unwrap();
        let view_args = serde_json::to_string(&expanded.arguments).unwrap();
        assert!(!view_args.contains(secret.as_str()));
        assert!(view_args.contains("[REDACTED]"));

        let raw = serde_json::to_string(state.get("ser").unwrap()).expect("serialize card");
        assert!(
            !raw.contains(secret.as_str()),
            "ToolCard Serialize leaked secret: {raw}"
        );
        assert!(
            !raw.contains("hunter2"),
            "ToolCard Serialize leaked password: {raw}"
        );
        assert!(
            raw.contains("[REDACTED]"),
            "expected redaction marker in Serialize dump: {raw}"
        );
    }

    #[test]
    fn late_update_after_end_leaves_terminal_card_immutable() {
        // CONTRACT: malformed Start→End(final)→Update(partial) must not mutate a
        // terminal card. Name/args/content/details/status stay as End left them.
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("t", "bash", json!({"command": "done"})));
        state.apply_event(&end(
            "t",
            "bash",
            result_with_details("final", json!({"exitCode": 0})),
            false,
        ));
        assert_eq!(
            state.get("t").unwrap().status,
            ToolCallViewStatus::Succeeded
        );
        assert_eq!(
            content_blocks_text(&state.get("t").unwrap().content),
            "final"
        );

        state.apply_event(&update(
            "t",
            "other-name",
            json!({"command": "mutated"}),
            result_with_details("late-partial", json!({"partial": true})),
        ));
        let card = state.get("t").unwrap();
        assert_eq!(card.status, ToolCallViewStatus::Succeeded);
        assert!(card.is_terminal());
        assert!(!card.is_partial());
        assert_eq!(card.tool_name, "bash");
        assert_eq!(card.arguments["command"], "done");
        assert_eq!(content_blocks_text(&card.content), "final");
        assert_eq!(card.details["exitCode"], 0);
        assert!(card.details.get("partial").is_none());
    }

    #[test]
    fn message_start_alone_does_not_create_card() {
        // CONTRACT: MessageStart(ToolResult) is ignored. Only MessageEnd creates
        // orphan/history cards and sets has_message_result.
        let mut state = ToolPresentationState::new();
        state.apply_event(&AgentEvent::MessageStart {
            message: Message::ToolResult(tool_result_message(
                "start-only",
                "read",
                "preview body",
                false,
                Some(json!({"phase": "start"})),
            )),
        });
        assert!(
            state.is_empty(),
            "MessageStart alone must leave projection empty"
        );
        assert!(!state.contains("start-only"));

        // MessageEnd still creates the durable orphan/history card.
        state.apply_event(&message_end_result(tool_result_message(
            "start-only",
            "read",
            "preview body",
            false,
            Some(json!({"phase": "end"})),
        )));
        assert_eq!(state.len(), 1);
        let card = state.get("start-only").unwrap();
        assert_eq!(card.status, ToolCallViewStatus::OrphanRepaired);
        assert!(card.has_message_result);
        assert!(!card.has_execution_end);
        assert_eq!(content_blocks_text(&card.content), "preview body");
        assert_eq!(card.details["phase"], "end");
    }

    #[test]
    fn aws_credentials_do_not_leak_from_projection_surfaces() {
        let access = ["AK", "IA", "IOSFODNN7EXAMPLE"].concat();
        let standalone_access = ["AK", "IA", "1234567890ABCDEF"].concat();
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let session = "IQoJb3JpZ2luX2VjEExampleSessionToken1234567890";
        let command = format!(
            "AWS_ACCESS_KEY_ID={access} AWS_SECRET_ACCESS_KEY={secret} \
             AWS_SESSION_TOKEN={session} aws sts get-caller-identity {standalone_access}"
        );

        let summary = compact_tool_arguments(&json!({"command": command}));
        for credential in [access.as_str(), standalone_access.as_str(), secret, session] {
            assert!(
                !summary.contains(credential),
                "summary leaked {credential}: {summary}"
            );
        }

        let mut state = ToolPresentationState::new();
        state.apply_event(&start(
            "aws",
            "bash",
            json!({
                "command": command,
                "AWS_ACCESS_KEY_ID": access,
                "AWS_SECRET_ACCESS_KEY": secret,
                "AWS_SESSION_TOKEN": session,
            }),
        ));
        state.apply_event(&end(
            "aws",
            "bash",
            result_with_details(
                &format!("failed for {standalone_access} AWS_SESSION_TOKEN={session}"),
                json!({"AWS_ACCESS_KEY_ID": access, "note": format!("key {standalone_access}")}),
            ),
            true,
        ));

        let card = state.get("aws").unwrap();
        let surfaces = [
            card.compact_title(),
            serde_json::to_string(&card.compact_view()).unwrap(),
            serde_json::to_string(&card.expanded_view()).unwrap(),
            serde_json::to_string(card).unwrap(),
            format!("{card:?}"),
        ];
        for surface in surfaces {
            for credential in [access.as_str(), standalone_access.as_str(), secret, session] {
                assert!(
                    !surface.contains(credential),
                    "projection surface leaked {credential}: {surface}"
                );
            }
        }
    }

    #[test]
    fn empty_terminal_details_clear_stale_streaming_metadata() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&start("end", "bash", json!({"command": "long"})));
        state.apply_event(&update(
            "end",
            "bash",
            json!({"command": "long"}),
            result_with_details("partial", json!({"progress": 1})),
        ));
        state.apply_event(&end(
            "end",
            "bash",
            result_with_details("final", json!({})),
            false,
        ));
        let end_card = state.get("end").unwrap();
        assert_eq!(end_card.status, ToolCallViewStatus::Succeeded);
        assert!(is_empty_details(&end_card.details));
        assert!(!end_card.compact_view().has_details);

        state.apply_event(&start("result", "bash", json!({"command": "long"})));
        state.apply_event(&update(
            "result",
            "bash",
            json!({"command": "long"}),
            result_with_details("partial", json!({"progress": 2})),
        ));
        state.apply_event(&message_end_result(tool_result_message(
            "result", "bash", "final", false, None,
        )));
        let result_card = state.get("result").unwrap();
        assert!(is_empty_details(&result_card.details));
        assert!(!result_card.compact_view().has_details);
    }

    #[test]
    fn durable_results_repair_end_only_completion_order() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&end("b", "bash", text_result("out-b"), false));
        state.apply_event(&end("a", "bash", text_result("out-a"), false));
        assert_eq!(state.get("b").unwrap().first_observation_ordinal, 0);
        assert_eq!(state.get("a").unwrap().first_observation_ordinal, 1);

        state.apply_event(&message_end_result(tool_result_message(
            "a", "bash", "msg-a", false, None,
        )));
        state.apply_event(&message_end_result(tool_result_message(
            "b", "bash", "msg-b", false, None,
        )));

        let views = state.compact_views();
        assert_eq!(
            views
                .iter()
                .map(|view| view.tool_call_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(state.get("a").unwrap().ordinal, 0);
        assert_eq!(state.get("b").unwrap().ordinal, 1);
        assert_eq!(state.get("a").unwrap().first_observation_ordinal, 1);
        assert_eq!(state.get("b").unwrap().first_observation_ordinal, 0);
        assert_eq!(
            content_blocks_text(&state.get("a").unwrap().content),
            "msg-a"
        );
        assert_eq!(
            content_blocks_text(&state.get("b").unwrap().content),
            "msg-b"
        );
    }

    #[test]
    fn repaired_source_order_stays_before_unrepaired_completion_fallback() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&end("c", "bash", text_result("out-c"), false));
        state.apply_event(&end("b", "bash", text_result("out-b"), false));
        state.apply_event(&end("a", "bash", text_result("out-a"), false));
        state.apply_event(&message_end_result(tool_result_message(
            "a", "bash", "msg-a", false, None,
        )));
        state.apply_event(&message_end_result(tool_result_message(
            "b", "bash", "msg-b", false, None,
        )));

        let cards = state.cards_in_source_order();
        assert_eq!(
            cards
                .iter()
                .map(|card| card.tool_call_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(state.get("c").unwrap().first_observation_ordinal, 0);
    }

    #[test]
    fn assistant_tool_calls_repair_end_only_completion_order() {
        let mut state = ToolPresentationState::new();
        state.apply_event(&end("b", "bash", text_result("out-b"), false));
        state.apply_event(&end("a", "bash", text_result("out-a"), false));
        state.apply_event(&AgentEvent::MessageEnd {
            message: Message::Assistant(pi_ai::AssistantMessage {
                content: vec![
                    ContentBlock::ToolCall(pi_ai::ToolCall {
                        id: "a".to_owned(),
                        name: "bash".to_owned(),
                        arguments: json!({"command": "a"}),
                        thought_signature: None,
                    }),
                    ContentBlock::ToolCall(pi_ai::ToolCall {
                        id: "b".to_owned(),
                        name: "bash".to_owned(),
                        arguments: json!({"command": "b"}),
                        thought_signature: None,
                    }),
                ],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                response_model: None,
                response_id: None,
                diagnostics: Vec::new(),
                usage: pi_ai::Usage::default(),
                stop_reason: pi_ai::StopReason::ToolUse,
                error_message: None,
                raw_stop_reason: None,
                timestamp: now_millis(),
            }),
        });

        let views = state.compact_views();
        assert_eq!(
            views
                .iter()
                .map(|view| view.tool_call_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(state.get("a").unwrap().arguments["command"], "a");
        assert_eq!(state.get("b").unwrap().arguments["command"], "b");
    }

    #[test]
    fn bounded_preview_stops_before_unbounded_iterator_tail() {
        use std::cell::Cell;

        let visited = Cell::new(0usize);
        let parts = std::iter::from_fn(|| {
            let index = visited.get();
            visited.set(index + 1);
            assert!(index < 20, "preview traversed the unbounded tail");
            Some("0123456789")
        });

        let preview = bounded_text_preview(parts, 12);
        assert_eq!(preview, "012345678...");
        assert!(visited.get() <= 14, "visited {} chunks", visited.get());
    }
}

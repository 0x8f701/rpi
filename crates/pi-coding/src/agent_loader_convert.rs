use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use agent_loader::domain::{Session, SourceTool, TargetTool};
use agent_loader::emit::{self, EmitContext};
use agent_loader::formats::{claude, codex, droid, grok, omp, pi};
use serde_json::Value;
use uuid::Uuid;

pub fn parse_staged_sources(tool: SourceTool, staged_paths: &[&Path]) -> Result<Session> {
    if staged_paths.is_empty() {
        bail!("no staged source paths to convert");
    }
    if tool == SourceTool::Agent {
        bail!("Agent sessions cannot be converted");
    }
    // Chain semantics (root -> leaf staging order): messages concatenate,
    // start time anchors at the root, and the LEAF owns the session
    // identity and cwd — matching the authoritative local chain merge.
    let mut combined: Option<Session> = None;
    let mut leaf_meta: Option<(String, PathBuf)> = None;
    for path in staged_paths {
        let mut session = parse_one(tool, path)?;
        leaf_meta = Some((session.session_id.clone(), session.cwd.clone()));
        match &mut combined {
            None => combined = Some(session),
            Some(acc) => acc.messages.append(&mut session.messages),
        }
    }
    let mut combined = combined.ok_or_else(|| anyhow!("no staged source paths to convert"))?;
    if let Some((leaf_id, leaf_cwd)) = leaf_meta {
        combined.session_id = leaf_id;
        combined.cwd = leaf_cwd;
    }
    Ok(combined)
}

pub fn emit_rpi_records(session: &Session, session_id: &str, cwd: &Path) -> Result<Vec<Value>> {
    let mut session = session.clone();
    if !cwd.as_os_str().is_empty() {
        session.cwd = cwd.to_path_buf();
    }
    // A neutral model hint keeps the emitted model_change record truthful:
    // the library's default "converted-from-{tool}" would misreport the
    // session as still running its source tool's model.
    if session.hints.model.is_none() {
        session.hints.model = Some("imported".to_owned());
    }
    let dir = std::env::temp_dir().join(format!("rpi-emit-{}", Uuid::new_v4()));
    fs::create_dir(&dir).with_context(|| format!("creating emit staging {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting emit staging {}", dir.display()))?;
    }
    let output = dir.join("converted.jsonl");
    let context = EmitContext::new(&dir)
        .with_session_id(session_id)
        .with_output(&output);
    let emit_result = emit::emit(&session, TargetTool::Rpi, &context);
    let records = match emit_result {
        Ok(_) => read_jsonl_objects(&output)?,
        Err(error) => {
            let _ = fs::remove_dir_all(&dir);
            return Err(error);
        }
    };
    let _ = fs::remove_dir_all(&dir);
    // A source without model info gets the synthetic model authority
    // ("sessions-convert"/"imported") stamped on the model_change record AND
    // every assistant payload. Neither resolves in the model catalog, so
    // fork/resume of the imported session would fail closed
    // (recorded_cut_model). Strip the synthetic authority everywhere it was
    // stamped: drop the model_change record, remove provider/modelId keys
    // from assistant payloads, and reconnect the thinking record's parent to
    // the session root so lineage stays connected.
    const SYNTHETIC_PROVIDER: &str = "sessions-convert";
    let mut dropped_model_id: Option<String> = None;
    let records: Vec<Value> = records
        .into_iter()
        .filter_map(|mut record| {
            // The library fabricates provider "sessions-convert" for every
            // source that lacks a provider hint (model hints alone are kept).
            // That provider never resolves in the model catalog, so any
            // authority pair stamped with it would fail closed on fork/resume
            // (recorded_cut_model). Drop the model_change record, blank the
            // assistant payload authority, and reconnect the thinking
            // record's parent to the session root.
            let record_type = record.get("type").and_then(Value::as_str).map(str::to_owned);
            let provider = record.get("provider").and_then(Value::as_str).map(str::to_owned);
            if record_type.as_deref() == Some("model_change") {
                let synthetic = record.get("modelId").and_then(Value::as_str).is_some()
                    && provider.as_deref() == Some(SYNTHETIC_PROVIDER);
                if synthetic {
                    dropped_model_id = record.get("id").and_then(Value::as_str).map(str::to_owned);
                    return None;
                }
            }
            if record_type.as_deref() == Some("thinking_level_change") {
                let parent_is_dropped_model = dropped_model_id.as_deref().is_some_and(|dropped| {
                    record.get("parentId").and_then(Value::as_str) == Some(dropped)
                });
                if parent_is_dropped_model {
                    record
                        .as_object_mut()
                        .expect("JSON object")
                        .insert("parentId".to_owned(), Value::Null);
                }
            }
            if let Some(message) = record.get_mut("message").and_then(Value::as_object_mut) {
                if message.get("provider").and_then(Value::as_str) == Some(SYNTHETIC_PROVIDER) {
                    message.insert("provider".to_owned(), Value::String(String::new()));
                    if let Some(model) = message.get_mut("model") {
                        *model = Value::String(String::new());
                    }
                }
            }
            Some(record)
        })
        .collect();
    Ok(drop_imported_handoff_records(records))
}

fn drop_imported_handoff_records(records: Vec<Value>) -> Vec<Value> {
    let mut dropped_parent = HashMap::new();
    let kept: Vec<Value> = records
        .into_iter()
        .filter(|record| {
            let is_handoff = record.get("type").and_then(Value::as_str) == Some("custom_message")
                && record.get("customType").and_then(Value::as_str) == Some("handoff");
            if !is_handoff {
                return true;
            }
            if let Some(id) = record.get("id").and_then(Value::as_str) {
                dropped_parent.insert(
                    id.to_owned(),
                    record.get("parentId").cloned().unwrap_or(Value::Null),
                );
            }
            false
        })
        .collect();
    kept.into_iter()
        .map(|mut record| {
            let parent = record
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let Some(parent_id) = parent else {
                return record;
            };
            let mut replacement = match dropped_parent.get(&parent_id) {
                Some(value) => value.clone(),
                None => return record,
            };
            loop {
                let Some(next_id) = replacement.as_str().filter(|id| !id.is_empty()) else {
                    break;
                };
                match dropped_parent.get(next_id) {
                    Some(next) => replacement = next.clone(),
                    None => break,
                }
            }
            if let Some(object) = record.as_object_mut() {
                object.insert("parentId".to_owned(), replacement);
            }
            record
        })
        .collect()
}

fn parse_one(tool: SourceTool, path: &Path) -> Result<Session> {
    let mut session = match tool {
        SourceTool::Pi | SourceTool::Rpi => pi::parse(path)?,
        SourceTool::Omp => omp::parse(path)?,
        SourceTool::Codex => codex::parse(path)?,
        SourceTool::Claude => claude::parse(path)?,
        SourceTool::Grok => {
            let root = path
                .parent()
                .ok_or_else(|| anyhow!("Grok summary has no parent"))?;
            grok::parse(path, root)?
        }
        SourceTool::Droid => droid::parse(path)?,
        SourceTool::Agent => bail!("Agent sessions cannot be converted"),
    };
    session.tool = tool;
    session.path = path.to_path_buf();
    Ok(session)
}

fn read_jsonl_objects(path: &Path) -> Result<Vec<Value>> {
    let body = fs::read_to_string(path)
        .with_context(|| format!("reading converted JSONL {}", path.display()))?;
    let mut records = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)
            .with_context(|| format!("converted JSONL is not JSON in {}", path.display()))?;
        records.push(value);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_loader::domain::{Role, SourceTool};
    use std::collections::HashSet;

    #[test]
    fn grok_tool_turns_survive_rpi_jsonl_convert() {
        let home = tempfile::tempdir().expect("home");
        let session_dir = home.path().join("grok-session");
        fs::create_dir_all(&session_dir).expect("session dir");
        fs::write(
            session_dir.join("summary.json"),
            r#"{"info":{"id":"grok-tools","cwd":"<workspace>/grok"},"created_at":"2026-04-01T00:00:00Z"}"#,
        )
        .expect("summary");
        fs::write(
            session_dir.join("chat_history.jsonl"),
            concat!(
                r#"{"type":"user","content":[{"type":"text","text":"search it"}]}"#,
                "\n",
                r#"{"type":"backend_tool_call","kind":{"tool_type":"web_search","id":"ws_1","action":{"type":"search","query":"capybaras"}}}"#,
                "\n",
                r#"{"type":"tool_result","tool_call_id":"ws_1","content":"found"}"#,
                "\n",
                r#"{"role":"assistant","content":"done","created_at":"2026-04-01T00:00:02Z"}"#,
                "\n",
            ),
        )
        .expect("chat");
        let summary = session_dir.join("summary.json");
        let session = parse_staged_sources(SourceTool::Grok, &[summary.as_path()]).expect("parse");
        assert!(
            session.messages.iter().any(|message| {
                message.role == Role::Assistant
                    && message.parts.iter().any(|part| {
                        matches!(
                            part,
                            agent_loader::domain::ContentPart::ToolUse { id, name, .. }
                                if id == "ws_1" && name == "web_search"
                        )
                    })
            }),
            "agent-loader must keep the Grok tool use"
        );
        let records = emit_rpi_records(&session, "converted-id", Path::new("<workspace>/grok"))
            .expect("emit");
        let body = records
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains("\"type\":\"toolCall\"")
                && body.contains("web_search")
                && body.contains("ws_1"),
            "rpi JSONL must keep the Grok tool turn, got {body}"
        );
        assert!(body.contains("search it") && body.contains("done"), "{body}");
    }

    #[test]
    fn synthetic_model_authority_is_stripped_from_emitted_records() {
        let home = tempfile::tempdir().expect("home");
        let session_dir = home.path().join("no-model-session");
        fs::create_dir_all(&session_dir).expect("session dir");
        fs::write(
            session_dir.join("summary.json"),
            r#"{"info":{"id":"no-model","cwd":"<workspace>/plain"},"created_at":"2026-04-01T00:00:00Z"}"#,
        )
        .expect("summary");
        fs::write(
            session_dir.join("chat_history.jsonl"),
            concat!(
                r#"{"type":"user","content":[{"type":"text","text":"hello"}]}"#,
                "\n",
                r#"{"role":"assistant","content":"hi there","created_at":"2026-04-01T00:00:02Z"}"#,
                "\n",
            ),
        )
        .expect("chat");
        let summary = session_dir.join("summary.json");
        let session = parse_staged_sources(SourceTool::Grok, &[summary.as_path()]).expect("parse");
        let records = emit_rpi_records(&session, "converted-id", Path::new("<workspace>/plain"))
            .expect("emit");
        for record in &records {
            let record_type = record.get("type").and_then(Value::as_str).unwrap_or_default();
            assert!(
                !(record_type == "model_change"
                    && record.get("modelId").and_then(Value::as_str) == Some("imported")),
                "synthetic model_change must be dropped: {record}"
            );
            if let Some(message) = record.get("message").and_then(Value::as_object) {
                assert!(
                    message.get("provider").and_then(Value::as_str) != Some("sessions-convert"),
                    "synthetic provider must be stripped: {message:?}"
                );
                assert!(
                    message.get("model").and_then(Value::as_str) != Some("imported"),
                    "synthetic model must be stripped: {message:?}"
                );
            }
            if record_type == "thinking_level_change" {
                assert!(
                    record.get("parentId").and_then(Value::as_str) != Some("imported"),
                    "thinking record must not reference the dropped model id: {record}"
                );
            }
        }
        // Native reader contract: every non-null parentId in the emitted
        // journal resolves to an earlier record id, so lineage stays one chain.
        let mut seen_ids = std::collections::HashSet::new();
        for record in &records {
            if let Some(parent) = record.get("parentId").and_then(Value::as_str) {
                if !parent.is_empty() {
                    assert!(
                        seen_ids.contains(parent),
                        "dangling parent {parent} in {record}"
                    );
                }
            }
            if let Some(id) = record.get("id").and_then(Value::as_str) {
                seen_ids.insert(id.to_owned());
            }
        }
    }

    #[test]
    fn omp_handoff_custom_messages_are_not_emitted() {
        let home = tempfile::tempdir().expect("tmp");
        let path = home.path().join("omp-leaf.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"session","version":3,"id":"omp-leaf","timestamp":"2026-01-01T00:01:00Z","cwd":"<workspace>/omp"}"#,
                "\n",
                r#"{"type":"custom_message","customType":"handoff","content":"handoff context summarizing final history","display":true,"attribution":"agent","id":"handoff-1","parentId":null,"timestamp":"2026-01-01T00:01:00.500Z"}"#,
                "\n",
                r#"{"type":"message","id":"u0","parentId":"handoff-1","timestamp":"2026-01-01T00:01:01Z","message":{"role":"user","content":[{"type":"text","text":"OMP final prompt"}]}}"#,
                "\n",
                r#"{"type":"message","id":"a0","parentId":"u0","timestamp":"2026-01-01T00:01:02Z","message":{"role":"assistant","content":[{"type":"text","text":"OMP final reply"}]}}"#,
                "\n",
            ),
        )
        .expect("write");
        let session = parse_staged_sources(SourceTool::Omp, &[path.as_path()]).expect("parse");
        let records = emit_rpi_records(&session, "imported-id", Path::new("<workspace>/omp"))
            .expect("emit");
        let body = records
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !body.contains("handoff context"),
            "handoff custom-message text must not reach the native import, got {body}"
        );
        assert!(
            body.contains("OMP final prompt") && body.contains("OMP final reply"),
            "{body}"
        );
        let mut seen_ids = HashSet::new();
        for record in &records {
            let record_type = record.get("type").and_then(Value::as_str).unwrap_or_default();
            assert!(
                !(record_type == "custom_message"
                    && record.get("customType").and_then(Value::as_str) == Some("handoff")),
                "handoff custom_message must be dropped: {record}"
            );
            if let Some(parent) = record.get("parentId").and_then(Value::as_str) {
                if !parent.is_empty() {
                    assert!(
                        seen_ids.contains(parent),
                        "dangling parent {parent} in {record}"
                    );
                }
            }
            if let Some(id) = record.get("id").and_then(Value::as_str) {
                seen_ids.insert(id.to_owned());
            }
        }
    }
}

#!/usr/bin/env python3
"""Dedicated loopback mock provider for the user-perspective project-authoring E2E.

Serves deterministic SSE responses on localhost and writes the chosen port to
`--port-file` so the bash scenario can build `models.json` and launch rpi
against it.

The mock drives a finite state machine that makes the real rpi agent build a
dependency-free Rust CLI task tracker (task-tracker) from an EMPTY workspace:

1. `todo` init (4 phases, 10 tasks) — the model must plan the work.
2. `write` Cargo.toml + src/{model,parser,store,main}.rs. src/store.rs is
   written with a deliberate marker-parsing defect: each line is split on the
   first space, which breaks `[ ]` markers — the space lives INSIDE the marker,
   so `"[ ] beta"` splits into marker `"["` and title `"] beta"`.
3. `bash cargo test --offline` — MUST FAIL on the planted defect
   ("test result: FAILED", exit 101); the mock verifies the failure.
4. `read` src/store.rs — the model must inspect the defect.
5. `edit` src/store.rs — repair the marker parse via exact text replacement.
6. `bash cargo test --offline` — MUST PASS ("test result: ok"); verified.
7. `bash` valid CLI run: build + add "buy milk" + done 0 + list (exit 0,
   deterministic output: "added 0: buy milk" / "completed 0" / "0 [x]: buy milk").
8. `bash` invalid CLI runs: `task-tracker bogus` and `task-tracker done abc`
   (both exit 1 with actionable stderr).
9. Interleaved `todo` done ops mark every task completed, so the session ends
   with a fully completed Todo DAG.

Routing classifies the accumulated conversation by the mock's OWN deterministic
call ids (tool results keyed by `tool_call_id`) plus filesystem state — never
by a brittle global request parity. Every response is a single tool call (or
the final assistant text), so the run is bounded and reproducible.

Usage:
    python3 project_authoring_mock.py --port-file FILE --workspace DIR
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from typing import Any

# Deterministic mock-generated tool call ids. The agent executes them verbatim
# and returns results keyed by these ids, which is how the state machine
# advances (call-id/name classification, not request parity).
CALL_TODO_INIT = "pa-todo-init"
CALL_TODO_DONE_SCAFFOLD = "pa-todo-done-scaffold"
CALL_TODO_DONE_IMPLEMENT = "pa-todo-done-implement"
CALL_TODO_DONE_REPAIR_INSPECT = "pa-todo-done-repair-inspect"
CALL_TODO_DONE_REPAIR_FIX = "pa-todo-done-repair-fix"
CALL_TODO_DONE_PASS_TEST = "pa-todo-done-pass-test"
CALL_TODO_DONE_VALID = "pa-todo-done-valid"
CALL_TODO_DONE_INVALID = "pa-todo-done-invalid"
CALL_WRITE_CARGO_TOML = "pa-write-cargo-toml"
CALL_WRITE_MODEL = "pa-write-model"
CALL_WRITE_PARSER = "pa-write-parser"
CALL_WRITE_STORE = "pa-write-store"
CALL_WRITE_MAIN = "pa-write-main"
CALL_BASH_TEST_FAIL = "pa-bash-test-fail"
CALL_BASH_TEST_PASS = "pa-bash-test-pass"
CALL_BASH_VALID = "pa-bash-valid"
CALL_BASH_INVALID_1 = "pa-bash-invalid-1"
CALL_BASH_INVALID_2 = "pa-bash-invalid-2"
CALL_READ_STORE = "pa-read-store"
CALL_EDIT_STORE = "pa-edit-store"

# Hard budget: the state machine must never serve more requests than one
# healthy run needs (well under this cap). Exceeding it is fail-closed.
MAX_REQUESTS = 80

# The planted defect: splitting on the first space breaks "[ ]" markers
# ("[ ] beta".split_once(' ') yields marker "[" and title "] beta"), so
# parse_store always rejects saved files and load() falls back to an empty
# list. The defect text sits near the top of store.rs — inside the first 6
# content rows the read/write cards render.
DEFECT_MARKER = """        let (marker, title) = line
            .split_once(' ')
            .ok_or_else(|| format!("line {}: expected '<marker> <title>'", line_number + 1))?;"""
FIXED_MARKER = """        let (marker, title) = line.split_at(3);
        let title = title.trim();"""

CARGO_TOML = """[package]
name = "task-tracker"
version = "0.1.0"
edition = "2021"

[dependencies]
"""

MODEL_RS = """//! Task domain model for the task-tracker CLI.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: u32,
    pub title: String,
    pub done: bool,
}

impl Task {
    pub fn new(id: u32, title: impl Into<String>) -> Self {
        Self { id, title: title.into(), done: false }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TaskList {
    pub tasks: Vec<Task>,
    pub next_id: u32,
}

impl TaskList {
    pub fn add(&mut self, title: impl Into<String>) -> u32 {
        let id = self.next_id;
        self.tasks.push(Task::new(id, title));
        self.next_id += 1;
        id
    }

    pub fn mark_done(&mut self, id: u32) -> bool {
        match self.tasks.iter_mut().find(|task| task.id == id) {
            Some(task) => {
                task.done = true;
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_assigns_incrementing_ids() {
        let mut list = TaskList::default();
        assert_eq!(list.add("first"), 0);
        assert_eq!(list.add("second"), 1);
    }

    #[test]
    fn mark_done_flips_the_flag() {
        let mut list = TaskList::default();
        let id = list.add("wash dishes");
        assert!(list.mark_done(id));
        assert!(list.tasks[0].done);
    }

    #[test]
    fn mark_done_unknown_id_returns_false() {
        let mut list = TaskList::default();
        assert!(!list.mark_done(99));
    }
}
"""

PARSER_RS = """//! Command-line parsing for the task-tracker CLI.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Add { title: String },
    Done { id: u32 },
    List,
    Help,
}

/// Parse argv (without the program name) into a Command.
pub fn parse(args: &[String]) -> Result<Command, String> {
    match args.first().map(String::as_str) {
        None | Some("help") | Some("--help") | Some("-h") => Ok(Command::Help),
        Some("add") => {
            let title = args[1..].join(" ").trim().to_string();
            if title.is_empty() {
                return Err("usage: task-tracker add <title>".to_string());
            }
            Ok(Command::Add { title })
        }
        Some("done") => {
            let raw = args
                .get(1)
                .ok_or_else(|| "usage: task-tracker done <id>".to_string())?;
            let id: u32 = raw
                .parse()
                .map_err(|_| format!("invalid task id: {raw}"))?;
            Ok(Command::Done { id })
        }
        Some("list") => Ok(Command::List),
        Some(other) => Err(format!("unknown command: {other} (try 'help')")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn parses_add_with_title() {
        assert_eq!(
            parse(&argv(&["add", "buy", "milk"])),
            Ok(Command::Add { title: "buy milk".into() })
        );
    }

    #[test]
    fn parses_done_with_id() {
        assert_eq!(parse(&argv(&["done", "3"])), Ok(Command::Done { id: 3 }));
    }

    #[test]
    fn rejects_unknown_command() {
        let error = parse(&argv(&["bogus"])).unwrap_err();
        assert!(error.contains("unknown command: bogus"), "{error}");
    }

    #[test]
    fn rejects_non_numeric_done_id() {
        let error = parse(&argv(&["done", "abc"])).unwrap_err();
        assert!(error.contains("invalid task id: abc"), "{error}");
    }

    #[test]
    fn empty_title_is_rejected() {
        assert!(parse(&argv(&["add"])).is_err());
    }
}
"""

STORE_RS = """//! Task persistence: file-backed store for the task-tracker CLI.

use std::fs;
use std::path::Path;

use crate::model::TaskList;

fn parse_store(contents: &str) -> Result<TaskList, String> {
    let mut list = TaskList::default();
    for (line_number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Lines are "<marker> <title>": marker is "[x]" (done) or "[ ]" (open).
        let (marker, title) = line
            .split_once(' ')
            .ok_or_else(|| format!("line {}: expected '<marker> <title>'", line_number + 1))?;
        let done = match marker {
            "[x]" => true,
            "[ ]" => false,
            other => return Err(format!("line {}: invalid marker {other:?}", line_number + 1)),
        };
        let id = list.add(title);
        if done {
            list.mark_done(id);
        }
    }
    Ok(list)
}

fn format_store(list: &TaskList) -> String {
    let mut out = String::new();
    for task in &list.tasks {
        let marker = if task.done { "[x]" } else { "[ ]" };
        out.push_str(&format!("{marker} {}\\n", task.title));
    }
    out
}

pub fn load(path: &Path) -> TaskList {
    match fs::read_to_string(path) {
        Ok(contents) => match parse_store(&contents) {
            Ok(list) => list,
            Err(_) => TaskList::default(),
        },
        Err(_) => TaskList::default(),
    }
}

pub fn save(path: &Path, list: &TaskList) -> Result<(), String> {
    let contents = format_store(list);
    fs::write(path, contents)
        .map_err(|error| format!("could not save {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn round_trip_preserves_done_flag() {
        let path = PathBuf::from(format!(
            "{}/task-tracker-store-test-{}.e2e",
            std::env::temp_dir().display(),
            std::process::id()
        ));
        let mut list = TaskList::default();
        list.add("alpha");
        list.add("beta");
        assert!(list.mark_done(0));
        save(&path, &list).unwrap();
        let reloaded = load(&path);
        assert_eq!(reloaded, list);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_loads_empty_list() {
        let list = load(Path::new("/nonexistent/task-store.e2e"));
        assert!(list.tasks.is_empty());
    }

    #[test]
    fn malformed_lines_are_reported_actionably() {
        let error = parse_store("nonsense line\\n").unwrap_err();
        assert!(error.contains("line 1"), "{error}");
    }

    #[test]
    fn corrupt_parse_falls_back_to_empty_list() {
        let path = PathBuf::from(format!(
            "{}/task-tracker-corrupt-{}.e2e",
            std::env::temp_dir().display(),
            std::process::id()
        ));
        std::fs::write(&path, "not a store\\n").unwrap();
        let list = load(&path);
        assert!(list.tasks.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
"""

MAIN_RS = """//! task-tracker: a dependency-free CLI task list.

mod model;
mod parser;
mod store;

use std::env;
use std::path::PathBuf;

use model::TaskList;
use parser::Command;

const STORE_FILE: &str = ".task-tracker.txt";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match parser::parse(&args) {
        Ok(Command::Help) => print_help(),
        Ok(Command::List) => run_list(),
        Ok(Command::Add { title }) => run_add(&title),
        Ok(Command::Done { id }) => run_done(id),
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
    }
}

fn store_path() -> PathBuf {
    PathBuf::from(STORE_FILE)
}

fn run_list() {
    let list = store::load(&store_path());
    if list.tasks.is_empty() {
        println!("no tasks");
        return;
    }
    for task in &list.tasks {
        let marker = if task.done { "[x]" } else { "[ ]" };
        println!("{} {}: {}", task.id, marker, task.title);
    }
}

fn run_add(title: &str) {
    let mut list = store::load(&store_path());
    let id = list.add(title);
    store::save(&store_path(), &list).unwrap_or_else(|error| {
        eprintln!("error: {error}");
        std::process::exit(1);
    });
    println!("added {}: {}", id, title);
}

fn run_done(id: u32) {
    let mut list = store::load(&store_path());
    if list.mark_done(id) {
        store::save(&store_path(), &list).unwrap_or_else(|error| {
            eprintln!("error: {error}");
            std::process::exit(1);
        });
        println!("completed {id}");
    } else {
        eprintln!("error: no task with id {id}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!("usage: task-tracker <command> [args]");
    println!("commands:");
    println!("  add <title>    add a task");
    println!("  done <id>      mark a task complete");
    println!("  list           list tasks");
    println!("  help           show this help");
}
"""

PROJECT_FILES: list[tuple[str, str]] = [
    ("Cargo.toml", CARGO_TOML),
    ("src/model.rs", MODEL_RS),
    ("src/parser.rs", PARSER_RS),
    ("src/store.rs", STORE_RS),
    ("src/main.rs", MAIN_RS),
]

TODO_PHASES: list[tuple[str, list[str]]] = [
    ("Scaffold", ["create Cargo.toml manifest"]),
    ("Implement", ["write model module", "write parser module", "write store module", "write main module"]),
    ("Repair", ["inspect store module", "fix deliberate marker-parse defect"]),
    ("Verify", ["pass cargo test", "exercise CLI on valid input", "exercise CLI on invalid input"]),
]

BASH_CARGO_TEST = "cargo test --offline"
BASH_VALID_RUN = (
    'cargo build --offline -q && '
    './target/debug/task-tracker add "buy milk" && '
    './target/debug/task-tracker done 0 && '
    './target/debug/task-tracker list'
)
BASH_INVALID_UNKNOWN = "./target/debug/task-tracker bogus"
BASH_INVALID_BAD_ID = "./target/debug/task-tracker done abc"

FINAL_TEXT = (
    "task-tracker complete. The dependency-free Rust CLI was built end to end: "
    "Cargo.toml plus src/{model,parser,store,main}.rs were written, the deliberate "
    "marker-parsing defect in src/store.rs (the line split broke '[ ]' markers) was "
    "inspected via read and repaired via edit, cargo test failed on the planted "
    "defect and then passed (12/12), and the binary was exercised on valid input "
    "(add/done/list) and invalid input (unknown command and a bad task id — both "
    "exit non-zero with actionable errors)."
)


def stream_response(payloads: list[dict[str, Any]]) -> bytes:
    rows = [f"data: {json.dumps(payload, separators=(',', ':'))}\n\n" for payload in payloads]
    rows.append("data: [DONE]\n\n")
    return "".join(rows).encode()


def tool_call_payload(rid: str, call_id: str, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": rid,
        "model": "project-authoring-mock",
        "choices": [
            {
                "index": 0,
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": json.dumps(arguments, separators=(",", ":")),
                            },
                        }
                    ]
                },
                "finish_reason": "tool_calls",
            }
        ],
        "usage": {"prompt_tokens": 8, "completion_tokens": 4, "total_tokens": 12},
    }


def text_payload(rid: str, text: str) -> dict[str, Any]:
    return {
        "id": rid,
        "model": "project-authoring-mock",
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 8, "completion_tokens": len(text), "total_tokens": 8 + len(text)},
    }


def todo_init_arguments() -> dict[str, Any]:
    return {
        "op": "init",
        "list": [{"phase": phase, "items": items} for phase, items in TODO_PHASES],
    }


def last_user_text(body: dict[str, Any]) -> str:
    """Text of the last REAL user input (system reminders are not input)."""
    for message in reversed(body.get("messages") or []):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, str):
            if content.lstrip().startswith("<system-reminder>"):
                continue
            return content
        if isinstance(content, list):
            text = " ".join(
                str(block.get("text", ""))
                for block in content
                if isinstance(block, dict) and block.get("type") == "text"
            )
            if text.lstrip().startswith("<system-reminder>"):
                continue
            return text
    return ""


def message_text(body: dict[str, Any]) -> str:
    fragments: list[str] = []
    for message in body.get("messages") or []:
        content = message.get("content")
        if isinstance(content, str):
            fragments.append(content)
        elif isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and isinstance(block.get("text"), str):
                    fragments.append(block["text"])
    return "\n".join(fragments)


def tool_result_for(body: dict[str, Any], call_id: str) -> tuple[str, str] | None:
    """(tool_name, content_text) of the tool result for `call_id`, or None.

    Classifies by the mock's own deterministic call id, so routing never
    depends on a fragile global request counter."""
    for message in body.get("messages") or []:
        if message.get("role") != "tool":
            continue
        if str(message.get("tool_call_id")) != call_id:
            continue
        content = message.get("content")
        if isinstance(content, str):
            text = content
        elif isinstance(content, list):
            text = "".join(
                str(block.get("text", ""))
                for block in content
                if isinstance(block, dict) and block.get("type") == "text"
            )
        else:
            text = str(content or "")
        return str(message.get("name") or ""), text
    return None


def todo_result_for(body: dict[str, Any], call_id: str) -> str | None:
    result = tool_result_for(body, call_id)
    if result is None:
        return None
    _name, text = result
    return text


def log(line: str) -> None:
    print(line, file=sys.stderr, flush=True)


class ProjectAuthoringServer(BaseHTTPRequestHandler):
    serial = 0
    workspace: Path = Path(".")

    def do_POST(self) -> None:
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length))
        type(self).serial += 1
        request_number = type(self).serial
        try:
            response, step = self.route(request_number, body)
        except BaseException as error:  # never let a routing bug wedge the client
            log(f"project-authoring request#{request_number} error: {error!r}")
            response = stream_response(
                [text_payload(f"pa-err-{request_number}", "mock rejected")]
            )
            step = "error"
        log(
            f"project-authoring request#{request_number} step={step} "
            f"user={last_user_text(body)!r}"
        )
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)

    def route(self, request_number: int, body: dict[str, Any]) -> tuple[bytes, str]:
        rid = f"pa-{request_number}"
        ws = type(self).workspace
        if request_number > MAX_REQUESTS:
            log(f"project-authoring request#{request_number} budget exhausted")
            return (
                stream_response(
                    [
                        text_payload(
                            rid,
                            "mock state machine exhausted (project-authoring); the run did not complete",
                        )
                    ]
                ),
                "budget-exhausted",
            )

        # 1. Plan: todo init first.
        if todo_result_for(body, CALL_TODO_INIT) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid, CALL_TODO_INIT, "todo", todo_init_arguments()
                        )
                    ]
                ),
                "todo-init",
            )

        # 2. Scaffold/implement: write every project file (filesystem-gated so
        #    retries are idempotent).
        for path, content in PROJECT_FILES:
            if not (ws / path).exists():
                return (
                    stream_response(
                        [
                            tool_call_payload(
                                rid,
                                f"pa-write-{path.replace('/', '-').removesuffix('.rs').removesuffix('.toml')}",
                                "write",
                                {"path": path, "content": content},
                            )
                        ]
                    ),
                    f"write-{path}",
                )

        store_text = (ws / "src/store.rs").read_text(encoding="utf-8")
        defect_present = DEFECT_MARKER in store_text

        # Todo milestones: scaffold phase done once Cargo.toml exists, then the
        # whole Implement phase once every module exists.
        if todo_result_for(body, CALL_TODO_DONE_SCAFFOLD) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_TODO_DONE_SCAFFOLD,
                            "todo",
                            {"op": "done", "task": "create Cargo.toml manifest"},
                        )
                    ]
                ),
                "todo-done-scaffold",
            )
        if todo_result_for(body, CALL_TODO_DONE_IMPLEMENT) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_TODO_DONE_IMPLEMENT,
                            "todo",
                            {"op": "done", "phase": "Implement"},
                        )
                    ]
                ),
                "todo-done-implement",
            )

        # 3. The first cargo test MUST fail on the planted defect.
        if defect_present and todo_result_for(body, CALL_BASH_TEST_FAIL) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_BASH_TEST_FAIL,
                            "bash",
                            {"command": BASH_CARGO_TEST, "timeout": 600},
                        )
                    ]
                ),
                "bash-test-fail",
            )
        if todo_result_for(body, CALL_BASH_TEST_FAIL) is not None:
            _name, text = tool_result_for(body, CALL_BASH_TEST_FAIL)  # type: ignore[misc]
            if "test result: FAILED" not in text:
                log(
                    f"project-authoring request#{request_number} FAIL-CLOSED: "
                    f"first cargo test did not fail on the planted defect: {text!r}"
                )
                return (
                    stream_response(
                        [text_payload(rid, "first cargo test did not FAIL; refusing to continue")]
                    ),
                    "fail-closed-test-fail",
                )

        # 4. Inspect the defect through the read tool.
        if defect_present and todo_result_for(body, CALL_READ_STORE) is None:
            return (
                stream_response(
                    [tool_call_payload(rid, CALL_READ_STORE, "read", {"path": "src/store.rs"})]
                ),
                "read-store",
            )

        # 5. Repair the defect through the edit tool (exact replacement).
        if defect_present:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_EDIT_STORE,
                            "edit",
                            {
                                "path": "src/store.rs",
                                "edits": [
                                    {"oldText": DEFECT_MARKER, "newText": FIXED_MARKER}
                                ],
                            },
                        )
                    ]
                ),
                "edit-store",
            )

        # Repair milestones land once the fix is applied.
        if todo_result_for(body, CALL_TODO_DONE_REPAIR_INSPECT) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_TODO_DONE_REPAIR_INSPECT,
                            "todo",
                            {"op": "done", "task": "inspect store module"},
                        )
                    ]
                ),
                "todo-done-repair-inspect",
            )
        if todo_result_for(body, CALL_TODO_DONE_REPAIR_FIX) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_TODO_DONE_REPAIR_FIX,
                            "todo",
                            {"op": "done", "task": "fix deliberate marker-parse defect"},
                        )
                    ]
                ),
                "todo-done-repair-fix",
            )

        # 6. The second cargo test MUST pass.
        if todo_result_for(body, CALL_BASH_TEST_PASS) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_BASH_TEST_PASS,
                            "bash",
                            {"command": BASH_CARGO_TEST, "timeout": 600},
                        )
                    ]
                ),
                "bash-test-pass",
            )
        _name, pass_text = tool_result_for(body, CALL_BASH_TEST_PASS)  # type: ignore[misc]
        if "test result: ok" not in pass_text:
            log(
                f"project-authoring request#{request_number} FAIL-CLOSED: "
                f"second cargo test did not pass: {pass_text!r}"
            )
            return (
                stream_response(
                    [text_payload(rid, "cargo test did not pass after the fix; refusing to continue")]
                ),
                "fail-closed-test-pass",
            )

        if todo_result_for(body, CALL_TODO_DONE_PASS_TEST) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_TODO_DONE_PASS_TEST,
                            "todo",
                            {"op": "done", "task": "pass cargo test"},
                        )
                    ]
                ),
                "todo-done-pass-test",
            )

        # 7. Valid CLI run (build + add + done + list in one chain).
        if todo_result_for(body, CALL_BASH_VALID) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_BASH_VALID,
                            "bash",
                            {"command": BASH_VALID_RUN, "timeout": 300},
                        )
                    ]
                ),
                "bash-valid",
            )
        _name, valid_text = tool_result_for(body, CALL_BASH_VALID)  # type: ignore[misc]
        for needle in ["added 0: buy milk", "completed 0", "0 [x]: buy milk"]:
            if needle not in valid_text:
                log(
                    f"project-authoring request#{request_number} FAIL-CLOSED: "
                    f"valid CLI run missing {needle!r}: {valid_text!r}"
                )
                return (
                    stream_response(
                        [text_payload(rid, f"valid CLI run missing {needle!r}; refusing to continue")]
                    ),
                    "fail-closed-valid",
                )

        if todo_result_for(body, CALL_TODO_DONE_VALID) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_TODO_DONE_VALID,
                            "todo",
                            {"op": "done", "task": "exercise CLI on valid input"},
                        )
                    ]
                ),
                "todo-done-valid",
            )

        # 8. Invalid CLI runs (both exit non-zero with actionable stderr).
        if todo_result_for(body, CALL_BASH_INVALID_1) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_BASH_INVALID_1,
                            "bash",
                            {"command": BASH_INVALID_UNKNOWN, "timeout": 120},
                        )
                    ]
                ),
                "bash-invalid-1",
            )
        if todo_result_for(body, CALL_BASH_INVALID_2) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_BASH_INVALID_2,
                            "bash",
                            {"command": BASH_INVALID_BAD_ID, "timeout": 120},
                        )
                    ]
                ),
                "bash-invalid-2",
            )
        for call_id, needle in [
            (CALL_BASH_INVALID_1, "unknown command: bogus"),
            (CALL_BASH_INVALID_2, "invalid task id: abc"),
        ]:
            _name, text = tool_result_for(body, call_id)  # type: ignore[misc]
            if "Command exited with code 1" not in text or needle not in text:
                log(
                    f"project-authoring request#{request_number} FAIL-CLOSED: "
                    f"invalid CLI run {call_id} missing {needle!r} or non-zero exit: {text!r}"
                )
                return (
                    stream_response(
                        [text_payload(rid, f"invalid CLI run {call_id} did not fail actionably")]
                    ),
                    "fail-closed-invalid",
                )

        if todo_result_for(body, CALL_TODO_DONE_INVALID) is None:
            return (
                stream_response(
                    [
                        tool_call_payload(
                            rid,
                            CALL_TODO_DONE_INVALID,
                            "todo",
                            {"op": "done", "task": "exercise CLI on invalid input"},
                        )
                    ]
                ),
                "todo-done-invalid",
            )

        # 9. Every step complete: the final assistant text ends the turn.
        return (
            stream_response([text_payload(rid, FINAL_TEXT)]),
            "final",
        )

    def log_message(self, _format: str, *_args: object) -> None:
        return


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port-file", required=True)
    parser.add_argument("--workspace", required=True)
    args = parser.parse_args()
    ProjectAuthoringServer.workspace = Path(args.workspace).resolve()
    if not ProjectAuthoringServer.workspace.is_dir():
        raise SystemExit(f"workspace is not a directory: {args.workspace}")
    server = HTTPServer(("127.0.0.1", 0), ProjectAuthoringServer)
    with open(args.port_file, "w", encoding="utf-8") as handle:
        handle.write(str(server.server_address[1]))
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()

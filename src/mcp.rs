//! MCP-сервер (`ztaskmgr mcp`): книжка как инструменты для ИИ-агентов — Claude Code, opencode,
//! локальных моделей. JSON-RPC 2.0 по stdio, по сообщению на строку. Описания инструментов —
//! по-английски: их лучше понимают любые модели; данные книжки — как есть, по-русски.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::agent::{Agent, parse_date, parse_list};
use crate::clock::Date;
use crate::model::Category;

const PROTOCOL_VERSION: &str = "2025-06-18";

const INSTRUCTIONS: &str = "\
ztaskmgr is the user's personal notebook of tasks. Every date is a page; every page has four \
lists: Работа (work), Дом (home), Личное (personal), Прочее (other). The position of a task in \
its list is its priority. Unfinished tasks move to the next day automatically; on the old page \
they stay marked [>] with the date they moved to. A task can have notes: dated notes (possibly \
multi-line) and subtasks. A task is done only when the user (or you, if asked) marks it — \
subtasks do not complete it.\n\
Tasks are referenced by `ref` from the tool output: a short id, or DATE/LIST/N for tasks that \
have none yet. Prefer ids. Deleted notes are kept in the archive, so nothing is lost.\n\
Some lists and tasks are hidden from you by the user's choice: do not ask about them or try to \
guess them. The user writes in Russian: answer and write notes in the user's language.";

fn tools() -> Value {
    let date = json!({"type": "string", "description": "YYYY-MM-DD, DD.MM, +N/-N (days from today), today, tomorrow, yesterday"});
    let task = json!({"type": "string", "description": "Task ref from tool output: an id like k3x9ab, or DATE/LIST/N"});
    let list = json!({"type": "string", "description": "Работа|Дом|Личное|Прочее (or work|home|personal|other)"});
    json!([
        {
            "name": "list_pages",
            "description": "Dates that have tasks, with counts of open and done tasks. Use to see what periods have entries.",
            "inputSchema": {"type": "object", "properties": {"from": date, "to": date}},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "get_pages",
            "description": "Tasks of one date or a period (e.g. this week), grouped by date and list, with refs and a notes marker. For a period only days with tasks are shown.",
            "inputSchema": {"type": "object", "properties": {
                "from": date,
                "to": {"type": "string", "description": "End date (inclusive); omit for one day"},
                "lists": {"type": "array", "items": list, "description": "Only these lists"},
                "with_notes": {"type": "boolean", "description": "Include each task's notes"}
            }, "required": ["from"]},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "search",
            "description": "Find tasks by text in the task or its notes (case-insensitive), optionally within dates.",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}, "from": date, "to": date}, "required": ["query"]},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "get_task",
            "description": "One task in full: list, page, state, history across pages it moved through, and all its notes with entry numbers.",
            "inputSchema": {"type": "object", "properties": {"task": task}, "required": ["task"]},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "add_task",
            "description": "Add a task to the end of a list on a date (the end is the lowest priority).",
            "inputSchema": {"type": "object", "properties": {"date": date, "list": list, "text": {"type": "string", "description": "One line"}}, "required": ["date", "list", "text"]}
        },
        {
            "name": "update_task",
            "description": "Change a task: text, done/open, move to another list, or position in its list (1 = top priority).",
            "inputSchema": {"type": "object", "properties": {
                "task": task,
                "text": {"type": "string"},
                "done": {"type": "boolean"},
                "list": list,
                "position": {"type": "integer", "minimum": 1}
            }, "required": ["task"]}
        },
        {
            "name": "postpone_task",
            "description": "Move an open task to another date. It is copied to that page with its notes; the old page keeps it marked [>].",
            "inputSchema": {"type": "object", "properties": {"task": task, "date": date}, "required": ["task", "date"]}
        },
        {
            "name": "delete_task",
            "description": "Delete a task from its page. Its notes go to the archive if no other copy of the task is left.",
            "inputSchema": {"type": "object", "properties": {"task": task}, "required": ["task"]},
            "annotations": {"destructiveHint": true}
        },
        {
            "name": "add_note",
            "description": "Add to a task's notes: a dated note (default; the time is added automatically, may be multi-line) or a one-line subtask.",
            "inputSchema": {"type": "object", "properties": {
                "task": task,
                "text": {"type": "string"},
                "kind": {"type": "string", "enum": ["note", "subtask"]}
            }, "required": ["task", "text"]}
        },
        {
            "name": "update_note",
            "description": "Change a notes entry by its number from get_task: new text and/or mark a subtask done.",
            "inputSchema": {"type": "object", "properties": {
                "task": task,
                "entry": {"type": "integer", "minimum": 1},
                "text": {"type": "string"},
                "done": {"type": "boolean"}
            }, "required": ["task", "entry"]}
        },
        {
            "name": "delete_note",
            "description": "Delete a notes entry by its number from get_task; it is kept in the archive.",
            "inputSchema": {"type": "object", "properties": {"task": task, "entry": {"type": "integer", "minimum": 1}}, "required": ["task", "entry"]},
            "annotations": {"destructiveHint": true}
        }
    ])
}

fn str_arg<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str)
}

fn required<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    str_arg(args, name).with_context(|| format!("missing `{name}`"))
}

fn date_arg(args: &Value, name: &str, today: Date) -> Result<Option<Date>> {
    str_arg(args, name).map(|d| parse_date(d, today)).transpose()
}

fn number_arg(args: &Value, name: &str) -> Result<Option<usize>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(|n| Some(n as usize))
            .with_context(|| format!("`{name}` must be a positive integer")),
    }
}

/// Вызов инструмента: текст ответа или ошибка (она уходит агенту как `isError`).
fn call(agent: &Agent, name: &str, args: &Value) -> Result<String> {
    let today = Date::today();
    match name {
        "list_pages" => agent.list_pages(date_arg(args, "from", today)?, date_arg(args, "to", today)?),
        "get_pages" => {
            let from = parse_date(required(args, "from")?, today)?;
            let to = date_arg(args, "to", today)?.unwrap_or(from);
            let lists = match args.get("lists").and_then(Value::as_array) {
                Some(names) => Some(
                    names
                        .iter()
                        .map(|n| parse_list(n.as_str().unwrap_or("")))
                        .collect::<Result<Vec<Category>>>()?,
                ),
                None => None,
            };
            let with_notes = args.get("with_notes").and_then(Value::as_bool).unwrap_or(false);
            agent.get_pages(from, to, lists.as_deref(), with_notes)
        }
        "search" => agent.search(
            required(args, "query")?,
            date_arg(args, "from", today)?,
            date_arg(args, "to", today)?,
        ),
        "get_task" => agent.get_task(required(args, "task")?),
        "add_task" => agent.add_task(
            parse_date(required(args, "date")?, today)?,
            parse_list(required(args, "list")?)?,
            required(args, "text")?,
        ),
        "update_task" => agent.update_task(
            required(args, "task")?,
            str_arg(args, "text"),
            args.get("done").and_then(Value::as_bool),
            str_arg(args, "list").map(parse_list).transpose()?,
            number_arg(args, "position")?,
        ),
        "postpone_task" => agent.postpone_task(required(args, "task")?, parse_date(required(args, "date")?, today)?),
        "delete_task" => agent.delete_task(required(args, "task")?),
        "add_note" => {
            let subtask = match str_arg(args, "kind").unwrap_or("note") {
                "note" => false,
                "subtask" => true,
                other => anyhow::bail!("unknown kind «{other}»: note or subtask"),
            };
            agent.add_note(required(args, "task")?, required(args, "text")?, subtask)
        }
        "update_note" => agent.update_note(
            required(args, "task")?,
            number_arg(args, "entry")?.context("missing `entry`")?,
            str_arg(args, "text"),
            args.get("done").and_then(Value::as_bool),
        ),
        "delete_note" => agent.delete_note(
            required(args, "task")?,
            number_arg(args, "entry")?.context("missing `entry`")?,
        ),
        other => anyhow::bail!("unknown tool «{other}»"),
    }
}

/// Ответ на одно сообщение; `None` — уведомление, отвечать не нужно.
pub fn handle(agent: &Agent, message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "ztaskmgr", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call(agent, name, &args) {
                Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
                Err(e) => json!({"content": [{"type": "text", "text": format!("Error: {e:#}")}], "isError": true}),
            }
        }
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("method not found: {method}")}
            }));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Цикл сервера: читает сообщения из stdin, пишет ответы в stdout до конца ввода.
pub fn serve(agent: &Agent) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(agent, &message),
            Err(e) => Some(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32700, "message": format!("parse error: {e}")}
            })),
        };
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Book;

    fn agent() -> Agent {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Agent::new(Book::new(dir))
    }

    fn call_tool(agent: &Agent, name: &str, args: Value) -> (bool, String) {
        let reply = handle(
            agent,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}),
        )
        .unwrap();
        let result = &reply["result"];
        (
            result["isError"].as_bool().unwrap(),
            result["content"][0]["text"].as_str().unwrap().to_string(),
        )
    }

    #[test]
    fn protocol() {
        let agent = agent();
        let init = handle(
            &agent,
            &json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}),
        )
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "ztaskmgr");
        assert!(handle(&agent, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        let list = handle(&agent, &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"get_pages") && names.contains(&"delete_note"), "{names:?}");
        let unknown = handle(&agent, &json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);

        let (error, text) = call_tool(&agent, "add_task", json!({"date": "today", "list": "work", "text": "отчёт"}));
        assert!(!error, "{text}");
        let id = text.rsplit("ref: ").next().unwrap().trim().to_string();
        let (error, text) = call_tool(&agent, "add_note", json!({"task": id, "text": "начал", "kind": "subtask"}));
        assert!(!error, "{text}");
        let (_, text) = call_tool(&agent, "get_pages", json!({"from": "today", "lists": ["Работа"]}));
        assert!(text.contains("1. [ ] отчёт [notes, subtasks 0/1]"), "{text}");
        let (error, text) = call_tool(&agent, "update_note", json!({"task": id, "entry": 1, "done": true}));
        assert!(!error, "{text}");
        let (error, text) = call_tool(&agent, "get_pages", json!({"from": "потом"}));
        assert!(error && text.starts_with("Error: not a date"), "{text}");
        let (error, _) = call_tool(&agent, "add_note", json!({"task": id, "text": "x", "kind": "idea"}));
        assert!(error);
        let _ = std::fs::remove_dir_all(agent.book().dir());
    }
}

// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! [`BufferTools`] — the agent's read/edit tools over a single buffer.

use architect::{ToolCall, Tools};
use majestic_core::{apply_hashline, tagged_read, Buffer, HashlineEdit, LineRef};
use seraph::AgentAction;
use serde::Deserialize;

/// The agent's tool surface over one buffer: `read` it as `LINE:TAG│text` lines, or `edit` it with
/// tagged edits. The host points this at the active buffer for a turn; the governed loop has already
/// gated every call through Seraph by the time [`Tools::run`] is reached.
#[derive(Debug)]
pub struct BufferTools<'a> {
    buffer: &'a mut Buffer,
}

impl<'a> BufferTools<'a> {
    /// Wraps `buffer` as the agent's tool target.
    pub fn new(buffer: &'a mut Buffer) -> Self {
        Self { buffer }
    }
}

/// The JSON arguments of the `edit` tool: a batch of tagged edits applied atomically.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    edits: Vec<EditArg>,
}

/// One tagged edit as the agent writes it: the 1-based `line` and its `tag` (exactly as shown in a
/// `read`), the operation, and the replacement/inserted `text` (unused by `delete`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArg {
    line: usize,
    tag: String,
    op: EditOp,
    #[serde(default)]
    text: String,
}

/// The edit operation an [`EditArg`] requests.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EditOp {
    /// Replace the cited line's content.
    Replace,
    /// Insert a new line after the cited line.
    InsertAfter,
    /// Delete the cited line.
    Delete,
}

impl EditArg {
    /// Converts to a core [`HashlineEdit`], translating the agent's 1-based line to the 0-based one
    /// the core uses.
    fn into_edit(self) -> HashlineEdit {
        let at = LineRef::new(self.line.saturating_sub(1), self.tag);
        match self.op {
            EditOp::Replace => HashlineEdit::Replace {
                at,
                text: self.text,
            },
            EditOp::InsertAfter => HashlineEdit::InsertAfter {
                at,
                text: self.text,
            },
            EditOp::Delete => HashlineEdit::Delete { at },
        }
    }
}

impl Tools for BufferTools<'_> {
    fn action(&self, call: &ToolCall) -> AgentAction {
        match call.name.as_str() {
            "edit" => AgentAction::Edit,
            // `read` and anything else are gated as a read (low-risk); `run` rejects unknown tools.
            _ => AgentAction::ReadPath {
                path: self.buffer.path().unwrap_or_default(),
            },
        }
    }

    fn run(&mut self, call: &ToolCall) -> Result<String, String> {
        match call.name.as_str() {
            "read" => Ok(tagged_read(self.buffer)),
            "edit" => {
                let args: EditArgs = serde_json::from_value(call.arguments.clone())
                    .map_err(|error| format!("bad edit arguments: {error}"))?;
                let edits: Vec<HashlineEdit> =
                    args.edits.into_iter().map(EditArg::into_edit).collect();
                let count = edits.len();
                apply_hashline(self.buffer, &edits)
                    .map(|()| format!("applied {count} edit(s)"))
                    .map_err(|error| error.to_string())
            }
            other => Err(format!("unknown tool `{other}`")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BufferTools;
    use architect::{ToolCall, Tools};
    use majestic_core::{tagged_read, Buffer};
    use seraph::AgentAction;
    use serde_json::{json, Value};

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: format!("{name}-1"),
            name: name.to_owned(),
            arguments,
        }
    }

    /// The tag `tagged_read` assigned to 0-based `line` of `buffer`.
    fn tag_of(buffer: &Buffer, line: usize) -> String {
        let read = tagged_read(buffer);
        let row = read.lines().nth(line).expect("line present");
        let after_colon = row.split_once(':').expect("colon").1;
        after_colon.split_once('│').expect("separator").0.to_owned()
    }

    #[test]
    fn read_returns_tagged_lines() {
        let mut buffer = Buffer::from_text("alpha\nbeta\ngamma");
        let mut tools = BufferTools::new(&mut buffer);
        let read = tools.run(&call("read", json!({}))).expect("read ok");
        assert!(read.starts_with("1:"));
        assert!(read.contains("│alpha"));
        assert!(read.contains("│gamma"));
    }

    #[test]
    fn edit_applies_with_a_fresh_tag() {
        let mut buffer = Buffer::from_text("alpha\nbeta\ngamma");
        let tag = tag_of(&buffer, 1); // line 2 (1-based)
        let mut tools = BufferTools::new(&mut buffer);
        let edit = call(
            "edit",
            json!({ "edits": [{ "line": 2, "tag": tag, "op": "replace", "text": "BETA" }] }),
        );
        assert_eq!(tools.run(&edit).expect("edit ok"), "applied 1 edit(s)");
        // `tools`'s &mut borrow of the buffer ends at its last use above, so the buffer is readable.
        assert_eq!(buffer.text(), "alpha\nBETA\ngamma");
    }

    #[test]
    fn edit_with_a_stale_tag_is_rejected() {
        let mut buffer = Buffer::from_text("alpha\nbeta\ngamma");
        let mut tools = BufferTools::new(&mut buffer);
        let edit = call(
            "edit",
            json!({ "edits": [{ "line": 2, "tag": "zz", "op": "replace", "text": "BETA" }] }),
        );
        let error = tools.run(&edit).expect_err("stale tag rejected");
        assert!(
            error.contains("changed") || error.contains("re-read"),
            "got: {error}"
        );
    }

    #[test]
    fn unknown_tool_errors() {
        let mut buffer = Buffer::from_text("x");
        let mut tools = BufferTools::new(&mut buffer);
        let error = tools
            .run(&call("frobnicate", json!({})))
            .expect_err("unknown");
        assert!(error.contains("unknown tool"));
    }

    #[test]
    fn action_classifies_edit_as_edit_and_read_as_read() {
        let mut buffer = Buffer::from_text("x");
        let tools = BufferTools::new(&mut buffer);
        assert_eq!(tools.action(&call("edit", json!({}))), AgentAction::Edit);
        assert!(matches!(
            tools.action(&call("read", json!({}))),
            AgentAction::ReadPath { .. }
        ));
    }
}

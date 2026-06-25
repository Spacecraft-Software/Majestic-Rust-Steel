// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! [`BufferTools`] — the agent's read/edit tools over a single buffer.

use architect::{ToolCall, ToolSpec, Tools};
use majestic_core::{apply_hashline, tagged_read, Buffer, HashlineEdit, LineRef};
use seraph::AgentAction;
use serde::Deserialize;
use serde_json::json;

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

/// One edit's before/after preview for the approval UI: the cited 1-based `line`, the line's current
/// content (`old`, absent for an insertion), and the proposed content (`new`, absent for a deletion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditPreview {
    /// The 1-based line the edit cites (as the model wrote it).
    pub line: usize,
    /// The line's current content — `Some` for replace/delete, `None` for an insertion.
    pub old: Option<String>,
    /// The proposed content — `Some` for replace/insert, `None` for a deletion.
    pub new: Option<String>,
}

/// Previews the `edit` tool `call` against `buffer` for the approval dialog — the before/after of each
/// edit, keyed by current line number (it does not re-check tags; the gated [`BufferTools::run`] does
/// that on apply). Returns an empty list if `call` is not a well-formed `edit`.
#[must_use]
pub fn preview_edits(buffer: &Buffer, call: &ToolCall) -> Vec<EditPreview> {
    if call.name != "edit" {
        return Vec::new();
    }
    let Ok(args) = serde_json::from_value::<EditArgs>(call.arguments.clone()) else {
        return Vec::new();
    };
    let rope = buffer.rope();
    let line_count = rope.len_lines();
    args.edits
        .into_iter()
        .map(|edit| {
            // The agent cites 1-based lines; the rope is 0-based. Guard the lookup so a bad line
            // number previews as an insertion-like change rather than panicking.
            let row = edit.line.saturating_sub(1);
            let current = (row < line_count).then(|| rope.line(row));
            match edit.op {
                EditOp::Replace => EditPreview {
                    line: edit.line,
                    old: current,
                    new: Some(edit.text),
                },
                EditOp::InsertAfter => EditPreview {
                    line: edit.line,
                    old: None,
                    new: Some(edit.text),
                },
                EditOp::Delete => EditPreview {
                    line: edit.line,
                    old: current,
                    new: None,
                },
            }
        })
        .collect()
}

/// The function-calling spec for the `shell` tool: run one program (no shell features) in the project
/// directory. The host advertises it only when the policy permits some shell (a non-empty allow-list);
/// every invocation is still policy-gated and user-approved, and runs in the [`seraph::Sandbox`].
#[must_use]
pub fn shell_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "shell".to_owned(),
        description: "Run a single program in the project directory and return its output. There is \
                      NO shell: pipes, redirects, `;`/`&&` chaining, globs, and `$(…)` do not work — \
                      pass one program and its arguments. The program must be on the allow-list and \
                      the user approves every command."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "the command line, e.g. `cargo test` or `git status`"
                }
            },
            "required": ["command"]
        }),
    }
}

/// The function-calling specs the agent advertises for [`BufferTools`]: `read` and `edit`. The host
/// passes these to the provider so the model knows the tools and the exact hashline edit shape.
#[must_use]
pub fn buffer_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read".to_owned(),
            description: "Read the current buffer as `LINE:TAG│text` lines. Cite a line's LINE and \
                          TAG when you edit it."
                .to_owned(),
            parameters: json!({ "type": "object", "properties": {} }),
        },
        ToolSpec {
            name: "edit".to_owned(),
            description: "Apply edits to the current buffer. Each edit cites a line by its 1-based \
                          LINE and the TAG shown by `read`; if the tag is stale the edit is rejected \
                          and you must re-read first."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "line": { "type": "integer", "description": "1-based line number" },
                                "tag": { "type": "string", "description": "the line's hashline tag from `read`" },
                                "op": { "type": "string", "enum": ["replace", "insert_after", "delete"] },
                                "text": { "type": "string", "description": "new content (omit for delete)" }
                            },
                            "required": ["line", "tag", "op"]
                        }
                    }
                },
                "required": ["edits"]
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{preview_edits, BufferTools, EditPreview};
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

    #[test]
    fn preview_edits_shows_before_and_after_per_op() {
        let buffer = Buffer::from_text("alpha\nbeta\ngamma");
        let edit = call(
            "edit",
            json!({ "edits": [
                { "line": 1, "tag": "x", "op": "replace", "text": "ALPHA" },
                { "line": 2, "tag": "y", "op": "insert_after", "text": "inserted" },
                { "line": 3, "tag": "z", "op": "delete" }
            ] }),
        );
        let preview = preview_edits(&buffer, &edit);
        assert_eq!(
            preview,
            vec![
                EditPreview {
                    line: 1,
                    old: Some("alpha".to_owned()),
                    new: Some("ALPHA".to_owned()),
                },
                EditPreview {
                    line: 2,
                    old: None,
                    new: Some("inserted".to_owned()),
                },
                EditPreview {
                    line: 3,
                    old: Some("gamma".to_owned()),
                    new: None,
                },
            ]
        );
    }

    #[test]
    fn preview_edits_ignores_non_edit_and_malformed_calls() {
        let buffer = Buffer::from_text("x");
        assert!(preview_edits(&buffer, &call("read", json!({}))).is_empty());
        assert!(preview_edits(&buffer, &call("edit", json!({ "wrong": 1 }))).is_empty());
    }

    /// M3 exit criterion (hashline TOCTOU safety): an edit citing a tag that has gone stale because
    /// the line changed since the read is rejected, and the buffer is left byte-for-byte unchanged —
    /// no edit-against-a-changed-buffer ever lands.
    #[test]
    fn m3_exit_stale_tag_edit_is_rejected_and_leaves_the_buffer_unchanged() {
        let mut buffer = Buffer::from_text("alpha\nbeta\ngamma");
        let tag = tag_of(&buffer, 1); // line 2's tag, as a first `read` showed it

        // A valid edit changes line 2, so the captured `tag` (for "beta") is now stale. The inline
        // `BufferTools` temporary releases its &mut borrow at the end of the statement.
        let _applied = BufferTools::new(&mut buffer)
            .run(&call(
                "edit",
                json!({ "edits": [{ "line": 2, "tag": tag, "op": "replace", "text": "BETA" }] }),
            ))
            .expect("the fresh-tag edit applies");
        let after_valid_edit = buffer.text();
        assert_eq!(after_valid_edit, "alpha\nBETA\ngamma");

        // A second edit citing the now-stale tag must be rejected and change nothing.
        let error = BufferTools::new(&mut buffer)
            .run(&call(
                "edit",
                json!({ "edits": [{ "line": 2, "tag": tag, "op": "replace", "text": "CLOBBER" }] }),
            ))
            .expect_err("a stale-tag edit must be rejected");
        assert!(
            error.contains("changed") || error.contains("re-read"),
            "rejection should tell the agent to re-read: {error}"
        );
        assert_eq!(
            buffer.text(),
            after_valid_edit,
            "a stale-tag edit must not modify the buffer"
        );
    }
}

//! Canonical tool definitions and tool-choice policy.
//!
//! Inbound adapters (②) normalize each protocol's tool schema into these types;
//! the Raycast builder (④) renders them into Raycast's `tools[]` shape
//! (`local_tool` with a JSON Schema, or `remote_tool` by name). The builder
//! may also *synthesize* a `remote_tool` entry the client never sent — the
//! `image_generation` remote tool, injected when the model advertises the
//! ability — so canonical tools are not the complete wire tool set.

use serde_json::Value;

/// A tool the model may call.
#[derive(Debug, Clone, serde::Serialize)]
pub enum ToolDef {
    /// A locally-executed tool with a full JSON Schema for its parameters.
    Local {
        name: String,
        description: Option<String>,
        /// JSON Schema for the tool's parameters.
        parameters: Value,
    },
    /// A locally-executed freeform tool (codex's `exec`): the model supplies
    /// raw text (JavaScript source) rather than JSON arguments. Bridged over
    /// the upstream as a single `input` string parameter, and back to the
    /// client as a `custom_tool_call` item.
    Custom {
        name: String,
        description: Option<String>,
        /// The tool's free-text input grammar, folded into the description.
        grammar: String,
    },
    /// A backend-provided tool referenced by name only (e.g. `web_search`).
    Remote { name: String },
}

impl ToolDef {
    /// The tool's name, across all three kinds.
    pub fn name(&self) -> &str {
        match self {
            ToolDef::Local { name, .. }
            | ToolDef::Custom { name, .. }
            | ToolDef::Remote { name } => name,
        }
    }
}

/// How the model should decide whether to call a tool.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum ToolChoice {
    /// Model decides freely (the default).
    Auto,
    /// Model must not call a tool.
    None,
    /// Model must call some tool.
    Required,
    /// Model must call the named tool.
    Named(String),
}

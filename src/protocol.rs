use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
#[derive(Clone, Debug, Deserialize)]
pub struct MessageRequest {
    pub model: String,
    pub max_tokens: u64,
    #[serde(default, deserialize_with = "system_blocks")]
    pub system: Vec<SystemBlock>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<Tool>,
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub stream: bool,
    pub stop_sequences: Option<Vec<String>>,
    pub output_config: Option<OutputConfig>,
    #[serde(flatten)]
    pub unknown: HashMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum SystemBlock {
    Text(String),
    Block {
        #[serde(rename = "type")]
        kind: String,
        text: String,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
}
fn system_blocks<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<SystemBlock>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(SystemBlock),
        Many(Vec<SystemBlock>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(v) => vec![v],
        OneOrMany::Many(v) => v,
    })
}
#[derive(Clone, Debug, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Content,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    ToolResult {
        tool_use_id: String,
        content: Option<Value>,
        #[serde(default)]
        is_error: bool,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    Image {
        source: ImageSource,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}
#[derive(Clone, Debug, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct OutputConfig {
    pub effort: Option<String>,
    pub format: Option<Value>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

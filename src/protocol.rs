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
#[derive(Clone, Debug, Default, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl Usage {
    pub fn from_openai(value: &Value) -> Self {
        let total = value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_read_input_tokens = value
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_creation_input_tokens = value
            .pointer("/input_tokens_details/cache_write_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Self {
            input_tokens: total.saturating_sub(
                cache_read_input_tokens.saturating_add(cache_creation_input_tokens),
            ),
            output_tokens: value
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_creation_input_tokens,
            cache_read_input_tokens,
        }
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_openai_cache_usage_without_double_counting_input() {
        let usage = Usage::from_openai(&json!({
            "input_tokens": 100,
            "output_tokens": 9,
            "input_tokens_details": {
                "cached_tokens": 60,
                "cache_write_tokens": 25
            }
        }));
        assert_eq!(usage.input_tokens, 15);
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(usage.cache_creation_input_tokens, 25);
        assert_eq!(usage.cache_read_input_tokens, 60);
    }
}

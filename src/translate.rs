use crate::{config::Model, error::ApiError, protocol::*};
use serde_json::{Value, json};
pub fn request(r: &MessageRequest, m: &Model) -> Result<Value, ApiError> {
    if r.max_tokens > m.output_limit {
        return Err(ApiError::validation(format!(
            "max_tokens exceeds {} limit",
            m.output_limit
        )));
    }
    if r.tools.len() > 128 {
        return Err(ApiError::validation("tool count exceeds 128"));
    }
    let tool_schemas = r
        .tools
        .iter()
        .map(tool_schema)
        .collect::<Result<Vec<_>, _>>()?;
    if tool_schemas
        .iter()
        .any(|schema| serde_json::to_vec(schema).map_or(true, |value| value.len() > 1024 * 1024))
    {
        return Err(ApiError::validation("tool schema exceeds 1 MiB"));
    }
    let effort = r.output_config.as_ref().and_then(|o| o.effort.as_deref());
    if let Some(e) = effort
        && !m.efforts.iter().any(|v| v == e)
    {
        return Err(ApiError::validation(format!(
            "effort '{e}' is unsupported by {}; supported: {}",
            r.model,
            m.efforts.join(", ")
        )));
    }
    let format = r
        .output_config
        .as_ref()
        .and_then(|output| output.format.clone())
        .map(translate_format)
        .transpose()?;
    if format.is_some() && !m.structured_output {
        return Err(ApiError::validation(format!(
            "structured output is unsupported by {}",
            r.model
        )));
    }
    let mut input = Vec::new();
    for msg in &r.messages {
        match &msg.content { Content::Text(t)=>input.push(json!({"role":msg.role,"content":[{"type":if msg.role=="assistant"{"output_text"}else{"input_text"},"text":t}]})), Content::Blocks(bs)=>for b in bs {match b {
  ContentBlock::Text{text,..}=>input.push(json!({"role":msg.role,"content":[{"type":if msg.role=="assistant"{"output_text"}else{"input_text"},"text":text}]})),
  ContentBlock::ToolUse{id,name,input:v,..}=>input.push(json!({"type":"function_call","call_id":id,"name":name,"arguments":serde_json::to_string(v).unwrap_or_else(|_|"{}".into())})),
  ContentBlock::ToolResult{tool_use_id,content,is_error,..}=>input.push(json!({"type":"function_call_output","call_id":tool_use_id,"output":tool_output(content.as_ref(),*is_error)})),
  ContentBlock::Image{source,..}=>{let url=match source{ImageSource::Base64{media_type,data}=>format!("data:{media_type};base64,{data}"),ImageSource::Url{url}=>url.clone()};input.push(json!({"role":"user","content":[{"type":"input_image","image_url":url}]}));},
  ContentBlock::Thinking{signature:Some(s),..}=>{if let Ok(v)=serde_json::from_str::<Value>(s){input.push(v)}}, _=>{}
 }}}
    }
    let instructions = r
        .system
        .iter()
        .map(|b| match b {
            SystemBlock::Text(s) => s.as_str(),
            SystemBlock::Block { text, .. } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let tools=r.tools.iter().zip(tool_schemas).map(|(tool,schema)|json!({"type":"function","name":tool.name,"description":tool.description,"parameters":strip_annotations(schema),"strict":false})).collect::<Vec<_>>();
    let mut out = json!({"model":m.upstream,"instructions":instructions,"input":input,"tools":tools,"tool_choice":"auto","parallel_tool_calls":true,"stream":true,"store":false,"include":["reasoning.encrypted_content"]});
    if let Some(e) = effort {
        out["reasoning"] = json!({"effort":e});
    }
    if let Some(f) = format {
        out["text"] = json!({"format":f});
    }
    if let Some(c) = &r.tool_choice {
        out["tool_choice"] = translate_choice(c)?;
    }
    Ok(out)
}
fn translate_format(mut format: Value) -> Result<Value, ApiError> {
    let object = format
        .as_object_mut()
        .ok_or_else(|| ApiError::validation("output format must be a JSON object"))?;
    if object.get("type").and_then(Value::as_str) == Some("json_schema") {
        object
            .entry("name")
            .or_insert_with(|| json!("claude_structured_output"));
        object.entry("strict").or_insert_with(|| json!(false));
    }
    Ok(format)
}

fn tool_schema(tool: &Tool) -> Result<Value, ApiError> {
    if let Some(schema) = &tool.input_schema {
        return Ok(schema.clone());
    }
    match tool.name.as_str() {
        "WebSearch" | "web_search" => Ok(json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 2},
                "allowed_domains": {"type": "array", "items": {"type": "string"}},
                "blocked_domains": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["query"],
            "additionalProperties": false
        })),
        "WebFetch" | "web_fetch" => Ok(json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "format": "uri"},
                "prompt": {"type": "string"}
            },
            "required": ["url", "prompt"],
            "additionalProperties": false
        })),
        _ => Err(ApiError::validation(format!(
            "tool '{}' is missing input_schema",
            tool.name
        ))),
    }
}

fn tool_output(v: Option<&Value>, err: bool) -> String {
    let x = v.cloned().unwrap_or(Value::String(String::new()));
    let s = match x {
        Value::String(s) => s,
        v => serde_json::to_string(&v).unwrap_or_default(),
    };
    if err { format!("Error: {s}") } else { s }
}
fn strip_annotations(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        for k in ["cache_control", "input_examples", "defer_loading"] {
            o.remove(k);
        }
        for x in o.values_mut() {
            *x = strip_annotations(x.take());
        }
    } else if let Some(a) = v.as_array_mut() {
        for x in a {
            *x = strip_annotations(x.take());
        }
    }
    v
}
fn translate_choice(v: &Value) -> Result<Value, ApiError> {
    match v.get("type").and_then(Value::as_str) {
        Some("auto") => Ok(json!("auto")),
        Some("any") => Ok(json!("required")),
        Some("none") => Ok(json!("none")),
        Some("tool") => v
            .get("name")
            .map(|n| json!({"type":"function","name":n}))
            .ok_or_else(|| ApiError::validation("tool_choice.tool requires name")),
        _ => Err(ApiError::validation("unsupported tool_choice")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn model() -> Model {
        Model {
            alias: "claude-codex".into(),
            display_name: "Codex".into(),
            upstream: "gpt-test".into(),
            efforts: vec!["low".into(), "high".into()],
            context_limit: 1000,
            output_limit: 100,
            structured_output: true,
        }
    }
    #[test]
    fn translates_text_effort_and_storage() {
        let r:MessageRequest=serde_json::from_value(json!({"model":"claude-codex","max_tokens":50,"system":[{"type":"text","text":"first"},{"type":"text","text":"second","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"hello"}],"output_config":{"effort":"high"}})).unwrap();
        let v = request(&r, &model()).unwrap();
        assert_eq!(v["model"], "gpt-test");
        assert_eq!(v["instructions"], "first\nsecond");
        assert_eq!(v["reasoning"]["effort"], "high");
        assert_eq!(v["store"], false);
    }
    #[test]
    fn supplies_responses_fields_for_claude_json_schema_format() {
        let message_request: MessageRequest = serde_json::from_value(json!({
            "model": "claude-codex",
            "max_tokens": 50,
            "messages": [{"role": "user", "content": "title"}],
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {"title": {"type": "string"}},
                        "required": ["title"],
                        "additionalProperties": false
                    }
                }
            }
        }))
        .unwrap();
        let translated = request(&message_request, &model()).unwrap();
        assert_eq!(
            translated["text"]["format"]["name"],
            "claude_structured_output"
        );
        assert_eq!(translated["text"]["format"]["strict"], false);
    }

    #[test]
    fn preserves_tools_and_results() {
        let r:MessageRequest=serde_json::from_value(json!({"model":"claude-codex","max_tokens":50,"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"read","input":{"path":"x"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"ok"}]}],"tools":[{"name":"read","description":"read","input_schema":{"type":"object","cache_control":{},"properties":{"path":{"type":"string"}}}}],"tool_choice":{"type":"tool","name":"read"}})).unwrap();
        let v = request(&r, &model()).unwrap();
        assert_eq!(v["input"][0]["type"], "function_call");
        assert_eq!(v["input"][1]["type"], "function_call_output");
        assert!(v["tools"][0]["parameters"].get("cache_control").is_none());
    }
    #[test]
    fn supplies_known_web_tool_schemas_when_claude_omits_them() {
        let request: MessageRequest = serde_json::from_value(json!({
            "model": "claude-codex",
            "max_tokens": 50,
            "messages": [{"role": "user", "content": "search"}],
            "tools": [
                {"type": "web_search_20250305", "name": "WebSearch"},
                {"type": "web_fetch_20250910", "name": "WebFetch"}
            ]
        }))
        .unwrap();
        let translated = super::request(&request, &model()).unwrap();
        assert_eq!(
            translated["tools"][0]["parameters"]["required"],
            json!(["query"])
        );
        assert_eq!(
            translated["tools"][1]["parameters"]["required"],
            json!(["url", "prompt"])
        );
    }

    #[test]
    fn rejects_unknown_tools_without_schemas() {
        let request: MessageRequest = serde_json::from_value(json!({
            "model": "claude-codex",
            "max_tokens": 50,
            "messages": [],
            "tools": [{"name": "unknown"}]
        }))
        .unwrap();
        assert!(
            super::request(&request, &model())
                .unwrap_err()
                .message
                .contains("missing input_schema")
        );
    }

    #[test]
    fn rejects_unsupported_effort() {
        let r:MessageRequest=serde_json::from_value(json!({"model":"claude-codex","max_tokens":50,"messages":[],"output_config":{"effort":"max"}})).unwrap();
        assert!(
            request(&r, &model())
                .unwrap_err()
                .message
                .contains("supported")
        );
    }
}

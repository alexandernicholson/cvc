use crate::{
    error::{ApiError, from_upstream_event},
    protocol::Usage,
};
use axum::response::sse::Event;
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Default)]
pub struct Machine {
    started: bool,
    next_index: u64,
    blocks: HashMap<String, Block>,
    usage: Usage,
    saw_tool: bool,
}
struct Block {
    index: u64,
    kind: &'static str,
    args: String,
}

impl Machine {
    pub fn apply(&mut self, event: &Value) -> Result<Vec<Event>, ApiError> {
        let ty = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(ty, "response.failed" | "error") {
            let error = from_upstream_event(event);
            return Ok(vec![sse(
                "error",
                json!({"type":"error","error":{"type":error.kind,"message":error.message}}),
            )]);
        }
        let mut out = Vec::new();
        if !self.started {
            let id = event
                .pointer("/response/id")
                .and_then(Value::as_str)
                .unwrap_or("msg_cvc");
            let model = event
                .pointer("/response/model")
                .and_then(Value::as_str)
                .unwrap_or("codex");
            out.push(sse("message_start", json!({"type":"message_start","message":{"id":id,"type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":Usage::default()}})));
            self.started = true;
        }
        match ty {
            "response.output_item.added" => {
                let item = &event["item"];
                let id = item
                    .get("id")
                    .or_else(|| item.get("call_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("item")
                    .to_owned();
                let kind = match item.get("type").and_then(Value::as_str) {
                    Some("function_call") => "tool_use",
                    Some("reasoning") => "thinking",
                    _ => "text",
                };
                let index = self.next_index;
                self.next_index += 1;
                let content = match kind {
                    "tool_use" => {
                        self.saw_tool = true;
                        json!({"type":"tool_use","id":item.get("call_id").and_then(Value::as_str).unwrap_or(&id),"name":item.get("name").and_then(Value::as_str).unwrap_or(""),"input":{}})
                    }
                    "thinking" => json!({"type":"thinking","thinking":"","signature":""}),
                    _ => json!({"type":"text","text":""}),
                };
                self.blocks.insert(
                    id,
                    Block {
                        index,
                        kind,
                        args: String::new(),
                    },
                );
                out.push(sse(
                    "content_block_start",
                    json!({"type":"content_block_start","index":index,"content_block":content}),
                ));
            }
            "response.output_text.delta" => out.push(self.delta(event, "text_delta", "text")?),
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                out.push(self.delta(event, "thinking_delta", "thinking")?)
            }
            "response.function_call_arguments.delta" => {
                let block = self
                    .blocks
                    .get_mut(item_id(event))
                    .ok_or_else(|| ApiError::server("upstream tool delta without block"))?;
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                block.args.push_str(delta);
                out.push(sse("content_block_delta", json!({"type":"content_block_delta","index":block.index,"delta":{"type":"input_json_delta","partial_json":delta}})));
            }
            "response.output_item.done" => {
                if let Some(block) = self.blocks.remove(item_id(event)) {
                    if block.kind == "tool_use"
                        && serde_json::from_str::<serde_json::Map<String, Value>>(&block.args)
                            .is_err()
                    {
                        return Err(ApiError::server(
                            "upstream tool arguments are not a JSON object",
                        ));
                    }
                    out.push(sse(
                        "content_block_stop",
                        json!({"type":"content_block_stop","index":block.index}),
                    ));
                }
            }
            "response.completed" => {
                let response = &event["response"];
                self.usage = Usage::from_openai(&response["usage"]);
                let stop = if self.saw_tool {
                    "tool_use"
                } else if response.get("status").and_then(Value::as_str) == Some("incomplete") {
                    "max_tokens"
                } else {
                    "end_turn"
                };
                let mut remaining = self.blocks.drain().map(|(_, b)| b).collect::<Vec<_>>();
                remaining.sort_by_key(|b| b.index);
                for block in remaining {
                    out.push(sse(
                        "content_block_stop",
                        json!({"type":"content_block_stop","index":block.index}),
                    ));
                }
                out.push(sse("message_delta", json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":self.usage})));
                out.push(sse("message_stop", json!({"type":"message_stop"})));
            }
            _ => {}
        }
        Ok(out)
    }
    fn delta(&self, event: &Value, kind: &str, key: &str) -> Result<Event, ApiError> {
        let block = self
            .blocks
            .get(item_id(event))
            .ok_or_else(|| ApiError::server("upstream delta without block"))?;
        let mut delta = serde_json::Map::new();
        delta.insert("type".into(), json!(kind));
        delta.insert(
            key.into(),
            json!(
                event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ),
        );
        Ok(sse(
            "content_block_delta",
            json!({"type":"content_block_delta","index":block.index,"delta":delta}),
        ))
    }
}
fn item_id(event: &Value) -> &str {
    event
        .get("item_id")
        .and_then(Value::as_str)
        .or_else(|| event.pointer("/item/id").and_then(Value::as_str))
        .or_else(|| event.pointer("/item/call_id").and_then(Value::as_str))
        .unwrap_or("item")
}
fn sse(name: &str, data: Value) -> Event {
    Event::default().event(name).json_data(data).unwrap()
}

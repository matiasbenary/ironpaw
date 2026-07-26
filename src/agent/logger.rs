use crate::{
    inference::{InferenceOutput, InferenceRequest},
    message::{Message, ToolCall},
};

const LOG_PATH: &str = "/Users/gagdiez/tmp/miniclaw/release/llm_requests.log";

pub(super) fn raw_inference_request_logged(request: &InferenceRequest) {
    let Ok(request) = serde_json::to_string(request) else {
        return;
    };
    append(&format!("=== RAW LLM REQUEST ===\n{request}"));
}

pub(super) fn raw_inference_response_logged(response: &InferenceOutput) {
    let Ok(response) = serde_json::to_string(response) else {
        return;
    };
    append(&format!("=== RAW LLM RESPONSE ===\n{response}"));
}

pub(super) fn raw_tool_call_logged(tool_call: &ToolCall) {
    append(&format!(
        "=== RAW TOOL CALL ===\nid: {}\nname: {}\narguments: {}",
        tool_call.id, tool_call.name, tool_call.arguments
    ));
}

pub(super) fn raw_tool_results_logged(messages: &[Message]) {
    for message in messages {
        let Message::ToolResult {
            tool_call_id,
            tool_name,
            result,
        } = message
        else {
            continue;
        };
        append(&format!(
            "=== RAW TOOL RESULT ===\nid: {tool_call_id}\nname: {tool_name}\nresult: {result}"
        ));
    }
}

fn append(entry: &str) {
    if entry.is_empty() {
        return;
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
        .and_then(|mut file| {
            use std::io::Write as _;
            writeln!(file, "{entry}\n")
        });
}

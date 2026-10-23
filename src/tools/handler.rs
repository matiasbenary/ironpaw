use std::sync::Arc;

use crate::{
    bus::{Bus, Event},
    message::ToolCall,
    tools::{
        ExecutionContext, ToolCallResult, ToolError, ToolOutcome, ToolRegistry, ToolResult,
        runner::execute_tool_outcome,
    },
};

pub struct ToolHandler {
    bus: Bus,
    registry: Arc<ToolRegistry>,
}

impl ToolHandler {
    pub fn new(bus: Bus, registry: Arc<ToolRegistry>) -> Self {
        Self { bus, registry }
    }

    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        let mut events = self.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(Event::ToolRequest {
                        request_id,
                        context,
                        tool_calls,
                        allowed_tool_names,
                    }) => {
                        this.spawn_tool_request(request_id, context, tool_calls, allowed_tool_names)
                    }
                    Ok(Event::MessageIn(message))
                        if message.sender.kind() == crate::bus::SenderKind::User =>
                    {
                        let approvals = crate::tools::approval::Approvals::global();
                        approvals.note_user_conversation(&message.conversation);
                        approvals.resolve(&message.conversation, &message.text);
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    fn spawn_tool_request(
        &self,
        request_id: String,
        context: ExecutionContext,
        tool_calls: Vec<ToolCall>,
        allowed_tool_names: Option<Vec<String>>,
    ) {
        let bus = self.bus.clone();
        let registry = Arc::clone(&self.registry);
        tokio::spawn(async move {
            let outcome = execute_tool_request(
                registry.as_ref(),
                &context,
                tool_calls,
                allowed_tool_names.as_deref(),
            )
            .await;
            bus.publish(Event::ToolResponse {
                request_id,
                outcome,
            });
        });
    }
}

async fn execute_tool_request(
    registry: &ToolRegistry,
    context: &ExecutionContext,
    tool_calls: Vec<ToolCall>,
    allowed_tool_names: Option<&[String]>,
) -> ToolOutcome<Vec<ToolCallResult>> {
    let mut results = Vec::with_capacity(tool_calls.len());
    let mut outcome = ToolOutcome::Result(());
    for tool_call in &tool_calls {
        let result = match execute_tool_call(registry, context, tool_call, allowed_tool_names).await
        {
            ToolOutcome::Result(result) => result,
            ToolOutcome::EndTurn(result) => {
                outcome = ToolOutcome::EndTurn(());
                result
            }
        };
        results.push(ToolCallResult {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            result,
        });
    }
    outcome.map(|()| results)
}

async fn execute_tool_call(
    registry: &ToolRegistry,
    context: &ExecutionContext,
    tool_call: &ToolCall,
    allowed_tool_names: Option<&[String]>,
) -> ToolOutcome<ToolResult> {
    if allowed_tool_names.is_some_and(|names| !names.iter().any(|name| name == &tool_call.name)) {
        return ToolOutcome::Result(Err(ToolError::new("tool is not allowed in this agent run")));
    }
    let args = match serde_json::from_str(&tool_call.arguments) {
        Ok(args) => unstringify_nested_args(registry, &tool_call.name, args),
        Err(error) => {
            return ToolOutcome::Result(Err(invalid_arguments(
                registry,
                &tool_call.name,
                error.to_string(),
            )));
        }
    };
    if let Err(error) = registry.validate_args_for_tool(&tool_call.name, &args) {
        return ToolOutcome::Result(Err(invalid_arguments(registry, &tool_call.name, error)));
    }
    execute_tool_outcome(registry, &tool_call.name, args, context).await
}

/// Some models send nested objects as JSON-encoded strings (`"conversation": "{...}"`).
/// Decode those back when the parameter's schema does not accept a string.
fn unstringify_nested_args(
    registry: &ToolRegistry,
    tool_name: &str,
    mut args: serde_json::Value,
) -> serde_json::Value {
    let Some(tool) = registry
        .all()
        .into_iter()
        .find(|tool| tool.name() == tool_name)
    else {
        return args;
    };
    let parameters = tool.tool_definition().parameters;
    let Some(object) = args.as_object_mut() else {
        return args;
    };
    for (name, value) in object.iter_mut() {
        let Some(text) = value.as_str() else {
            continue;
        };
        let Some(schema) = parameters["properties"].get(name) else {
            continue;
        };
        if schema_accepts_string(schema) {
            continue;
        }
        if let Ok(decoded @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) =
            serde_json::from_str(text)
        {
            *value = decoded;
        }
    }
    args
}

fn schema_accepts_string(schema: &serde_json::Value) -> bool {
    match &schema["type"] {
        serde_json::Value::String(kind) => kind == "string",
        serde_json::Value::Array(kinds) => kinds.iter().any(|kind| kind == "string"),
        // `$ref` / `anyOf` without a top-level type: check the variants we can see.
        _ => ["anyOf", "oneOf"].iter().any(|key| {
            schema[*key]
                .as_array()
                .is_some_and(|variants| variants.iter().any(schema_accepts_string))
        }),
    }
}

fn invalid_arguments(registry: &ToolRegistry, tool_name: &str, error: String) -> ToolError {
    let interface = registry
        .all()
        .into_iter()
        .find(|tool| tool.name() == tool_name)
        .map(|tool| tool.tool_definition().parameters.to_string());
    match interface {
        Some(interface) => ToolError::new(format!(
            "invalid arguments for `{tool_name}`: {error}. Expected parameters: {interface}"
        )),
        None => ToolError::new(format!("unknown tool: {tool_name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::schema_accepts_string;

    #[test]
    fn only_non_string_parameters_are_decoded() {
        assert!(schema_accepts_string(
            &serde_json::json!({"type": "string"})
        ));
        assert!(schema_accepts_string(
            &serde_json::json!({"type": ["string", "null"]})
        ));
        assert!(schema_accepts_string(
            &serde_json::json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
        ));
        assert!(!schema_accepts_string(
            &serde_json::json!({"$ref": "#/$defs/ConversationKey"})
        ));
        assert!(!schema_accepts_string(
            &serde_json::json!({"type": "object"})
        ));
    }
}

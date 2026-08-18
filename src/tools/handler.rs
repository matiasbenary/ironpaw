use std::sync::Arc;

use crate::{
    bus::{Bus, Event},
    message::ToolCall,
    tools::{ExecutionContext, ToolCallResult, ToolError, ToolRegistry, ToolResult, execute_tool},
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
            let results = execute_tool_request(
                registry.as_ref(),
                &context,
                tool_calls,
                allowed_tool_names.as_deref(),
            )
            .await;
            bus.publish(Event::ToolResponse {
                request_id,
                results,
            });
        });
    }
}

async fn execute_tool_request(
    registry: &ToolRegistry,
    context: &ExecutionContext,
    tool_calls: Vec<ToolCall>,
    allowed_tool_names: Option<&[String]>,
) -> Vec<ToolCallResult> {
    let mut results = Vec::with_capacity(tool_calls.len());
    for tool_call in &tool_calls {
        let result = execute_tool_call(registry, context, tool_call, allowed_tool_names).await;
        results.push(ToolCallResult {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            result,
        });
    }
    results
}

async fn execute_tool_call(
    registry: &ToolRegistry,
    context: &ExecutionContext,
    tool_call: &ToolCall,
    allowed_tool_names: Option<&[String]>,
) -> ToolResult {
    if allowed_tool_names.is_some_and(|names| !names.iter().any(|name| name == &tool_call.name)) {
        return Err(ToolError::new("tool is not allowed in this agent run"));
    }
    let args = match serde_json::from_str(&tool_call.arguments) {
        Ok(args) => args,
        Err(error) => {
            return Err(invalid_arguments(
                registry,
                &tool_call.name,
                error.to_string(),
            ));
        }
    };
    if let Err(error) = registry.validate_args_for_tool(&tool_call.name, &args) {
        return Err(invalid_arguments(registry, &tool_call.name, error));
    }
    execute_tool(registry, &tool_call.name, args, context).await
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

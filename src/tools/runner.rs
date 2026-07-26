use std::sync::Arc;

use crate::tools::{
    ExecutionContext, InternalTool, InternalTools, ToolError, ToolRegistry, ToolResponder,
    ToolResult,
};

/// How deep tool-calls-tool chains may go (dynamic tools and agents can
/// call tools, which can call them again).
pub(crate) const MAX_TOOL_DEPTH: usize = 8;
pub(crate) const MAX_TOOL_CALLS: usize = 50;
const TOOL_CALL_WARNING_INTERVAL: usize = 10;

fn reserve_tool_call(ctx: &ExecutionContext) -> Result<(), String> {
    let mut budget = ctx.tool_budget.lock().expect("tool budget lock");
    if budget.used >= MAX_TOOL_CALLS {
        return Err(format!(
            "tool call limit reached ({MAX_TOOL_CALLS}); no more tool calls are available for this turn"
        ));
    }
    budget.used += 1;
    if budget.used.is_multiple_of(TOOL_CALL_WARNING_INTERVAL) {
        let used = budget.used;
        budget
            .pending_warnings
            .push(crate::message::Message::System(format!(
                "[tool budget] {} of {MAX_TOOL_CALLS} tool calls used; {} remain.",
                used,
                MAX_TOOL_CALLS.saturating_sub(used),
            )));
    }
    Ok(())
}

/// Execute a tool by name, folding errors into the result JSON so every
/// caller (agent loop, scripts, automations) sees the same shape.
pub async fn run_tool(
    tools: &InternalTools,
    name: &str,
    args: serde_json::Value,
    ctx: &ExecutionContext,
) -> ToolResult {
    run_registered_tool(&tools.registry, name, args, ctx).await
}

pub async fn run_registered_tool(
    registry: &ToolRegistry,
    name: &str,
    args: serde_json::Value,
    ctx: &ExecutionContext,
) -> ToolResult {
    let tool = registry
        .all()
        .iter()
        .find(|tool| tool.name() == name)
        .cloned();
    let Some(tool) = tool else {
        return Err(ToolError::new(format!("unknown tool: {name}")));
    };
    run_internal_tool(tool, args, ctx).await
}

pub async fn run_internal_tool(
    tool: Arc<dyn InternalTool>,
    args: serde_json::Value,
    ctx: &ExecutionContext,
) -> ToolResult {
    if let Err(error) = reserve_tool_call(ctx) {
        return Err(ToolError::new(error));
    }
    if ctx.depth >= MAX_TOOL_DEPTH {
        return Err(ToolError::new(format!(
            "tool call chain exceeded max depth ({MAX_TOOL_DEPTH})"
        )));
    }
    let (responder, captured) = ToolResponder::capture();
    let mut ctx = ctx.with_tool(tool.name().to_string());
    ctx.depth += 1;
    tool.execute(args, &ctx, responder).await;
    captured
        .lock()
        .expect("tool response lock")
        .take()
        .unwrap_or_else(|| Err(ToolError::new("tool finished without responding")))
}

pub(crate) async fn run_registered_tool_for_context(
    registry: &ToolRegistry,
    ctx: &ExecutionContext,
    tool_name: String,
    args: serde_json::Value,
    responder: ToolResponder,
) {
    let tool = registry
        .all()
        .iter()
        .find(|tool| tool.name() == tool_name)
        .cloned();
    let Some(tool) = tool else {
        responder.respond(Err(ToolError::new(format!("unknown tool: {tool_name}"))));
        return;
    };
    if let Err(error) = reserve_tool_call(ctx) {
        responder.respond(Err(ToolError::new(error)));
    } else if ctx.depth >= MAX_TOOL_DEPTH {
        responder.respond(Err(ToolError::new(format!(
            "tool call chain exceeded max depth ({MAX_TOOL_DEPTH})"
        ))));
    } else {
        let mut tool_ctx = ctx.with_tool(tool.name().to_string());
        tool_ctx.depth += 1;
        tool.execute(args, &tool_ctx, responder).await;
    }
}

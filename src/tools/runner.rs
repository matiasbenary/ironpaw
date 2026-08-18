use std::sync::Arc;

use crate::tools::{
    ExecutionContext, InternalTool, ToolError, ToolOutcome, ToolRegistry, ToolResult,
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

pub async fn execute_tool(
    registry: &ToolRegistry,
    name: &str,
    args: serde_json::Value,
    ctx: &ExecutionContext,
) -> ToolResult {
    execute_tool_outcome(registry, name, args, ctx)
        .await
        .into_result()
}

pub(crate) async fn execute_tool_outcome(
    registry: &ToolRegistry,
    name: &str,
    args: serde_json::Value,
    ctx: &ExecutionContext,
) -> ToolOutcome<ToolResult> {
    let tool = registry
        .all()
        .iter()
        .find(|tool| tool.name() == name)
        .cloned();
    let Some(tool) = tool else {
        return ToolOutcome::Result(Err(ToolError::new(format!("unknown tool: {name}"))));
    };
    execute_resolved_tool_outcome(tool, args, ctx).await
}

#[cfg(test)]
pub(crate) async fn execute_resolved_tool(
    tool: Arc<dyn InternalTool>,
    args: serde_json::Value,
    ctx: &ExecutionContext,
) -> ToolResult {
    execute_resolved_tool_outcome(tool, args, ctx)
        .await
        .into_result()
}

async fn execute_resolved_tool_outcome(
    tool: Arc<dyn InternalTool>,
    args: serde_json::Value,
    ctx: &ExecutionContext,
) -> ToolOutcome<ToolResult> {
    if let Err(error) = reserve_tool_call(ctx) {
        return ToolOutcome::Result(Err(ToolError::new(error)));
    }
    if ctx.depth >= MAX_TOOL_DEPTH {
        return ToolOutcome::Result(Err(ToolError::new(format!(
            "tool call chain exceeded max depth ({MAX_TOOL_DEPTH})"
        ))));
    }
    let mut ctx = ctx.with_tool(tool.name().to_string());
    ctx.depth += 1;
    tool.outcome(args, &ctx).await
}

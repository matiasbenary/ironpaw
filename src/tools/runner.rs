use std::sync::Arc;

use crate::{
    bus::Bus,
    history::History,
    tools::{ExecutionContext, InternalTool, InternalTools, ToolRegistry, ToolResponder},
};

/// How deep tool-calls-tool chains may go (dynamic tools and agents can
/// call tools, which can call them again).
pub(crate) const MAX_TOOL_DEPTH: usize = 8;

/// Execute a tool by name, folding errors into the result JSON so every
/// caller (agent loop, scripts, automations) sees the same shape.
pub async fn run_tool(
    tools: &InternalTools,
    name: &str,
    args: serde_json::Value,
    ctx: &ExecutionContext,
) -> serde_json::Value {
    run_registered_tool(&tools.registry, name, args, ctx).await
}

pub async fn run_registered_tool(
    registry: &ToolRegistry,
    name: &str,
    args: serde_json::Value,
    ctx: &ExecutionContext,
) -> serde_json::Value {
    let tool = registry
        .all()
        .iter()
        .find(|tool| tool.name() == name)
        .cloned();
    let Some(tool) = tool else {
        return serde_json::json!({ "ok": false, "error": format!("unknown tool: {name}") });
    };
    run_internal_tool(tool, args, ctx).await
}

pub async fn run_internal_tool(
    tool: Arc<dyn InternalTool>,
    args: serde_json::Value,
    ctx: &ExecutionContext,
) -> serde_json::Value {
    if ctx.depth >= MAX_TOOL_DEPTH {
        return serde_json::json!({
            "ok": false,
            "error": format!("tool call chain exceeded max depth ({MAX_TOOL_DEPTH})"),
        });
    }
    let (responder, captured) = ToolResponder::capture();
    let ctx = ExecutionContext {
        invoked_by: ctx.invoked_by.clone(),
        depth: ctx.depth + 1,
    };
    match tool.execute(args, &ctx, responder).await {
        Ok(()) => captured
            .lock()
            .expect("tool response lock")
            .take()
            .unwrap_or_else(
                || serde_json::json!({ "ok": false, "error": "tool finished without responding" }),
            ),
        Err(err) => serde_json::json!({ "ok": false, "error": err }),
    }
}

pub(crate) async fn run_registered_tool_for_context(
    bus: Bus,
    history: std::sync::Arc<dyn History>,
    registry: &ToolRegistry,
    ctx: &ExecutionContext,
    tool_call_id: String,
    tool_name: String,
    args: serde_json::Value,
) {
    let tool = registry
        .all()
        .iter()
        .find(|tool| tool.name() == tool_name)
        .cloned();
    let responder = ToolResponder::bus(bus, history, ctx.clone(), tool_call_id, tool_name.clone());
    let Some(tool) = tool else {
        let _ = responder.respond(serde_json::json!({
            "ok": false,
            "error": format!("unknown tool: {tool_name}"),
        }));
        return;
    };
    let result = if ctx.depth >= MAX_TOOL_DEPTH {
        Err(format!(
            "tool call chain exceeded max depth ({MAX_TOOL_DEPTH})"
        ))
    } else {
        tool.execute(
            args,
            &ExecutionContext {
                invoked_by: ctx.invoked_by.clone(),
                depth: ctx.depth + 1,
            },
            responder.clone(),
        )
        .await
    };
    if let Err(err) = result
        && !responder.has_responded()
    {
        let _ = responder.respond(serde_json::json!({ "ok": false, "error": err }));
    }
}

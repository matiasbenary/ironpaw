use std::{collections::HashSet, sync::Arc};

use crate::{
    bus::{Bus, ChannelAction, ConversationKey, Event},
    history::{HistoryContext, HistoryEntry, HistoryScope, MessageMetadata},
    memory::Memory,
    message::{Message, ToolCall, ToolDefinition},
    models,
    tools::{self, ExecutionContext, InternalTool, InvocationScope, ToolRegistry},
};

const DEFAULT_TASK_TOOLS: [&str; 4] = ["list_files", "read_file", "create_file", "update_file"];

use super::{
    preamble::system_preamble,
    utils::{publish_channel_action, publish_progress},
};
use crate::tools::raw_inference::{RawInferenceRequest, RawInferenceTool};

/// The conversation a run replies to.
pub type Destination<'a> = Option<&'a ConversationKey>;

/// The bounded tool loop: history + raw_inference + tool execution.
pub struct Agent {
    bus: Bus,
    tools: Arc<ToolRegistry>,
    history_context: Arc<HistoryContext>,
    raw_inference: RawInferenceTool,
    memory: Arc<Memory>,
}

impl Agent {
    pub fn new(
        bus: Bus,
        tools: Arc<ToolRegistry>,
        history_context: Arc<HistoryContext>,
        raw_inference: RawInferenceTool,
        memory: Arc<Memory>,
    ) -> Self {
        Self {
            bus,
            tools,
            history_context,
            raw_inference,
            memory,
        }
    }

    pub async fn handle_conversation(
        &self,
        destination: Destination<'_>,
        context: ExecutionContext,
    ) -> Result<Option<String>, String> {
        let Some(conversation) = destination else {
            return Err("conversation is required".to_string());
        };
        self.run(AgentRun {
            destination,
            history_scope: HistoryScope::Conversation(conversation.clone()),
            context,
            transient_messages: Vec::new(),
            tools: self.tools.all(),
            allowed_tool_names: None,
            recalled_memories: Vec::new(),
        })
        .await
    }

    pub async fn run_task(
        &self,
        automation_id: String,
        prompt: String,
        mut tool_names: Vec<String>,
    ) -> Result<Option<String>, String> {
        for name in DEFAULT_TASK_TOOLS {
            if !tool_names.iter().any(|tool_name| tool_name == name) {
                tool_names.push(name.to_string());
            }
        }
        let tools = self.tools.named_tools(&tool_names)?;
        let history_scope = HistoryScope::Automation(automation_id.clone());
        self.history_context.append(
            &history_scope,
            &[HistoryEntry {
                message: Message::User {
                    text: prompt,
                    attachments: Vec::new(),
                },
                metadata: MessageMetadata::for_automation(automation_id.clone()),
            }],
        )?;
        self.run(AgentRun {
            destination: None,
            history_scope,
            context: ExecutionContext::new(InvocationScope::ScheduledAutomation { automation_id }),
            transient_messages: Vec::new(),
            tools,
            allowed_tool_names: Some(tool_names),
            recalled_memories: Vec::new(),
        })
        .await
    }

    /// Shared bounded tool loop. `transient_messages` are included in the
    /// model input for this run but are not persisted by the agent as user
    /// history.
    async fn run(&self, mut run: AgentRun<'_>) -> Result<Option<String>, String> {
        let ctx = run.context.clone();
        if let Some(query) = self.latest_user_text(&run.history_scope)? {
            match self.memory.recall(&query).await {
                Ok(memories) => run.recalled_memories = memories,
                Err(error) => debug_log(format!("memory recall failed: {error}")),
            }
        }
        let mut round = 0;
        loop {
            round += 1;
            run.transient_messages
                .extend(ctx.drain_tool_budget_messages());
            if let Some(conversation) = run.destination {
                publish_channel_action(&self.bus, conversation, ChannelAction::Typing);
            }
            let available_tools = tool_definitions(&run.tools);
            let model_messages = self.model_messages(&run)?;
            debug_log(format!(
                "agent: round {} calling chat_completions(messages={}, tools={})",
                round,
                model_messages.len(),
                available_tools.len()
            ));
            let request = RawInferenceRequest {
                model: models::text_model(),
                messages: model_messages,
                tools: available_tools,
                parallel_tool_calls: false,
            };

            let output = self.raw_inference.execute(request).await?;
            self.history_context
                .compact_if_needed(&run.history_scope, output.usage.prompt_tokens)
                .await?;
            let output = output.message;
            let Message::Assistant {
                text, tool_calls, ..
            } = output.clone()
            else {
                return Err("model did not return an assistant message".to_string());
            };
            run.transient_messages.clear();
            if tool_calls.is_empty() {
                debug_log(format!(
                    "agent: round {} finished with assistant text",
                    round
                ));
                if text.as_deref().is_some_and(|text| !text.is_empty()) {
                    let scope = &run.history_scope;
                    self.history_context
                        .append(scope, &[history_entry(output.clone(), scope)])?;
                    self.ingest_memory(scope);
                }
                return Ok(text.filter(|text| !text.is_empty()));
            }

            debug_log(format!(
                "agent: round {} received {} tool call(s)",
                round,
                tool_calls.len()
            ));
            let mut tool_results = Vec::new();
            let mut tool_requests = Vec::new();
            for tool_call in tool_calls {
                self.publish_tool_progress(run.destination, &tool_call.name);
                match prepare_tool_request(tool_call, run.allowed_tool_names.as_deref()) {
                    PreparedToolCall::Request { tool_call, args } => {
                        tool_requests.push((tool_call, args));
                    }
                    PreparedToolCall::Result(message) => tool_results.push(message),
                }
            }
            if !tool_results.is_empty() {
                run.transient_messages
                    .push(assistant_message_for_history(output));
                run.transient_messages.extend(tool_results);
            } else {
                let mut events = self.bus.subscribe();
                let pending = tool_requests
                    .iter()
                    .map(|(tool_call, _)| tool_call.id.clone())
                    .collect::<HashSet<_>>();
                for (tool_call, args) in tool_requests {
                    self.bus.publish(Event::ToolRequest {
                        context: ctx.clone(),
                        tool_call,
                        args,
                    });
                }
                wait_for_tool_results(&mut events, pending).await?;
            }
        }
    }

    fn model_messages(&self, run: &AgentRun<'_>) -> Result<Vec<Message>, String> {
        let mut messages = system_preamble(run.destination);
        if !run.recalled_memories.is_empty() {
            let recalled = run
                .recalled_memories
                .iter()
                .map(|memory| format!("- {memory}"))
                .collect::<Vec<_>>()
                .join("\n");
            messages.push(Message::System(format!(
                "[recalled user memory]\n{recalled}\nUse these memories only when relevant."
            )));
        }
        for entry in self.history_context.load(&run.history_scope)? {
            let mut message = entry.message;
            if let Some(external_id) = entry.metadata.external_id {
                match &mut message {
                    Message::User { text, .. } => {
                        *text = format!("[message_id:{external_id}] {text}")
                    }
                    Message::Assistant { text, .. } => {
                        *text = Some(format!(
                            "[message_id:{external_id}] {}",
                            text.clone().unwrap_or_default()
                        ))
                    }
                    _ => {}
                }
            }
            messages.push(message);
        }
        messages.extend(run.transient_messages.clone());
        Ok(messages)
    }

    fn latest_user_text(&self, scope: &HistoryScope) -> Result<Option<String>, String> {
        Ok(self
            .history_context
            .load(scope)?
            .into_iter()
            .rev()
            .find_map(|entry| match entry.message {
                Message::User { text, .. } => Some(text),
                _ => None,
            }))
    }

    fn ingest_memory(&self, scope: &HistoryScope) {
        let Ok(entries) = self.history_context.load(scope) else {
            return;
        };
        let Some(user_index) = entries
            .iter()
            .rposition(|entry| matches!(entry.message, Message::User { .. }))
        else {
            return;
        };
        let previous = entries[user_index.saturating_sub(10)..user_index]
            .iter()
            .map(|entry| entry.message.clone())
            .collect::<Vec<_>>();
        let current = entries[user_index..]
            .iter()
            .map(|entry| entry.message.clone())
            .collect::<Vec<_>>();
        let memory = Arc::clone(&self.memory);
        tokio::spawn(async move {
            if let Err(error) = memory.ingest(&previous, &current).await {
                debug_log(format!("memory ingestion failed: {error}"));
            }
        });
    }

    fn publish_tool_progress(&self, destination: Destination<'_>, tool_name: &str) {
        let progress_message = format!("calling {tool_name}");
        debug_log(format!("agent: {progress_message}"));
        if let Some(conversation) = destination {
            publish_progress(&self.bus, conversation, progress_message);
        }
    }
}

async fn wait_for_tool_results(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    mut pending: HashSet<String>,
) -> Result<(), String> {
    while !pending.is_empty() {
        match events.recv().await {
            Ok(Event::ToolResult { tool_call_id, .. }) => {
                pending.remove(&tool_call_id);
            }
            Ok(_) => {}
            Err(error) => return Err(format!("failed waiting for tool result: {error}")),
        }
    }
    Ok(())
}

struct AgentRun<'a> {
    destination: Destination<'a>,
    history_scope: HistoryScope,
    context: ExecutionContext,
    transient_messages: Vec<Message>,
    tools: Vec<Arc<dyn InternalTool>>,
    allowed_tool_names: Option<Vec<String>>,
    recalled_memories: Vec<String>,
}

fn tool_definitions(tools: &[Arc<dyn InternalTool>]) -> Vec<ToolDefinition> {
    tools
        .iter()
        .map(|tool| tools::definition_for_model(tool.as_ref()))
        .collect()
}

fn history_entry(message: Message, scope: &HistoryScope) -> HistoryEntry {
    HistoryEntry {
        message,
        metadata: history_metadata(scope),
    }
}

fn history_metadata(scope: &HistoryScope) -> MessageMetadata {
    match scope {
        HistoryScope::Conversation(conversation) => {
            MessageMetadata::new(conversation.clone(), None)
        }
        HistoryScope::Automation(automation_id) => {
            MessageMetadata::for_automation(automation_id.clone())
        }
    }
}

enum PreparedToolCall {
    Request {
        tool_call: ToolCall,
        args: serde_json::Value,
    },
    Result(Message),
}

fn prepare_tool_request(
    tool_call: ToolCall,
    allowed_tool_names: Option<&[String]>,
) -> PreparedToolCall {
    let tool_name = tool_call.name.clone();
    if allowed_tool_names.is_some_and(|names| !names.iter().any(|name| name == &tool_name)) {
        return PreparedToolCall::Result(Message::ToolResult {
            tool_call_id: tool_call.id,
            tool_name,
            result: error_result("tool is not allowed in this agent run"),
        });
    }
    let args: serde_json::Value = match serde_json::from_str(&tool_call.arguments) {
        Ok(args) => args,
        Err(err) => {
            return PreparedToolCall::Result(Message::ToolResult {
                tool_call_id: tool_call.id,
                tool_name,
                result: error_result(err.to_string()),
            });
        }
    };
    PreparedToolCall::Request { tool_call, args }
}

fn error_result(error: impl Into<String>) -> serde_json::Value {
    serde_json::json!({ "ok": false, "error": error.into() })
}

fn assistant_message_for_history(message: Message) -> Message {
    let Message::Assistant {
        text,
        attachments,
        tool_calls,
    } = message
    else {
        return message;
    };
    Message::Assistant {
        text,
        attachments,
        tool_calls: tool_calls
            .into_iter()
            .map(|mut tool_call| {
                if serde_json::from_str::<serde_json::Value>(&tool_call.arguments).is_err() {
                    tool_call.arguments = "{}".to_string();
                }
                tool_call
            })
            .collect(),
    }
}

fn debug_log(message: impl AsRef<str>) {
    eprintln!("{}", message.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_tool_arguments_are_sanitized_for_history() {
        let message = Message::Assistant {
            text: None,
            attachments: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_bad".to_string(),
                name: "send_message".to_string(),
                arguments: "{\"channel\":\"telegram\"".to_string(),
            }],
        };

        let Message::Assistant { tool_calls, .. } = assistant_message_for_history(message) else {
            panic!("expected assistant message");
        };
        assert_eq!(tool_calls[0].arguments, "{}");
    }
}

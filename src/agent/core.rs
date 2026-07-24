use std::{collections::HashSet, sync::Arc};

use crate::{
    bus::{Bus, ChannelAction, ConversationKey, Event},
    history::{History, HistoryEntry, HistoryScope, MessageMetadata},
    memory::Memory,
    message::{Message, ToolCall, ToolDefinition},
    models,
    tools::{
        self, ExecutionContext, InternalTool, InvocationScope, ToolRegistry, tool_result_json,
    },
};

const DEFAULT_TASK_TOOLS: [&str; 4] = ["list_files", "read_file", "create_file", "update_file"];
const COMPACT_AT_PROMPT_TOKENS: u64 = 256_000;

use super::{
    logger,
    preamble::system_preamble,
    utils::{publish_channel_action, publish_progress},
};
use crate::tools::raw_inference::{ModelInference, RawInferenceRequest};

/// The conversation a run replies to.
pub type Destination<'a> = Option<&'a ConversationKey>;

/// The bounded tool loop: history + raw_inference + tool execution.
pub struct Agent {
    bus: Bus,
    tools: Arc<ToolRegistry>,
    history: Arc<dyn History>,
    inference: ModelInference,
    memory: Arc<Memory>,
}

impl Agent {
    pub fn new(
        bus: Bus,
        tools: Arc<ToolRegistry>,
        history: Arc<dyn History>,
        inference: ModelInference,
        memory: Arc<Memory>,
    ) -> Self {
        Self {
            bus,
            tools,
            history,
            inference,
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
        self.history.append_scope(
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
            };

            logger::raw_inference_request_logged(&request);
            let inference = self.inference.execute(request).await?;
            logger::raw_inference_response_logged(&inference);
            if inference.usage.prompt_tokens >= COMPACT_AT_PROMPT_TOKENS {
                if let Some(conversation) = run.destination {
                    publish_progress(&self.bus, conversation, "compacting history...".to_string());
                }
                if let Err(error) = self.compact_history(&run.history_scope).await {
                    debug_log(format!("history compaction failed: {error}"));
                }
            }
            let output = inference.message.clone();
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
                    self.history
                        .append_scope(scope, &[history_entry(output.clone(), scope)])?;
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
                logger::raw_tool_call_logged(&tool_call);
                match prepare_tool_request(tool_call, run.allowed_tool_names.as_deref()) {
                    PreparedToolCall::Request { tool_call, args } => {
                        tool_requests.push((tool_call, args));
                    }
                    PreparedToolCall::Result(message) => tool_results.push(message),
                }
            }
            if !tool_results.is_empty() {
                logger::raw_tool_results_logged(&tool_results);
                run.transient_messages
                    .push(assistant_message_for_tool_results(output, &tool_results));
                run.transient_messages.extend(tool_results);
            }
            if !tool_requests.is_empty() {
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
                        record_in_history: true,
                    });
                }
                let tool_results = wait_for_tool_results(&mut events, pending).await?;
                logger::raw_tool_results_logged(&tool_results);
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
        for entry in self.history.load_scope(&run.history_scope)? {
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
        messages.push(Message::System(format!(
            "[runtime context] The current local time is {}.",
            chrono::Local::now().format("%A %Y-%m-%dT%H:%M:%S%:z")
        )));
        messages.extend(run.transient_messages.clone());
        Ok(messages)
    }

    fn latest_user_text(&self, scope: &HistoryScope) -> Result<Option<String>, String> {
        Ok(self
            .history
            .load_scope(scope)?
            .into_iter()
            .rev()
            .find_map(|entry| match entry.message {
                Message::User { text, .. } => Some(text),
                _ => None,
            }))
    }

    fn ingest_memory(&self, scope: &HistoryScope) {
        let Ok(entries) = self.history.load_scope(scope) else {
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

    async fn compact_history(&self, scope: &HistoryScope) -> Result<(), String> {
        let history = self.history.load_scope(scope)?;
        if history.is_empty() {
            return Ok(());
        }
        let retained = retained_entries(&history);
        let retained_ids = retained
            .iter()
            .map(|entry| entry.metadata.id.as_str())
            .collect::<HashSet<_>>();
        let messages = history
            .iter()
            .filter(|entry| !retained_ids.contains(entry.metadata.id.as_str()))
            .map(|entry| entry.message.clone())
            .collect::<Vec<_>>();
        if messages.is_empty() {
            return Ok(());
        }
        let transcript = serde_json::to_string(&messages).map_err(|err| err.to_string())?;
        let response = self
            .inference
            .execute(RawInferenceRequest {
                model: models::text_model(),
                messages: vec![
                    Message::System(
                        "Summarize older history for future runs. This summary is not an action ledger. Do not claim an action, delivery, user acknowledgement, recommendation, date, or research finding unless the transcript explicitly proves it. Omit uncertain information. Preserve only user preferences, durable source ids, verified tool evidence, and unresolved work."
                            .to_string(),
                    ),
                    Message::User {
                        text: transcript,
                        attachments: Vec::new(),
                    },
                ],
                tools: Vec::new(),
            })
            .await?;
        let Message::Assistant { text, .. } = response.message else {
            return Err(
                "history compaction model response was not an assistant message".to_string(),
            );
        };
        let summary = text.unwrap_or_default();
        if summary.trim().is_empty() {
            return Err("history compaction response was empty".to_string());
        }
        self.history.compact_scope(scope, &summary, &retained)
    }
}

fn retained_entries(history: &[HistoryEntry]) -> Vec<HistoryEntry> {
    let mut retained = history.iter().rev().take(4).cloned().collect::<Vec<_>>();
    if let Some(user) = history
        .iter()
        .rev()
        .find(|entry| matches!(entry.message, Message::User { .. }))
        && !retained
            .iter()
            .any(|entry| entry.metadata.id == user.metadata.id)
    {
        retained.push(user.clone());
    }
    retained.reverse();
    retained
}

async fn wait_for_tool_results(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    mut pending: HashSet<String>,
) -> Result<Vec<Message>, String> {
    let mut results = Vec::new();
    while !pending.is_empty() {
        match events.recv().await {
            Ok(Event::ToolResult {
                tool_call_id,
                tool_name,
                result,
                ..
            }) => {
                if pending.remove(&tool_call_id) {
                    results.push(Message::ToolResult {
                        tool_call_id,
                        tool_name,
                        result: tool_result_json(&result),
                    });
                }
            }
            Ok(_) => {}
            Err(error) => return Err(format!("failed waiting for tool result: {error}")),
        }
    }
    Ok(results)
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
    tool_result_json(&Err(tools::ToolError::new(error.into())))
}

fn assistant_message_for_tool_results(message: Message, results: &[Message]) -> Message {
    let Message::Assistant {
        text,
        attachments,
        tool_calls,
    } = message
    else {
        return message;
    };
    let result_ids = results
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    Message::Assistant {
        text,
        attachments,
        tool_calls: tool_calls
            .into_iter()
            .filter(|tool_call| result_ids.contains(tool_call.id.as_str()))
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
    fn immediate_tool_results_keep_original_arguments_and_matching_calls() {
        let message = Message::Assistant {
            text: None,
            attachments: Vec::new(),
            tool_calls: vec![
                ToolCall {
                    id: "call_good".to_string(),
                    name: "list_tools".to_string(),
                    arguments: "{}".to_string(),
                },
                ToolCall {
                    id: "call_bad".to_string(),
                    name: "list_channels".to_string(),
                    arguments: "{}\"\"".to_string(),
                },
            ],
        };
        let results = vec![Message::ToolResult {
            tool_call_id: "call_bad".to_string(),
            tool_name: "list_channels".to_string(),
            result: error_result("invalid arguments"),
        }];

        let Message::Assistant { tool_calls, .. } =
            assistant_message_for_tool_results(message, &results)
        else {
            panic!("expected assistant message");
        };
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_bad");
        assert_eq!(tool_calls[0].arguments, "{}\"\"");
    }
}

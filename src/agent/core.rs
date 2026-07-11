use std::sync::Arc;

use crate::{
    bus::{Bus, ChannelAction, ConversationKey, Event},
    history::{History, HistoryEntry, HistoryScope, MessageMetadata},
    memory::Memory,
    message::{Message, ToolCall, ToolDefinition},
    models,
    tools::{self, ExecutionContext, InternalTool, InvocationScope, ToolHandler, ToolRegistry},
};

use super::{
    preamble::system_preamble,
    utils::{publish_channel_action, publish_progress},
};
use crate::tools::raw_inference::{RawInferenceRequest, RawInferenceTool};

const MAX_TOOL_ROUNDS: usize = 8;
const COMPACT_AT_PROMPT_TOKENS: u64 = 256_000;

/// The conversation a run replies to.
pub type Destination<'a> = Option<&'a ConversationKey>;

/// The bounded tool loop: history + raw_inference + tool execution.
pub struct Agent {
    bus: Bus,
    tools: Arc<ToolRegistry>,
    tool_handler: Arc<ToolHandler>,
    history: Arc<dyn History>,
    raw_inference: RawInferenceTool,
    memory: Arc<Memory>,
}

impl Agent {
    pub fn new(
        bus: Bus,
        tools: Arc<ToolRegistry>,
        tool_handler: Arc<ToolHandler>,
        history: Arc<dyn History>,
        raw_inference: RawInferenceTool,
        memory: Arc<Memory>,
    ) -> Self {
        Self {
            bus,
            tools,
            tool_handler,
            history,
            raw_inference,
            memory,
        }
    }

    pub async fn handle_conversation(
        &self,
        destination: Destination<'_>,
    ) -> Result<Option<String>, String> {
        let Some(conversation) = destination else {
            return Err("conversation is required".to_string());
        };
        self.run(AgentRun {
            destination,
            history_scope: Some(HistoryScope::Conversation(conversation.clone())),
            invocation_scope: InvocationScope::Conversation(conversation.clone()),
            transient_messages: Vec::new(),
            tools: self.tools.all(),
            allowed_tool_names: None,
            persist_history: true,
            recalled_memories: Vec::new(),
        })
        .await
    }

    pub async fn run_task(
        &self,
        automation_id: String,
        prompt: String,
        tool_names: Vec<String>,
    ) -> Result<Option<String>, String> {
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
            history_scope: Some(history_scope),
            invocation_scope: InvocationScope::ScheduledAutomation { automation_id },
            transient_messages: Vec::new(),
            tools,
            allowed_tool_names: Some(tool_names),
            persist_history: true,
            recalled_memories: Vec::new(),
        })
        .await
    }

    /// Shared bounded tool loop. `transient_messages` are included in the
    /// model input for this run but are not persisted by the agent as user
    /// history.
    async fn run(&self, mut run: AgentRun<'_>) -> Result<Option<String>, String> {
        let ctx = ExecutionContext {
            invoked_by: run.invocation_scope.clone(),
            depth: 0,
        };
        if let Some(query) = self.latest_user_text(run.history_scope.as_ref())? {
            match self.memory.recall(&query).await {
                Ok(memories) => run.recalled_memories = memories,
                Err(error) => debug_log(format!("memory recall failed: {error}")),
            }
        }
        let mut compact_before_next_round = false;

        for round in 0..MAX_TOOL_ROUNDS {
            if compact_before_next_round {
                self.compact_history(run.history_scope.as_ref()).await?;
            }
            if let Some(conversation) = run.destination {
                publish_channel_action(&self.bus, conversation, ChannelAction::Typing);
            }
            let available_tools = tool_definitions(&run.tools);
            let model_messages = self.model_messages(&run)?;
            debug_log(format!(
                "agent: round {} calling chat_completions(messages={}, tools={})",
                round + 1,
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
            compact_before_next_round =
                run.persist_history && output.usage.prompt_tokens >= COMPACT_AT_PROMPT_TOKENS;
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
                    round + 1
                ));
                if run.persist_history && text.as_deref().is_some_and(|text| !text.is_empty()) {
                    let scope = run
                        .history_scope
                        .as_ref()
                        .expect("persistent run has scope");
                    self.history
                        .append_scope(scope, &[history_entry(output.clone(), Some(scope))])?;
                    self.ingest_memory(scope);
                }
                return Ok(text.filter(|text| !text.is_empty()));
            }

            debug_log(format!(
                "agent: round {} received {} tool call(s)",
                round + 1,
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
            } else if tool_requests.len() == 1 && run.persist_history && run.destination.is_some() {
                let (tool_call, args) = tool_requests.pop().expect("single tool request exists");
                self.bus.publish(Event::ToolRequest {
                    context: ctx.clone(),
                    tool_call,
                    args,
                });
                return Ok(None);
            } else if run.persist_history {
                for (tool_call, args) in tool_requests {
                    self.tool_handler
                        .execute_tool_call(tool_call, args, &ctx)
                        .await?;
                }
            } else {
                run.transient_messages.push(output);
                for (tool_call, args) in tool_requests {
                    let tool_result =
                        execute_prepared_tool_call(&self.tools, tool_call, args, &ctx).await;
                    run.transient_messages.push(tool_result.message);
                }
            }
        }

        let stopped = "stopped after too many tool calls".to_string();
        if run.persist_history {
            let scope = run
                .history_scope
                .as_ref()
                .expect("persistent run has scope");
            self.history.append_scope(
                scope,
                &[history_entry(
                    Message::Assistant {
                        text: Some(format!("[tool use] {stopped}")),
                        attachments: Vec::new(),
                        tool_calls: Vec::new(),
                    },
                    Some(scope),
                )],
            )?;
        }
        Ok(Some(stopped))
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
        if run.persist_history {
            for entry in self.history.load_scope(
                run.history_scope
                    .as_ref()
                    .expect("persistent run has scope"),
            )? {
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
        }
        messages.extend(run.transient_messages.clone());
        Ok(messages)
    }

    fn latest_user_text(&self, scope: Option<&HistoryScope>) -> Result<Option<String>, String> {
        let Some(scope) = scope else {
            return Ok(None);
        };
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

    async fn compact_history(&self, scope: Option<&HistoryScope>) -> Result<(), String> {
        let Some(scope) = scope else {
            return Ok(());
        };
        let history = self.history.load_scope(scope)?;
        if history.is_empty() {
            return Ok(());
        }
        let retained = retained_entries(&history);
        let retained_ids = retained
            .iter()
            .map(|entry| entry.metadata.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let messages = history
            .iter()
            .filter(|entry| !retained_ids.contains(entry.metadata.id.as_str()))
            .map(|entry| entry.message.clone())
            .collect::<Vec<_>>();
        if messages.is_empty() {
            return Ok(());
        }
        let summary = self.summarize_history(messages).await?;
        self.history.compact_scope(scope, &summary, &retained)?;
        Ok(())
    }

    async fn summarize_history(&self, messages: Vec<Message>) -> Result<String, String> {
        let transcript = serde_json::to_string(&messages).map_err(|err| err.to_string())?;
        let response = self
            .raw_inference
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
                parallel_tool_calls: false,
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
        Ok(summary)
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

struct AgentRun<'a> {
    destination: Destination<'a>,
    history_scope: Option<HistoryScope>,
    invocation_scope: InvocationScope,
    transient_messages: Vec<Message>,
    tools: Vec<Arc<dyn InternalTool>>,
    allowed_tool_names: Option<Vec<String>>,
    persist_history: bool,
    recalled_memories: Vec<String>,
}

fn tool_definitions(tools: &[Arc<dyn InternalTool>]) -> Vec<ToolDefinition> {
    tools
        .iter()
        .map(|tool| tools::definition_for_model(tool.as_ref()))
        .collect()
}

fn history_entry(message: Message, scope: Option<&HistoryScope>) -> HistoryEntry {
    HistoryEntry {
        message,
        metadata: history_metadata(scope),
    }
}

fn history_metadata(scope: Option<&HistoryScope>) -> MessageMetadata {
    match scope.expect("persistent history entry has scope") {
        HistoryScope::Conversation(conversation) => {
            MessageMetadata::new(conversation.clone(), None)
        }
        HistoryScope::Automation(automation_id) => {
            MessageMetadata::for_automation(automation_id.clone())
        }
    }
}

#[cfg(test)]
async fn execute_tool_call(
    tools: &ToolRegistry,
    tool_call: ToolCall,
    ctx: &ExecutionContext,
    allowed_tool_names: Option<&[String]>,
) -> ExecutedToolCall {
    let tool_name = tool_call.name;
    if allowed_tool_names.is_some_and(|names| !names.iter().any(|name| name == &tool_name)) {
        return ExecutedToolCall::finished(Message::ToolResult {
            tool_call_id: tool_call.id,
            tool_name,
            result: error_result("tool is not allowed in this agent run"),
        });
    }
    let args: serde_json::Value = match serde_json::from_str(&tool_call.arguments) {
        Ok(args) => args,
        Err(err) => {
            return ExecutedToolCall::finished(Message::ToolResult {
                tool_call_id: tool_call.id,
                tool_name,
                result: error_result(err.to_string()),
            });
        }
    };
    execute_prepared_tool_call(
        tools,
        ToolCall {
            id: tool_call.id,
            name: tool_name,
            arguments: "{}".to_string(),
        },
        args,
        ctx,
    )
    .await
}

async fn execute_prepared_tool_call(
    tools: &ToolRegistry,
    tool_call: ToolCall,
    args: serde_json::Value,
    ctx: &ExecutionContext,
) -> ExecutedToolCall {
    let tool_name = tool_call.name;
    let result = tools::run_registered_tool(tools, &tool_name, args, ctx).await;
    debug_log(format!("agent: tool {tool_name} finished"));
    ExecutedToolCall {
        message: Message::ToolResult {
            tool_call_id: tool_call.id,
            tool_name,
            result,
        },
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

struct ExecutedToolCall {
    message: Message,
}

#[cfg(test)]
impl ExecutedToolCall {
    fn finished(message: Message) -> Self {
        Self { message }
    }
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
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;
    use crate::bus::Event;
    use crate::tools::InternalTools;

    fn test_tools(bus: &Bus) -> InternalTools {
        let layout = crate::config::RuntimeLayout::from_root(std::env::temp_dir().join(format!(
            "warden-agent-test-runtime-{}",
            uuid::Uuid::new_v4()
        )));
        let files_dir = layout.files_dir();
        std::fs::create_dir_all(&files_dir).unwrap();
        let history = std::env::temp_dir().join(format!(
            "warden-agent-test-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        let secrets = crate::secrets::Secrets::for_tests();
        InternalTools::new(
            bus.clone(),
            layout,
            crate::tools::raw_inference::RawInferenceTool::new(secrets.tools.clone()).unwrap(),
            crate::tools::messages::SendMessageTool::new(bus.clone(), |_| {
                Err("unexpected attachment lookup".to_string())
            }),
            crate::tools::messages::DeleteMessageTool::new(bus.clone()),
            files_dir,
            Arc::new(crate::history::SqliteHistory::new(history).unwrap()),
            secrets,
        )
    }

    #[tokio::test]
    async fn tool_call_sends_message() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let tools = test_tools(&bus);
        let tool_call = ToolCall {
            id: "call_1".to_string(),
            name: "send_message".to_string(),
            arguments: serde_json::json!({
                "channel": "telegram",
                "chat_id": "chat-1",
                "content": "hi",
            })
            .to_string(),
        };

        let result = execute_tool_call(
            &tools.registry,
            tool_call,
            &ExecutionContext {
                invoked_by: InvocationScope::ScheduledAutomation {
                    automation_id: "test".to_string(),
                },
                depth: 0,
            },
            None,
        )
        .await;
        let result = result.message;
        let Message::ToolResult {
            tool_call_id,
            tool_name,
            result: tool_result,
        } = result
        else {
            panic!("expected tool result");
        };
        assert_eq!(tool_call_id, "call_1");
        assert_eq!(tool_name, "send_message");
        assert_eq!(tool_result["ok"], true);

        let event = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("timed out waiting for bus event")
            .expect("bus closed");

        let Event::MessageOut {
            channel,
            chat_id,
            thread_id,
            message,
            ..
        } = event
        else {
            panic!("expected MessageOut");
        };
        assert_eq!(channel, "telegram");
        assert_eq!(chat_id.as_deref(), Some("chat-1"));
        assert!(thread_id.is_none());
        let Message::Assistant { text, .. } = message else {
            panic!("expected assistant message");
        };
        assert_eq!(text.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn malformed_tool_arguments_return_tool_result_error() {
        let bus = Bus::new(16);
        let tools = test_tools(&bus);
        let tool_call = ToolCall {
            id: "call_bad".to_string(),
            name: "send_message".to_string(),
            arguments: "{\"channel\":\"telegram\"".to_string(),
        };

        let result = execute_tool_call(
            &tools.registry,
            tool_call,
            &ExecutionContext {
                invoked_by: InvocationScope::ScheduledAutomation {
                    automation_id: "test".to_string(),
                },
                depth: 0,
            },
            None,
        )
        .await;
        let result = result.message;
        let Message::ToolResult {
            tool_call_id,
            tool_name,
            result: tool_result,
        } = result
        else {
            panic!("expected tool result");
        };
        assert_eq!(tool_call_id, "call_bad");
        assert_eq!(tool_name, "send_message");
        assert_eq!(tool_result["ok"], false);
        assert!(
            tool_result["error"]
                .as_str()
                .unwrap()
                .contains("EOF while parsing")
        );
    }

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

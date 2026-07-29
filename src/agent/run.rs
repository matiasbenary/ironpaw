use std::{collections::HashSet, sync::Arc};

use crate::{
    bus::{Bus, ChannelAction, ConversationKey, Event},
    history::{History, HistoryEntry, HistoryScope, MessageMetadata},
    inference::{Inference, InferenceError, InferenceRequest},
    memory::Memory,
    message::{Message, ToolCall},
    tools::{self, ExecutionContext, InternalTool, InvocationScope, ToolRegistry},
};

use super::{logger, preamble::system_preamble, utils::publish_channel_action};

const DEFAULT_TASK_TOOLS: [&str; 4] = ["list_files", "read_file", "create_file", "update_file"];
const COMPACT_AT_PROMPT_TOKENS: u64 = 256_000;

/// The conversation a run replies to.
pub type Destination<'a> = Option<&'a ConversationKey>;

/// Entry point for conversation and scheduled-agent runs.
pub struct Agent {
    pub(super) bus: Bus,
    pub(super) tools: Arc<ToolRegistry>,
    pub(super) history: Arc<dyn History>,
    pub(super) inference: Inference,
    pub(super) memory: Arc<Memory>,
}

impl Agent {
    pub fn new(
        bus: Bus,
        tools: Arc<ToolRegistry>,
        history: Arc<dyn History>,
        inference: Inference,
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
}

impl Agent {
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
            tool_selection: ToolSelection::AgentVisible,
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
            tool_selection: ToolSelection::Selected(tools),
            recalled_memories: Vec::new(),
        })
        .await
    }

    async fn run(&self, mut run: AgentRun<'_>) -> Result<Option<String>, String> {
        if let Some(query) = self.latest_user_text(&run.history_scope)? {
            match self.memory.recall(&query).await {
                Ok(memories) => run.recalled_memories = memories,
                Err(error) => debug_log(format!("memory recall failed: {error}")),
            }
        }
        let ctx = run.context.clone();
        let mut round = 0;
        loop {
            round += 1;
            run.transient_messages
                .extend(ctx.drain_tool_budget_messages());
            if let Some(conversation) = run.destination {
                publish_channel_action(&self.bus, conversation, ChannelAction::Typing);
            }

            let active_tools = match &run.tool_selection {
                ToolSelection::AgentVisible => self.tools.agent_tools(),
                ToolSelection::Selected(tools) => tools.clone(),
            };
            let allowed_tool_names = active_tools
                .iter()
                .map(|tool| tool.name().to_string())
                .collect::<Vec<_>>();
            let tools = active_tools
                .iter()
                .map(|tool| tools::definition_for_model(tool.as_ref()))
                .collect::<Vec<_>>();
            let messages = self.model_messages(&run)?;
            debug_log(format!(
                "agent: round {round} calling chat_completions(messages={}, tools={})",
                messages.len(),
                tools.len()
            ));
            let request = InferenceRequest {
                model: self.inference.provider().text_model.clone(),
                messages,
                tools,
            };
            logger::raw_inference_request_logged(&request);
            let inference = match self.inference.execute(request, run.destination).await {
                Ok(inference) => inference,
                Err(InferenceError::ApiKey(_)) => return Ok(None),
                Err(error) => return Err(error.to_string()),
            };
            logger::raw_inference_response_logged(&inference);
            if inference.usage.prompt_tokens >= COMPACT_AT_PROMPT_TOKENS {
                if let Some(conversation) = run.destination {
                    publish_channel_action(
                        &self.bus,
                        conversation,
                        ChannelAction::Progress {
                            text: "compacting history...".to_string(),
                        },
                    );
                }
                if let Err(error) = self.compact_history(&run.history_scope).await {
                    debug_log(format!("history compaction failed: {error}"));
                }
            }

            let output = inference.message;
            let Message::Assistant {
                text, tool_calls, ..
            } = output.clone()
            else {
                return Err("model did not return an assistant message".to_string());
            };
            run.transient_messages.clear();
            if tool_calls.is_empty() {
                if text.as_deref().is_some_and(|text| !text.is_empty()) {
                    if run.destination.is_none() {
                        let metadata = match &run.history_scope {
                            HistoryScope::Conversation(conversation) => {
                                MessageMetadata::new(conversation.clone(), None)
                            }
                            HistoryScope::Automation(automation_id) => {
                                MessageMetadata::for_automation(automation_id.clone())
                            }
                        };
                        self.history.append_scope(
                            &run.history_scope,
                            &[HistoryEntry {
                                message: output.clone(),
                                metadata,
                            }],
                        )?;
                    }
                    self.ingest_memory(
                        &run.history_scope,
                        run.destination.is_some().then_some(output),
                    );
                }
                return Ok(text.filter(|text| !text.is_empty()));
            }
            self.handle_tool_calls(&mut run, output, tool_calls, allowed_tool_names)
                .await?;
        }
    }

    async fn handle_tool_calls(
        &self,
        run: &mut AgentRun<'_>,
        output: Message,
        tool_calls: Vec<ToolCall>,
        allowed_tool_names: Vec<String>,
    ) -> Result<(), String> {
        for tool_call in &tool_calls {
            logger::raw_tool_call_logged(tool_call);
            if let Some(conversation) = run.destination {
                publish_channel_action(
                    &self.bus,
                    conversation,
                    ChannelAction::Progress {
                        text: format!("calling {}", tool_call.name),
                    },
                );
            }
        }

        let mut events = self.bus.subscribe();
        let expected = tool_calls
            .iter()
            .map(|tool_call| tool_call.id.clone())
            .collect::<HashSet<_>>();
        self.bus.publish(Event::ToolRequest {
            context: run.context.clone(),
            tool_calls,
            allowed_tool_names: Some(allowed_tool_names),
        });
        let results =
            wait_for_tool_response(&mut events, &run.context.invoked_by, &expected).await?;
        let result_messages = results
            .iter()
            .map(|result| Message::ToolResult {
                tool_call_id: result.tool_call_id.clone(),
                tool_name: result.tool_name.clone(),
                result: crate::tools::tool_result_json(&result.result),
            })
            .collect::<Vec<_>>();
        self.history
            .append_tool_turn(&run.history_scope, &output, &result_messages)?;
        logger::raw_tool_results_logged(&result_messages);
        Ok(())
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
                        ));
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

    fn ingest_memory(&self, scope: &HistoryScope, final_message: Option<Message>) {
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
        let mut current = entries[user_index..]
            .iter()
            .map(|entry| entry.message.clone())
            .collect::<Vec<_>>();
        current.extend(final_message);
        let memory = Arc::clone(&self.memory);
        tokio::spawn(async move {
            if let Err(error) = memory.ingest(&previous, &current).await {
                debug_log(format!("memory ingestion failed: {error}"));
            }
        });
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
            .execute(
                InferenceRequest {
                    model: self.inference.provider().text_model.clone(),
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
                },
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
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

struct AgentRun<'a> {
    destination: Destination<'a>,
    history_scope: HistoryScope,
    context: ExecutionContext,
    transient_messages: Vec<Message>,
    tool_selection: ToolSelection,
    recalled_memories: Vec<String>,
}

enum ToolSelection {
    AgentVisible,
    Selected(Vec<Arc<dyn InternalTool>>),
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

async fn wait_for_tool_response(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    scope: &InvocationScope,
    expected: &HashSet<String>,
) -> Result<Vec<crate::tools::ToolCallResult>, String> {
    loop {
        match events.recv().await {
            Ok(Event::ToolResponse {
                context,
                tool_call_ids,
                result,
            }) if &context.invoked_by == scope
                && tool_call_ids.iter().collect::<HashSet<_>>()
                    == expected.iter().collect::<HashSet<_>>() =>
            {
                return result;
            }
            Ok(_) => {}
            Err(error) => return Err(format!("failed waiting for tool result: {error}")),
        }
    }
}

fn debug_log(message: impl AsRef<str>) {
    eprintln!("{}", message.as_ref());
}

use std::{collections::HashSet, sync::Arc};

use crate::{
    bus::{Bus, ChannelAction, ConversationKey, Event},
    history::{History, HistoryEntry, HistoryId, MessageMetadata},
    inference::{Inference, InferenceError, InferenceRequest},
    memory::Memory,
    message::{Message, ToolCall, ToolDefinition},
    tools::{self, ExecutionContext, InternalTool, InvocationScope, ToolRegistry},
};

use super::{logger, preamble::system_preamble};

const DEFAULT_TASK_TOOLS: [&str; 4] = ["list_files", "read_file", "create_file", "update_file"];
const COMPACT_AT_PROMPT_TOKENS: u64 = 256_000;

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
        conversation: &ConversationKey,
        context: ExecutionContext,
    ) -> Result<Option<String>, String> {
        let history_id = HistoryId::Conversation(conversation.clone());
        let core_memories = match self.memory.core() {
            Ok(memories) => memories,
            Err(error) => {
                debug_log(format!("core memory load failed: {error}"));
                Vec::new()
            }
        };
        let recalled_memories = self.recall_memories(&history_id).await?;
        let tools = self.tools.agent_tools();
        let mut messages = system_preamble(Some(conversation));
        if !core_memories.is_empty() {
            let core = core_memories
                .iter()
                .map(|memory| format!("- {memory}"))
                .collect::<Vec<_>>()
                .join("\n");
            messages.push(Message::System(format!(
                "[core user memory]\n{core}\nTreat these as established user facts."
            )));
        }
        if !recalled_memories.is_empty() {
            let recalled = recalled_memories
                .iter()
                .map(|memory| format!("- {memory}"))
                .collect::<Vec<_>>()
                .join("\n");
            messages.push(Message::System(format!(
                "[recalled user memory]\n{recalled}\nUse these memories only when relevant."
            )));
        }
        let run = AgentRun::new(history_id.clone(), context, tools, messages);
        let output = self.run(run).await?;
        let Message::Assistant { text, .. } = &output else {
            unreachable!("run only returns assistant messages");
        };
        let Some(text) = text.clone().filter(|text| !text.is_empty()) else {
            return Ok(None);
        };
        self.ingest_memory(&history_id, output);
        Ok(Some(text))
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
        let history_id = HistoryId::Automation(automation_id.clone());
        self.history.append_history(
            &history_id,
            &[HistoryEntry {
                message: Message::User {
                    text: prompt,
                    attachments: Vec::new(),
                },
                metadata: MessageMetadata::for_automation(automation_id.clone()),
            }],
        )?;
        let run = AgentRun::new(
            history_id.clone(),
            ExecutionContext::new(InvocationScope::ScheduledAutomation {
                automation_id: automation_id.clone(),
            }),
            tools,
            system_preamble(None),
        );
        let output = self.run(run).await?;
        let Message::Assistant { text, .. } = &output else {
            unreachable!("run only returns assistant messages");
        };
        let Some(text) = text.clone().filter(|text| !text.is_empty()) else {
            return Ok(None);
        };
        self.history.append_history(
            &history_id,
            &[HistoryEntry {
                message: output,
                metadata: MessageMetadata::for_automation(automation_id.clone()),
            }],
        )?;
        Ok(Some(text))
    }

    async fn run(&self, mut run: AgentRun) -> Result<Message, String> {
        let mut round = 0;
        loop {
            round += 1;
            run.transient_messages
                .extend(run.context.drain_tool_budget_messages());
            self.publish_action(&run.context, ChannelAction::Typing);

            let messages = self.model_messages(&run)?;
            debug_log(format!(
                "agent: round {round} calling chat_completions(messages={}, tools={})",
                messages.len(),
                run.tool_definitions.len()
            ));
            let request = InferenceRequest {
                model: self.inference.provider().text_model.clone(),
                messages,
                tools: run.tool_definitions.clone(),
            };
            logger::raw_inference_request_logged(&request);
            let inference = match self.inference.execute(request).await {
                Ok(inference) => inference,
                Err(InferenceError::ApiKey(_)) => {
                    return Err(
                        "AI provider authentication failed. Run /change_api_key to replace the key."
                            .to_string(),
                    );
                }
                Err(error) => return Err(error.to_string()),
            };
            logger::raw_inference_response_logged(&inference);
            self.compact_if_needed(&run, inference.usage.prompt_tokens)
                .await;

            let mut output = inference.message;
            let Message::Assistant {
                text, tool_calls, ..
            } = &mut output
            else {
                return Err("model did not return an assistant message".to_string());
            };
            if let Some(text) = text {
                *text = strip_message_id_prefix(text).to_string();
            }
            let tool_calls = tool_calls.clone();
            run.transient_messages.clear();
            if tool_calls.is_empty() {
                return Ok(output);
            }
            self.handle_tool_calls(&mut run, output, tool_calls).await?;
        }
    }

    async fn recall_memories(&self, history_id: &HistoryId) -> Result<Vec<String>, String> {
        let Some(query) = self.latest_user_text(history_id)? else {
            return Ok(Vec::new());
        };
        match self.memory.recall(&query).await {
            Ok(memories) => Ok(memories),
            Err(error) => {
                debug_log(format!("memory recall failed: {error}"));
                Ok(Vec::new())
            }
        }
    }

    async fn compact_if_needed(&self, run: &AgentRun, prompt_tokens: u64) {
        if prompt_tokens < COMPACT_AT_PROMPT_TOKENS {
            return;
        }
        self.publish_action(
            &run.context,
            ChannelAction::Progress {
                text: "compacting history...".to_string(),
            },
        );
        if let Err(error) = self.summarize_conversation(&run.history_id).await {
            debug_log(format!("history compaction failed: {error}"));
        }
    }

    async fn handle_tool_calls(
        &self,
        run: &mut AgentRun,
        output: Message,
        tool_calls: Vec<ToolCall>,
    ) -> Result<(), String> {
        for tool_call in &tool_calls {
            logger::raw_tool_call_logged(tool_call);
            self.publish_action(
                &run.context,
                ChannelAction::Progress {
                    text: format!("calling {}", tool_call.name),
                },
            );
        }

        let mut events = self.bus.subscribe();
        let expected = tool_calls
            .iter()
            .map(|tool_call| tool_call.id.clone())
            .collect::<HashSet<_>>();
        self.bus.publish(Event::ToolRequest {
            context: run.context.clone(),
            tool_calls,
            allowed_tool_names: Some(run.allowed_tool_names.clone()),
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
            .append_tool_turn(&run.history_id, &output, &result_messages)?;
        logger::raw_tool_results_logged(&result_messages);
        Ok(())
    }

    fn model_messages(&self, run: &AgentRun) -> Result<Vec<Message>, String> {
        let mut messages = run.initial_messages.clone();
        for entry in self.history.load_conversation(&run.history_id)? {
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

    fn publish_action(&self, context: &ExecutionContext, action: ChannelAction) {
        if let Some(conversation) = context.conversation() {
            self.bus.publish(Event::ChannelAction {
                conversation: conversation.clone(),
                action,
            });
        }
    }

    fn latest_user_text(&self, history_id: &HistoryId) -> Result<Option<String>, String> {
        Ok(self
            .history
            .load_conversation(history_id)?
            .into_iter()
            .rev()
            .find_map(|entry| match entry.message {
                Message::User { text, .. } => Some(text),
                _ => None,
            }))
    }

    fn ingest_memory(&self, history_id: &HistoryId, final_message: Message) {
        let Ok(entries) = self.history.load_conversation(history_id) else {
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
        current.push(final_message);
        let memory = Arc::clone(&self.memory);
        tokio::spawn(async move {
            if let Err(error) = memory.ingest(&previous, &current).await {
                debug_log(format!("memory ingestion failed: {error}"));
            }
        });
    }

    async fn summarize_conversation(&self, history_id: &HistoryId) -> Result<(), String> {
        let conversation = self.history.load_conversation(history_id)?;
        let Some(latest_user_index) = conversation
            .iter()
            .rposition(|entry| matches!(entry.message, Message::User { .. }))
        else {
            return Ok(());
        };
        let Some(boundary_index) = (0..latest_user_index)
            .rev()
            .find(|index| !is_history_summary(&conversation[*index].message))
        else {
            return Ok(());
        };
        let compacted_through_entry_id = conversation[boundary_index].metadata.id.clone();
        let messages = conversation[..=boundary_index]
            .iter()
            .map(|entry| entry.message.clone())
            .collect::<Vec<_>>();
        let transcript = serde_json::to_string(&messages).map_err(|err| err.to_string())?;
        let response = self
            .inference
            .execute(InferenceRequest {
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
                })
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
        self.history
            .save_conversation_summary(history_id, &summary, &compacted_through_entry_id)
    }
}

struct AgentRun {
    history_id: HistoryId,
    context: ExecutionContext,
    initial_messages: Vec<Message>,
    tool_definitions: Vec<ToolDefinition>,
    allowed_tool_names: Vec<String>,
    transient_messages: Vec<Message>,
}

impl AgentRun {
    fn new(
        history_id: HistoryId,
        context: ExecutionContext,
        tools: Vec<Arc<dyn InternalTool>>,
        initial_messages: Vec<Message>,
    ) -> Self {
        let tool_definitions = tools
            .iter()
            .map(|tool| tools::definition_for_model(tool.as_ref()))
            .collect();
        let allowed_tool_names = tools.iter().map(|tool| tool.name().to_string()).collect();
        Self {
            history_id,
            context,
            initial_messages,
            tool_definitions,
            allowed_tool_names,
            transient_messages: Vec::new(),
        }
    }
}

fn is_history_summary(message: &Message) -> bool {
    matches!(message, Message::System(text) if text.starts_with("[history summary] "))
}

fn strip_message_id_prefix(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("[message_id:") else {
        return text;
    };
    let Some((id, content)) = rest.split_once(']') else {
        return text;
    };
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return text;
    }
    content.trim_start()
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

#[cfg(test)]
mod tests {
    use super::strip_message_id_prefix;

    #[test]
    fn strips_leaked_message_id_from_model_output() {
        assert_eq!(
            strip_message_id_prefix(
                "[message_id:f17a7800-6a97-4827-a0c5-0e52221a5d9d] Is this Outlook?"
            ),
            "Is this Outlook?"
        );
    }

    #[test]
    fn preserves_non_metadata_brackets() {
        assert_eq!(
            strip_message_id_prefix("[message_id:example.com] text"),
            "[message_id:example.com] text"
        );
    }
}

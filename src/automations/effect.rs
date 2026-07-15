use crate::{
    bus::{AgentRequest, Bus, Event, InvocationScope},
    message::ToolCall,
    tools::ExecutionContext,
};

use super::render_args::render_args;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutomationEffect {
    Tool {
        tool: String,
        args: serde_json::Value,
    },
    AgentTask {
        prompt: String,
        tools: Vec<String>,
    },
    ContinueConversation,
}

impl AutomationEffect {
    pub fn dispatch(
        &self,
        bus: &Bus,
        execution_id: &str,
        automation_id: &str,
        context: &ExecutionContext,
        trigger: &serde_json::Value,
    ) {
        match self {
            Self::Tool { tool, args } => {
                let args = render_args(args, trigger);
                bus.publish(Event::ToolRequest {
                    context: context.clone(),
                    tool_call: ToolCall {
                        id: execution_id.to_string(),
                        name: tool.clone(),
                        arguments: args.to_string(),
                    },
                    args,
                    record_in_history: false,
                });
            }
            Self::AgentTask { prompt, tools } => {
                bus.publish(Event::AgentRequest {
                    execution_id: execution_id.to_string(),
                    request: AgentRequest::Task {
                        automation_id: automation_id.to_string(),
                        prompt: render_text(prompt, trigger),
                        tools: tools.clone(),
                    },
                });
            }
            Self::ContinueConversation => match &context.invoked_by {
                InvocationScope::Conversation(conversation) => {
                    bus.publish(Event::AgentRequest {
                        execution_id: execution_id.to_string(),
                        request: AgentRequest::ContinueConversation {
                            conversation: conversation.clone(),
                            context: context.clone(),
                        },
                    });
                }
                InvocationScope::ScheduledAutomation { .. } => {
                    bus.publish(Event::AgentResult {
                        execution_id: execution_id.to_string(),
                        result: Err(
                            "continue_conversation requires a conversation trigger".to_string()
                        ),
                    });
                }
            },
        }
    }
}

fn render_text(template: &str, event: &serde_json::Value) -> String {
    let rendered = render_args(&serde_json::Value::String(template.to_string()), event);
    match rendered {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    }
}

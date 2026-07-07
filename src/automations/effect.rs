use std::sync::Arc;

use crate::{
    agent::Agent,
    bus::{Bus, Event, InvocationScope, Sender},
    tools::{self, ExecutionContext, InternalTools},
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
    pub async fn execute(
        &self,
        bus: &Bus,
        tools: &Arc<InternalTools>,
        agent: &Agent,
        ctx: &ExecutionContext,
        event: &serde_json::Value,
    ) -> serde_json::Value {
        match self {
            Self::Tool { tool, args } => {
                let args = render_args(args, event);
                tools::run_tool(tools, tool, args, ctx).await
            }
            Self::AgentTask { prompt, tools } => {
                let prompt = render_text(prompt, event);
                match agent.run_task(prompt, tools.clone()).await {
                    Ok(result) => serde_json::json!({ "ok": true, "result": result }),
                    Err(err) => serde_json::json!({ "ok": false, "error": err }),
                }
            }
            Self::ContinueConversation => match &ctx.invoked_by {
                InvocationScope::Conversation(conversation) => {
                    match agent.handle_conversation(Some(conversation)).await {
                        Ok(result) => {
                            if let Some(text) = result {
                                send_to_destination(bus, conversation, text);
                            }
                            serde_json::json!({ "ok": true })
                        }
                        Err(err) => serde_json::json!({ "ok": false, "error": err }),
                    }
                }
                InvocationScope::ScheduledAutomation { automation_id } => serde_json::json!({
                    "ok": false,
                    "error": format!("continue_conversation cannot run from scheduled automation '{automation_id}'"),
                }),
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

fn send_to_destination(bus: &Bus, conversation: &crate::bus::ConversationKey, text: String) {
    bus.publish(Event::MessageOut {
        channel: conversation.channel.clone(),
        recipient: Sender::User,
        conversation: Some(conversation.clone()),
        message: crate::message::Message::Assistant {
            text: Some(text),
            attachments: Vec::new(),
            tool_calls: Vec::new(),
        },
    });
}

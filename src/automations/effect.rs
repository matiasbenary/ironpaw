use std::sync::Arc;

use crate::{
    agent::Agent,
    bus::{Bus, Event, Sender},
    tools::{self, InternalTools, ToolContext},
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
    ContinueConversation {
        conversation_id: String,
    },
}

impl AutomationEffect {
    pub async fn execute(
        &self,
        bus: &Bus,
        tools: &Arc<InternalTools>,
        agent: &Agent,
        ctx: &ToolContext,
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
            Self::ContinueConversation { conversation_id } => {
                let conversation_id = render_text(conversation_id, event);
                match agent
                    .handle_conversation(
                        ctx.destination
                            .as_ref()
                            .map(|(channel, address)| (channel.as_str(), address)),
                        &conversation_id,
                    )
                    .await
                {
                    Ok(result) => {
                        if let Some(text) = result {
                            send_to_destination(bus, ctx, text);
                        }
                        serde_json::json!({ "ok": true })
                    }
                    Err(err) => serde_json::json!({ "ok": false, "error": err }),
                }
            }
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

fn send_to_destination(bus: &Bus, ctx: &ToolContext, text: String) {
    let Some((channel, address)) = &ctx.destination else {
        eprintln!("automation result has no destination: {text}");
        return;
    };
    bus.publish(Event::MessageOut {
        channel: channel.clone(),
        recipient: Sender::User,
        address: Some(address.clone()),
        message: crate::message::Message::Assistant {
            text: Some(text),
            attachments: Vec::new(),
            tool_calls: Vec::new(),
        },
    });
}

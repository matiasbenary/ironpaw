use crate::{
    bus::{AgentRequest, Bus, Event},
    message::ToolCall,
    tools::ExecutionContext,
};

use super::render_args::render_args;

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
pub struct ToolEffect {
    pub tool: String,
    /// Tool arguments. In message automations, use `{{event.text}}` and other
    /// `{{event.field}}` placeholders to include data from the inbound message.
    pub args: serde_json::Value,
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
pub struct AgentTaskEffect {
    /// Task instructions. In message automations, use `{{event.text}}` and
    /// other `{{event.field}}` placeholders to include inbound message data.
    pub prompt: String,
    pub tools: Vec<String>,
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutomationEffect {
    Tool(ToolEffect),
    AgentTask(AgentTaskEffect),
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserMessageEffect {
    Tool(ToolEffect),
    AgentTask(AgentTaskEffect),
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
            Self::Tool(effect) => effect.dispatch(bus, execution_id, context, trigger),
            Self::AgentTask(effect) => effect.dispatch(bus, execution_id, automation_id, trigger),
        }
    }
}

impl UserMessageEffect {
    pub fn dispatch(
        &self,
        bus: &Bus,
        execution_id: &str,
        automation_id: &str,
        context: &ExecutionContext,
        trigger: &serde_json::Value,
    ) {
        match self {
            Self::Tool(effect) => effect.dispatch(bus, execution_id, context, trigger),
            Self::AgentTask(effect) => effect.dispatch(bus, execution_id, automation_id, trigger),
            Self::ContinueConversation => {
                let conversation = context
                    .conversation()
                    .expect("message automation has conversation context");
                bus.publish(Event::AgentRequest {
                    execution_id: execution_id.to_string(),
                    request: AgentRequest::ContinueConversation {
                        conversation: conversation.clone(),
                        context: context.clone(),
                    },
                });
            }
        }
    }
}

impl ToolEffect {
    fn dispatch(
        &self,
        bus: &Bus,
        execution_id: &str,
        context: &ExecutionContext,
        trigger: &serde_json::Value,
    ) {
        let args = render_args(&self.args, trigger);
        bus.publish(Event::ToolRequest {
            context: context.clone(),
            tool_calls: vec![ToolCall {
                id: execution_id.to_string(),
                name: self.tool.clone(),
                arguments: args.to_string(),
            }],
            allowed_tool_names: None,
        });
    }
}

impl AgentTaskEffect {
    fn dispatch(
        &self,
        bus: &Bus,
        execution_id: &str,
        automation_id: &str,
        trigger: &serde_json::Value,
    ) {
        bus.publish(Event::AgentRequest {
            execution_id: execution_id.to_string(),
            request: AgentRequest::Task {
                automation_id: automation_id.to_string(),
                prompt: render_text(&self.prompt, trigger),
                tools: self.tools.clone(),
            },
        });
    }
}

fn render_text(template: &str, event: &serde_json::Value) -> String {
    let rendered = render_args(&serde_json::Value::String(template.to_string()), event);
    match rendered {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    }
}

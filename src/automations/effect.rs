use crate::{
    bus::{AgentRequest, Bus, Event},
    message::ToolCall,
    tools::ExecutionContext,
};

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

fn render_args(args: &serde_json::Value, event: &serde_json::Value) -> serde_json::Value {
    match args {
        serde_json::Value::String(text) => render_string(text, event),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(|item| render_args(item, event)).collect())
        }
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), render_args(value, event)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn render_string(text: &str, event: &serde_json::Value) -> serde_json::Value {
    let trimmed = text.trim();
    if let Some(path) = exact_placeholder(trimmed) {
        return lookup(event, path)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
    }

    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            result.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let path = after[..end].trim();
        match lookup(event, path) {
            Some(serde_json::Value::String(value)) => result.push_str(value),
            Some(value) => result.push_str(&value.to_string()),
            None => {}
        }
        rest = &after[end + 2..];
    }
    result.push_str(rest);
    serde_json::Value::String(result)
}

fn exact_placeholder(text: &str) -> Option<&str> {
    let inner = text.strip_prefix("{{")?.strip_suffix("}}")?;
    let inner = inner.trim();
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    Some(inner)
}

fn lookup<'a>(event: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let path = path.strip_prefix("event.")?;
    let mut current = event;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_placeholders_fill_from_the_event() {
        let event = serde_json::json!({
            "channel": "tui",
            "text": "hi",
        });
        let args = serde_json::json!({
            "message": "{{event.text}}",
            "summary": "got '{{event.text}}' on {{event.channel}}",
            "missing": "{{event.nope}}",
            "untouched": 42,
        });

        let rendered = render_args(&args, &event);

        assert_eq!(rendered["message"], "hi");
        assert_eq!(rendered["summary"], "got 'hi' on tui");
        assert_eq!(rendered["missing"], serde_json::Value::Null);
        assert_eq!(rendered["untouched"], 42);
    }
}

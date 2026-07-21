use async_trait::async_trait;

use crate::{
    bus::{Bus, Event, InvocationScope, UserInputOption, UserInputRequest},
    tools::{self, ExecutionContext, InternalTool, ToolResponder},
};

pub struct RequestUserInputTool {
    bus: Bus,
}

impl RequestUserInputTool {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }
}

#[async_trait]
impl InternalTool for RequestUserInputTool {
    fn name(&self) -> &str {
        "request_user_input"
    }

    fn tool_definition(&self) -> crate::message::ToolDefinition {
        tools::tool_definition::<RequestUserInputParams>(
            self.name(),
            "Ask the user a question with specific possible answers. Use this only when the answer is required to continue. After calling this tool, do not continue the task, make assumptions, or repeat the question; end your response and wait for the user's next message.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = (|| {
            let input: RequestUserInputParams =
                serde_json::from_value(args).map_err(|err| err.to_string())?;
            let InvocationScope::Conversation(conversation) = &ctx.invoked_by else {
                return Err("request_user_input requires an active conversation".to_string());
            };
            validate(&input)?;
            self.bus.publish(Event::UserInputRequested {
                conversation: conversation.clone(),
                request: UserInputRequest {
                    question: input.question,
                    options: input
                        .options
                        .into_iter()
                        .map(|option| UserInputOption {
                            label: option.label,
                            value: option.value,
                        })
                        .collect(),
                },
            });
            Ok(serde_json::json!({ "status": "question_sent" }))
        })()
        .map_err(crate::tools::ToolError::new);
        responder.respond(result);
    }
}

#[derive(Debug, schemars::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestUserInputParams {
    /// The question displayed to the user.
    question: String,
    /// Answers the user may choose from.
    options: Vec<RequestUserInputOption>,
}

#[derive(Debug, schemars::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestUserInputOption {
    /// Short user-facing button label.
    label: String,
    /// Stable value returned as the user's answer. Must be at most 64 bytes.
    value: String,
}

fn validate(input: &RequestUserInputParams) -> Result<(), String> {
    if input.question.trim().is_empty() {
        return Err("question must not be empty".to_string());
    }
    if input.options.is_empty() {
        return Err("options must not be empty".to_string());
    }
    for option in &input.options {
        if option.label.trim().is_empty() {
            return Err("option labels must not be empty".to_string());
        }
        if option.value.is_empty() || option.value.len() > 64 {
            return Err("option values must be between 1 and 64 bytes".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bus::ConversationKey,
        tools::{InvocationScope, run_internal_tool},
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn publishes_request_to_current_conversation() {
        let bus = Bus::new(4);
        let mut events = bus.subscribe();
        let context = ExecutionContext::new(InvocationScope::Conversation(ConversationKey {
            channel: "telegram".to_string(),
            chat_id: "42".to_string(),
            thread_id: None,
        }));

        let result = run_internal_tool(
            Arc::new(RequestUserInputTool::new(bus)),
            serde_json::json!({
                "question": "Choose a database",
                "options": [
                    { "label": "PostgreSQL", "value": "postgres" },
                    { "label": "SQLite", "value": "sqlite" }
                ]
            }),
            &context,
        )
        .await;

        assert_eq!(result.unwrap()["status"], "question_sent");
        let Event::UserInputRequested {
            conversation,
            request,
        } = events.try_recv().unwrap()
        else {
            panic!("expected user input request");
        };
        assert_eq!(conversation.chat_id, "42");
        assert_eq!(request.question, "Choose a database");
        assert_eq!(request.options[0].value, "postgres");
    }

    #[test]
    fn rejects_callback_values_over_telegram_limit() {
        let input = RequestUserInputParams {
            question: "Choose".to_string(),
            options: vec![RequestUserInputOption {
                label: "Long".to_string(),
                value: "x".repeat(65),
            }],
        };

        assert_eq!(
            validate(&input).unwrap_err(),
            "option values must be between 1 and 64 bytes"
        );
    }
}

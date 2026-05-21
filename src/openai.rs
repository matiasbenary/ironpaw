pub use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls, ChatCompletionRequestMessage,
    ChatCompletionRequestToolMessage, ChatCompletionRequestToolMessageContent,
    ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageContent,
    ChatCompletionResponseMessage, ChatCompletionTool, ChatCompletionToolChoiceOption,
    ChatCompletionTools, CreateChatCompletionRequest, CreateChatCompletionResponse, FunctionObject,
    ToolChoiceOptions,
};

pub fn response_message_to_request_message(
    message: &ChatCompletionResponseMessage,
) -> Result<ChatCompletionRequestMessage, String> {
    serde_json::from_value(serde_json::to_value(message).map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string())
}

pub fn tool_result_message(
    tool_call_id: String,
    result: serde_json::Value,
) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Tool(ChatCompletionRequestToolMessage {
        content: ChatCompletionRequestToolMessageContent::Text(result.to_string()),
        tool_call_id,
    })
}

pub fn first_response_message(
    response: CreateChatCompletionResponse,
) -> ChatCompletionResponseMessage {
    response
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message)
        .unwrap_or_else(|| assistant_error_message("AI response had no choices"))
}

pub fn assistant_error_message(content: impl Into<String>) -> ChatCompletionResponseMessage {
    assistant_response_message(content)
}

pub fn assistant_response_message(content: impl Into<String>) -> ChatCompletionResponseMessage {
    serde_json::from_value(serde_json::json!({
        "role": "assistant",
        "content": content.into(),
    }))
    .expect("static assistant error message should match OpenAI chat schema")
}

pub fn user_request_message(
    content: ChatCompletionRequestUserMessageContent,
) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
        content,
        name: None,
    })
}

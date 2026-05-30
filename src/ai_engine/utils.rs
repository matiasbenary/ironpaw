use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    message::OutboundMessage,
    openai::{
        FunctionCallOutput, FunctionCallOutputItemParam, InputItem, Item, OutputItem,
        OutputMessageContent,
    },
};

pub(super) struct ParsedResponseOutput {
    pub(super) function_calls: Vec<crate::openai::FunctionToolCall>,
    pub(super) text: String,
}

pub(super) fn parse_response_output(response: crate::openai::Response) -> ParsedResponseOutput {
    let mut function_calls = Vec::new();
    let mut text_parts = Vec::new();

    for item in response.output {
        match &item {
            OutputItem::Message(message) => {
                for content in &message.content {
                    match content {
                        OutputMessageContent::OutputText(text) => {
                            if !text.text.trim().is_empty() {
                                text_parts.push(text.text.clone());
                            }
                        }
                        OutputMessageContent::Refusal(refusal) => {
                            text_parts.push(refusal.refusal.clone());
                        }
                    }
                }
            }
            OutputItem::FunctionCall(call) => {
                function_calls.push(call.clone());
            }
            _ => {}
        }
    }

    ParsedResponseOutput {
        function_calls,
        text: text_parts.join("\n\n"),
    }
}

pub(super) fn contains_input_media(input_items: &[InputItem]) -> bool {
    input_items
        .iter()
        .filter_map(|item| serde_json::to_value(item).ok())
        .any(|value| value_contains_input_media(&value))
}

fn value_contains_input_media(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            if object
                .get("type")
                .and_then(|value| value.as_str())
                .is_some_and(|r#type| matches!(r#type, "input_image" | "input_file"))
            {
                return true;
            }
            object.values().any(value_contains_input_media)
        }
        serde_json::Value::Array(values) => values.iter().any(value_contains_input_media),
        _ => false,
    }
}

pub(super) fn fill_current_destination(
    args: &mut serde_json::Value,
    channel: &str,
    address: &ChannelAddress,
) {
    let Some(object) = args.as_object_mut() else {
        return;
    };
    if object.get("channel").is_some() {
        return;
    }
    object.insert(
        "channel".to_string(),
        serde_json::Value::String(channel.to_string()),
    );
    object
        .entry("conversation_id")
        .or_insert_with(|| serde_json::Value::String(address.conversation_id.clone()));
    if let Some(thread_id) = &address.thread_id {
        object
            .entry("thread_id")
            .or_insert_with(|| serde_json::Value::String(thread_id.clone()));
    }
}

pub(super) fn tool_result_input_item(tool_call_id: &str, result: impl AsRef<str>) -> InputItem {
    Item::from(FunctionCallOutputItemParam {
        call_id: tool_call_id.to_string(),
        output: FunctionCallOutput::Text(result.as_ref().to_string()),
        id: None,
        status: None,
    })
    .into()
}

pub(super) fn publish_progress(
    bus: &Bus,
    channel: &str,
    address: &ChannelAddress,
    content: String,
) {
    bus.publish(Event::MessageOut {
        channel: channel.to_string(),
        recipient: Sender::User,
        address: Some(address.clone()),
        message: OutboundMessage {
            content: Some(content),
            attachments: Vec::new(),
        },
    });
}

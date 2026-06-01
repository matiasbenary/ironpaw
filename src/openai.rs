pub use async_openai::types::responses::{
    CreateResponse, EasyInputContent, EasyInputMessage, FunctionCallOutput,
    FunctionCallOutputItemParam, FunctionTool, FunctionToolCall, ImageDetail, InputContent,
    InputFileContent, InputImageContent, InputItem, InputMessage, InputParam, InputRole,
    InputTextContent, Item, MessageItem, OutputItem, OutputMessageContent, Response, Role, Status,
    Tool,
};

pub struct ParsedResponseOutput {
    pub function_calls: Vec<FunctionToolCall>,
    pub text: String,
}

pub fn response_text(response: Response) -> String {
    output_text(response.output)
}

pub fn parse_response_output(response: Response) -> ParsedResponseOutput {
    let mut function_calls = Vec::new();
    let mut message_items = Vec::new();

    for item in response.output {
        match item {
            OutputItem::FunctionCall(call) => function_calls.push(call),
            item => message_items.push(item),
        }
    }

    ParsedResponseOutput {
        function_calls,
        text: output_text(message_items),
    }
}

fn output_text(items: Vec<OutputItem>) -> String {
    let mut text_parts = Vec::new();

    for item in items {
        if let OutputItem::Message(message) = item {
            for content in message.content {
                match content {
                    OutputMessageContent::OutputText(text) => {
                        if !text.text.trim().is_empty() {
                            text_parts.push(text.text);
                        }
                    }
                    OutputMessageContent::Refusal(refusal) => {
                        text_parts.push(refusal.refusal);
                    }
                }
            }
        }
    }

    text_parts.join("\n\n")
}

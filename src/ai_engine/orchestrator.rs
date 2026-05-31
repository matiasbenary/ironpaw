use async_openai::types::chat::{
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
    CreateChatCompletionRequestArgs, ResponseFormat,
};

use crate::{
    message::OutboundAttachment,
    openai::{EasyInputContent, EasyInputMessage, InputItem, Role},
    tools::{
        ai_inference::AiInferenceTool,
        send_message::{SendMessageParams, SendMessageTool},
    },
};

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub(super) struct ExecutionPlan {
    #[serde(default)]
    user_goal: String,
    #[serde(default)]
    requirements: Vec<Requirement>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct Requirement {
    id: String,
    action: String,
    #[serde(default)]
    object: RequirementObject,
    #[serde(default)]
    object_ref: Option<String>,
    #[serde(default)]
    target: Option<RequirementTarget>,
    #[serde(default)]
    depends_on: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct RequirementObject {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    ref_id: Option<String>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct RequirementTarget {
    kind: String,
    name: String,
}

pub(super) async fn construct_execution_plan(
    ai_inference: &AiInferenceTool,
    channel: &str,
    messages: &[InputItem],
) -> Result<ExecutionPlan, String> {
    let user_text = extract_text_from_input_items(messages);
    if user_text.trim().is_empty() {
        return Ok(ExecutionPlan::default());
    }

    let model = std::env::var("NEAR_MODEL")
        .unwrap_or_else(|_| super::ai_engine::DEFAULT_MODEL.to_string());
    let request = CreateChatCompletionRequestArgs::default()
        .model(model)
        .messages([
            ChatCompletionRequestSystemMessageArgs::default()
                .content(format!(
                    "Extract compact operational requirements from the user's request. \
                    Return only valid JSON with this shape: \
                    {{\"user_goal\":\"...\",\"requirements\":[{{\"id\":\"r1\",\"action\":\"create|deliver|reply|fetch|transform\",\"object\":{{\"kind\":\"image|text|file|generated_file|unknown\",\"description\":\"...\",\"ref_id\":\"...\"}},\"object_ref\":\"r1\",\"target\":{{\"kind\":\"channel|contact|current_channel\",\"name\":\"...\"}},\"depends_on\":\"r1\"}}]}}. \
                    Use deliver for requests to send/share/post something to a destination. \
                    Use target.kind=\"channel\" for app channels such as telegram or tui. \
                    Current channel is '{channel}'. Omit optional fields when absent."
                ))
                .build()
                .map_err(|err| err.to_string())?
                .into(),
            ChatCompletionRequestUserMessageArgs::default()
                .content(user_text)
                .build()
                .map_err(|err| err.to_string())?
                .into(),
        ])
        .response_format(ResponseFormat::JsonObject)
        .build()
        .map_err(|err| err.to_string())?;

    let response = ai_inference.execute(request).await?;
    let content = response
        .choices
        .first()
        .and_then(|choice| choice.message.content.as_deref())
        .ok_or_else(|| "plan response missing content".to_string())?;
    let mut plan: ExecutionPlan = serde_json::from_str(content).map_err(|err| err.to_string())?;
    normalize_execution_plan(&mut plan);
    Ok(plan)
}

pub(super) fn execution_plan_input_item(plan: &ExecutionPlan) -> InputItem {
    let content = serde_json::to_string(plan).unwrap_or_else(|_| "{}".to_string());
    EasyInputMessage {
        r#type: Default::default(),
        role: Role::System,
        content: EasyInputContent::Text(format!(
            "Execution plan for the current request: {content}. Continue until these requirements are satisfied. If a generated file must be sent to another channel, call send_message with the returned file_id."
        )),
        phase: None,
    }
    .into()
}

pub(super) async fn send_generated_file_to_planned_channels(
    send_message: &SendMessageTool,
    current_channel: &str,
    plan: &ExecutionPlan,
    file_id: u32,
) -> Result<Vec<InputItem>, String> {
    let mut sent = Vec::new();
    for channel in plan.generated_file_channels(current_channel) {
        send_message
            .execute(SendMessageParams {
                channel: channel.clone(),
                content: "Here is the generated image.".to_string(),
                attachments: vec![OutboundAttachment {
                    kind: "image".to_string(),
                    mime_type: "image/png".to_string(),
                    filename: format!("{file_id}.png"),
                    file_id: Some(file_id),
                    data_base64: None,
                    url: None,
                }],
                conversation_id: None,
                thread_id: None,
            })
            .await?;
        sent.push(generated_file_sent_input_item(channel, file_id));
    }
    Ok(sent)
}

impl ExecutionPlan {
    fn generated_file_channels(&self, current_channel: &str) -> Vec<String> {
        self.requirements
            .iter()
            .filter_map(|requirement| {
                let target = requirement.target.as_ref()?;
                if target.kind == "channel" && target.name != current_channel {
                    Some(target.name.clone())
                } else {
                    None
                }
            })
            .collect()
    }
}

fn normalize_execution_plan(plan: &mut ExecutionPlan) {
    plan.requirements.retain(|requirement| {
        !requirement.id.trim().is_empty() && !requirement.action.trim().is_empty()
    });
    for requirement in &mut plan.requirements {
        requirement.action = requirement.action.trim().to_ascii_lowercase();
        requirement.object.kind = requirement.object.kind.trim().to_ascii_lowercase();
        if let Some(target) = &mut requirement.target {
            target.kind = target.kind.trim().to_ascii_lowercase();
            target.name = target.name.trim().to_ascii_lowercase();
        }
    }
}

fn extract_text_from_input_items(items: &[InputItem]) -> String {
    items
        .iter()
        .filter_map(|item| serde_json::to_value(item).ok())
        .flat_map(|value| collect_text_values_for_keys(&value, &["content", "text"]))
        .collect::<Vec<_>>()
        .join("\n")
}

fn collect_text_values_for_keys(value: &serde_json::Value, keys: &[&str]) -> Vec<String> {
    match value {
        serde_json::Value::Object(object) => object
            .iter()
            .flat_map(|(key, value)| {
                if keys.contains(&key.as_str()) {
                    match value {
                        serde_json::Value::String(text) => vec![text.clone()],
                        _ => collect_text_values_for_keys(value, keys),
                    }
                } else {
                    collect_text_values_for_keys(value, keys)
                }
            })
            .collect(),
        serde_json::Value::Array(values) => values
            .iter()
            .flat_map(|value| collect_text_values_for_keys(value, keys))
            .collect(),
        _ => Vec::new(),
    }
}

fn generated_file_sent_input_item(channel: String, file_id: u32) -> InputItem {
    EasyInputMessage {
        r#type: Default::default(),
        role: Role::System,
        content: EasyInputContent::Text(format!(
            "Generated file {file_id} was sent to channel '{channel}'."
        )),
        phase: None,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_plan_input_item_instructs_model_to_send_generated_file() {
        let plan = ExecutionPlan {
            user_goal: "Create and send an image of a dog to Telegram.".to_string(),
            requirements: vec![Requirement {
                id: "r1".to_string(),
                action: "create".to_string(),
                object: RequirementObject {
                    kind: "image".to_string(),
                    description: Some("a dog".to_string()),
                    ref_id: Some("r1".to_string()),
                },
                object_ref: Some("r1".to_string()),
                target: Some(RequirementTarget {
                    kind: "channel".to_string(),
                    name: "telegram".to_string(),
                }),
                depends_on: None,
            }],
        };
        let item = execution_plan_input_item(&plan);
        let value = serde_json::to_value(item).unwrap();

        assert!(value["content"].as_str().unwrap().contains("send_message"));
        assert!(value["content"].as_str().unwrap().contains("file_id"));
    }
}

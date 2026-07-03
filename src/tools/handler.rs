use std::sync::Arc;

use crate::{
    bus::{Bus, Event},
    history::{History, HistoryEntry, MessageMetadata},
    message::{Message, ToolCall},
    tools::{ToolContext, ToolRegistry, run_registered_tool, run_registered_tool_for_context},
};

pub struct ToolHandler {
    bus: Bus,
    registry: Arc<ToolRegistry>,
    history: Arc<dyn History>,
}

impl ToolHandler {
    pub fn new(bus: Bus, registry: Arc<ToolRegistry>, history: Arc<dyn History>) -> Self {
        Self {
            bus,
            registry,
            history,
        }
    }

    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        let mut events = self.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(Event::ToolRequest {
                        context,
                        tool_call,
                        args,
                    }) => this.spawn_tool_request(context, tool_call, args),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    fn spawn_tool_request(
        &self,
        context: ToolContext,
        tool_call: ToolCall,
        args: serde_json::Value,
    ) {
        let bus = self.bus.clone();
        let registry = Arc::clone(&self.registry);
        let history = Arc::clone(&self.history);
        self.persist_tool_call(&context, &tool_call);
        tokio::spawn(async move {
            run_registered_tool_for_context(
                bus,
                Arc::clone(&history),
                &registry,
                &context,
                tool_call.id,
                tool_call.name,
                args,
            )
            .await;
        });
    }

    pub async fn execute_tool_call(
        &self,
        tool_call: ToolCall,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Message {
        self.persist_tool_call(ctx, &tool_call);
        let result = run_registered_tool(&self.registry, &tool_call.name, args, ctx).await;
        let message = Message::ToolResult {
            tool_call_id: tool_call.id,
            tool_name: tool_call.name,
            result,
        };
        persist_tool_result(self.history.as_ref(), ctx, &message);
        message
    }

    fn persist_tool_call(&self, ctx: &ToolContext, tool_call: &ToolCall) {
        let Some(conversation) = &ctx.conversation else {
            return;
        };
        self.history.append(
            conversation,
            &[HistoryEntry {
                message: Message::Assistant {
                    text: None,
                    attachments: Vec::new(),
                    tool_calls: vec![tool_call.clone()],
                },
                metadata: MessageMetadata::new(conversation.clone(), None),
            }],
        );
    }
}

pub(crate) fn persist_tool_result(history: &dyn History, ctx: &ToolContext, message: &Message) {
    let Some(conversation) = &ctx.conversation else {
        return;
    };
    let Message::ToolResult { .. } = message else {
        return;
    };
    history.append(
        conversation,
        &[HistoryEntry {
            message: message.clone(),
            metadata: MessageMetadata::new(conversation.clone(), None),
        }],
    );
}

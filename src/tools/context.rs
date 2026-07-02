use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crate::bus::{Bus, ChannelAddress, Event, ToolOrigin};

/// Context every tool receives alongside its arguments. Tools use what they
/// need and ignore the rest.
#[derive(Clone, Default)]
pub struct ToolContext {
    /// Channel + address of the event that triggered the current run, if any.
    pub destination: Option<(String, ChannelAddress)>,
    /// Conversation the current agent run belongs to.
    pub conversation_id: Option<String>,
    /// Tool call-chain depth, incremented by `run_tool`.
    pub depth: usize,
    /// Original bus request metadata when a tool may finish asynchronously.
    pub tool_call: Option<ToolCallContext>,
    pub(crate) responder: Option<ToolResponder>,
}

#[derive(Clone, Debug)]
pub struct ToolCallContext {
    pub origin: ToolOrigin,
    pub tool_call_id: String,
    pub tool_name: String,
}

impl ToolContext {
    pub(crate) fn deeper(&self) -> Self {
        let mut ctx = self.clone();
        ctx.depth += 1;
        ctx
    }

    pub fn respond(&self, result: serde_json::Value) -> Result<(), String> {
        let responder = self
            .responder
            .as_ref()
            .ok_or_else(|| "tool response context is missing".to_string())?;
        responder.respond(result)
    }

    pub(crate) fn with_responder(&self, responder: ToolResponder) -> Self {
        let mut ctx = self.clone();
        ctx.responder = Some(responder);
        ctx
    }

    pub(crate) fn for_request(
        bus: Bus,
        origin: ToolOrigin,
        tool_call_id: String,
        tool_name: String,
    ) -> Self {
        let (destination, conversation_id) = match &origin {
            ToolOrigin::Conversation {
                channel, address, ..
            } => (
                Some((channel.clone(), address.clone())),
                Some(address.conversation_id.clone()),
            ),
            ToolOrigin::Automation { .. } => (None, None),
        };
        let tool_call = ToolCallContext {
            origin,
            tool_call_id,
            tool_name,
        };
        ToolContext {
            destination,
            conversation_id,
            depth: 0,
            responder: Some(ToolResponder::bus(bus, tool_call.clone())),
            tool_call: Some(tool_call),
        }
    }
}

#[derive(Clone)]
pub struct ToolResponder {
    inner: ToolResponderKind,
    responded: Arc<AtomicBool>,
}

#[derive(Clone)]
enum ToolResponderKind {
    Bus {
        bus: Bus,
        origin: ToolOrigin,
        tool_call_id: String,
        tool_name: String,
    },
    Capture(Arc<Mutex<Option<serde_json::Value>>>),
}

impl ToolResponder {
    pub(crate) fn bus(bus: Bus, tool_call: ToolCallContext) -> Self {
        Self {
            inner: ToolResponderKind::Bus {
                bus,
                origin: tool_call.origin,
                tool_call_id: tool_call.tool_call_id,
                tool_name: tool_call.tool_name,
            },
            responded: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn capture() -> (Self, Arc<Mutex<Option<serde_json::Value>>>) {
        let result = Arc::new(Mutex::new(None));
        (
            Self {
                inner: ToolResponderKind::Capture(Arc::clone(&result)),
                responded: Arc::new(AtomicBool::new(false)),
            },
            result,
        )
    }

    pub fn respond(&self, result: serde_json::Value) -> Result<(), String> {
        if self.responded.swap(true, Ordering::SeqCst) {
            return Err("tool already responded".to_string());
        }
        match &self.inner {
            ToolResponderKind::Bus {
                bus,
                origin,
                tool_call_id,
                tool_name,
            } => {
                bus.publish(Event::ToolResult {
                    origin: origin.clone(),
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    result,
                });
            }
            ToolResponderKind::Capture(captured) => {
                *captured.lock().expect("tool response lock") = Some(result);
            }
        }
        Ok(())
    }

    pub(crate) fn has_responded(&self) -> bool {
        self.responded.load(Ordering::SeqCst)
    }
}

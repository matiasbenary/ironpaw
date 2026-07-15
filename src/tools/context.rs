use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crate::bus::{Bus, Event, ExecutionContext};
use crate::{history::History, message::Message};

#[derive(Clone)]
pub struct ToolResponder {
    inner: ToolResponderKind,
    responded: Arc<AtomicBool>,
}

#[derive(Clone)]
enum ToolResponderKind {
    Bus {
        bus: Bus,
        history: Arc<dyn History>,
        context: ExecutionContext,
        tool_call_id: String,
        tool_name: String,
        record_in_history: bool,
    },
    Capture(Arc<Mutex<Option<serde_json::Value>>>),
}

impl ToolResponder {
    pub(crate) fn bus(
        bus: Bus,
        history: Arc<dyn History>,
        context: ExecutionContext,
        tool_call_id: String,
        tool_name: String,
        record_in_history: bool,
    ) -> Self {
        Self {
            inner: ToolResponderKind::Bus {
                bus,
                history,
                context,
                tool_call_id,
                tool_name,
                record_in_history,
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
                history,
                context,
                tool_call_id,
                tool_name,
                record_in_history,
            } => {
                let message = Message::ToolResult {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    result: result.clone(),
                };
                if *record_in_history {
                    crate::tools::handler::persist_tool_result(
                        history.as_ref(),
                        context,
                        &message,
                    )?;
                }
                bus.publish(Event::ToolResult {
                    context: context.clone(),
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

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crate::bus::{Bus, Event, ExecutionContext};
use crate::{history::History, message::Message, tools::{ToolResult, tool_result_json}};

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
    Capture(Arc<Mutex<Option<ToolResult>>>)
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

    pub(crate) fn capture() -> (Self, Arc<Mutex<Option<ToolResult>>>) {
        let result = Arc::new(Mutex::new(None));
        (
            Self {
                inner: ToolResponderKind::Capture(Arc::clone(&result)),
                responded: Arc::new(AtomicBool::new(false)),
            },
            result,
        )
    }

    pub fn respond(&self, result: ToolResult) {
        if self.responded.swap(true, Ordering::SeqCst) {
            eprintln!("tool responder received a duplicate response");
            return;
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
                let result_json = tool_result_json(&result);
                let message = Message::ToolResult {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    result: result_json,
                };
                if *record_in_history {
                    if let Err(error) = crate::tools::handler::persist_tool_result(
                        history.as_ref(),
                        context,
                        &message,
                    ) {
                        eprintln!("could not persist tool result: {error}");
                    }
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
    }

}

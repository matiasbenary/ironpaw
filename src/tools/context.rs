use std::sync::{Arc, Mutex};

use crate::tools::ToolResult;

#[derive(Clone)]
pub struct ToolResponder {
    sender: Arc<Mutex<Option<tokio::sync::oneshot::Sender<ToolResult>>>>,
}

impl ToolResponder {
    pub(crate) fn pending() -> (Self, tokio::sync::oneshot::Receiver<ToolResult>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        (
            Self {
                sender: Arc::new(Mutex::new(Some(sender))),
            },
            receiver,
        )
    }

    pub fn respond(&self, result: ToolResult) {
        let Some(sender) = self.sender.lock().expect("tool response lock").take() else {
            eprintln!("tool responder received a duplicate response");
            return;
        };
        if sender.send(result).is_err() {
            eprintln!("tool response receiver was dropped");
        }
    }
}

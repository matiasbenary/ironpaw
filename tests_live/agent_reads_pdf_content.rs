use super::common::*;

/// Verify the LLM can read the contents of an attached PDF.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn agent_reads_pdf_content() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_ironpaw(&bus, files_dir.clone());

    let conversation = ConversationKey {
        channel: "tui".to_string(),
        chat_id: "live-test-pdf".to_string(),
        thread_id: None,
    };

    // Mimic what the bridge does: store the file to disk, then tell the model about it.
    let pdf_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests_live/files/test.pdf");
    let pdf_bytes = std::fs::read(pdf_path).expect("test PDF should be readable");
    let filename = "test.pdf".to_string();
    let source = files_dir.join(&filename);
    std::fs::write(&source, &pdf_bytes).unwrap();

    let message = Message::User {
        text: "Call the inspect_file tool to read the attached PDF, then tell me what text it contains."
            .to_string(),
        attachments: vec![Attachment {
            filename,
            mime_type: "application/pdf".to_string(),
            source,
        }],
    };

    publish_user_message(&bus, history.as_ref(), conversation, message);

    let deadline = Duration::from_secs(30);
    let result = timeout(deadline, async {
        loop {
            let event = events.recv().await.expect("bus closed");
            if let Event::MessageOut { message, .. } = event {
                let Message::Assistant {
                    text: Some(content),
                    ..
                } = &message
                else {
                    continue;
                };
                {
                    eprintln!("[live_test] agent replied: {content}");
                    if content.to_lowercase().contains("blue") {
                        return;
                    }
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: agent did not reply with the PDF content within 30 s"
    );
}

use super::common::*;

struct TestClock(std::sync::Mutex<chrono::DateTime<chrono::Utc>>);

impl TestClock {
    fn new(now: &str) -> Self {
        Self(std::sync::Mutex::new(
            chrono::DateTime::parse_from_rfc3339(now)
                .expect("test time")
                .with_timezone(&chrono::Utc),
        ))
    }

    fn set(&self, now: &str) {
        *self.0.lock().expect("test clock") = chrono::DateTime::parse_from_rfc3339(now)
            .expect("test time")
            .with_timezone(&chrono::Utc);
    }
}

impl Clock for TestClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        *self.0.lock().expect("test clock")
    }
}

/// Verify the generated daily album automation returns different recommendations across runs.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture --test-threads=1 album_recommendation_automation
#[tokio::test]
#[ignore = "live: observational test that makes real NEAR AI calls"]
async fn album_recommendation_automation() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let clock = Arc::new(TestClock::new("2026-08-20T10:00:00+02:00"));
    let (_task, history, automations, _, _) =
        start_ironpaw_config_with_clock(&bus, files_dir, clock.clone());
    let existing = automations
        .list()
        .into_iter()
        .map(|automation| automation.name().to_string())
        .collect::<HashSet<_>>();

    let prompt = r#"I am trying to listing to new bands, the more distant from my usual experience the better. The idea is to move away from the mainstream influx of music we get bombarded with. Here are the albums I already listened to:

https://docs.google.com/spreadsheets/d/1bgDum7fxXzIo5bRDJzn7dy33s8bcPZxrf1YF1-jEy1c/edit?usp=sharing

rules:
- prefer non-english speaking bands
- obvious bands are bad candidates (e.g. daft punk, the cure, pink floyd)

every day at 10am I want you to suggest me a new album to listen, include the band name and album name.

Never repeat a recommendation from a previous day.
"#;

    publish_user_message(
        &bus,
        history.as_ref(),
        ConversationKey {
            channel: "tui".to_string(),
            chat_id: "live-album-recommendation".to_string(),
            thread_id: None,
        },
        user_message(prompt),
    );

    let generated = timeout(Duration::from_secs(300), async {
        loop {
            let generated = automations
                .list()
                .into_iter()
                .filter(|automation| !existing.contains(automation.name()))
                .collect::<Vec<_>>();
            if !generated.is_empty() {
                break generated;
            }
            if let Ok(Ok(Event::MessageOut {
                chat_id,
                message:
                    Message::Assistant {
                        text: Some(text), ..
                    },
                ..
            })) = timeout(Duration::from_millis(100), events.recv()).await
                && chat_id.as_deref() == Some("live-album-recommendation")
                && !text.starts_with("calling ")
            {
                eprintln!("[live_test] agent response: {text}");
                break automations
                    .list()
                    .into_iter()
                    .filter(|automation| !existing.contains(automation.name()))
                    .collect();
            }
        }
    })
    .await
    .unwrap_or_default();

    if generated.is_empty() {
        panic!("agent did not create an album automation");
    } else {
        eprintln!(
            "[live_test] generated album automation:\n{}",
            serde_json::to_string_pretty(&generated).expect("serialize generated automation")
        );
    }

    let automation_name = generated[0].name();
    clock.set("2026-08-20T10:00:00+02:00");
    let first = trigger_automation_and_wait(&bus, &mut events, automation_name).await;
    clock.set("2026-08-21T10:00:00+02:00");
    let second = trigger_automation_and_wait(&bus, &mut events, automation_name).await;
    eprintln!("[live_test] first recommendation: {first:?}");
    eprintln!("[live_test] second recommendation: {second:?}");
    assert_ne!(
        first, second,
        "album automation repeated its recommendation"
    );
}

async fn trigger_automation_and_wait(
    bus: &Bus,
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    automation_name: &str,
) -> Option<String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    bus.publish(Event::Automation {
        id: request_id.clone(),
        action: AutomationAction::Trigger {
            name: automation_name.to_string(),
        },
    });
    let execution_id = loop {
        if let Event::AutomationResult { id, result } = events.recv().await.expect("event")
            && id == request_id
        {
            let result = result.expect("trigger automation");
            break result["execution_id"]
                .as_str()
                .expect("execution id")
                .to_string();
        }
    };
    loop {
        if let Event::AgentResult {
            execution_id: result_id,
            result,
        } = events.recv().await.expect("event")
            && result_id == execution_id
        {
            return result.expect("automation agent result");
        }
    }
}

use base64::Engine as _;
use ironpaw::inference::Inference;

/// Verify NEAR AI (or the local whisper server, with WHISPER_URL set) transcribes the 16 kHz mono WAV format the ESP32 client sends.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture inference_transcribes_speech
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn inference_transcribes_speech() {
    let _ = dotenvy::dotenv();
    let near_api_key = std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");
    let secrets = ironpaw::secrets::Secrets::for_tests();
    secrets.set("NEAR_API_KEY", &near_api_key).unwrap();
    let inference = Inference::new(secrets.tools.clone()).expect("create inference");

    let audio = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/live-tests/files/hola.wav"
    ))
    .expect("test audio should be readable");
    let text = inference
        .transcribe(
            "hola.wav".to_string(),
            "audio/wav".to_string(),
            base64::engine::general_purpose::STANDARD.encode(audio),
        )
        .await
        .expect("transcription should succeed");

    eprintln!("[live_test] transcript: {text}");
    assert!(
        text.to_lowercase().contains("hola"),
        "unexpected transcript: {text}"
    );
}

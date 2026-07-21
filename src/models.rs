pub const DEFAULT_TEXT_MODEL: &str = "deepseek-ai/DeepSeek-V4-Flash";
pub const DEFAULT_VISION_MODEL: &str = "google/gemma-4-31B-it";
pub const DEFAULT_IMAGE_MODEL: &str = "black-forest-labs/FLUX.2-klein-4B";

pub fn text_model() -> String {
    std::env::var("NEAR_MODEL").unwrap_or_else(|_| DEFAULT_TEXT_MODEL.to_string())
}

pub fn vision_model() -> String {
    std::env::var("NEAR_VISION_MODEL").unwrap_or_else(|_| DEFAULT_VISION_MODEL.to_string())
}

pub fn image_model() -> String {
    std::env::var("NEAR_IMAGE_MODEL").unwrap_or_else(|_| DEFAULT_IMAGE_MODEL.to_string())
}

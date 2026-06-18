use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Url};

use crate::{
    message::ToolDefinition,
    tools::{self, InternalTool, ToolContext, ToolOutput},
};

const BRAVE_SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const DEFAULT_COUNT: u32 = 5;
const MAX_COUNT: u32 = 20;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct WebSearchParams {
    /// The search query to look up on the web.
    pub query: String,
    /// Number of results to return (1-20, default 5).
    #[serde(default)]
    pub count: Option<u32>,
    /// 2-letter uppercase country code to bias results, e.g. "US".
    #[serde(default)]
    pub country: Option<String>,
    /// 2-letter lowercase language code for search results, e.g. "en".
    #[serde(default)]
    pub search_lang: Option<String>,
    /// Locale in language-region format, e.g. "en-US".
    #[serde(default)]
    pub ui_lang: Option<String>,
    /// Discovery time filter: "pd", "pw", "pm", "py", or
    /// "YYYY-MM-DDtoYYYY-MM-DD".
    #[serde(default)]
    pub freshness: Option<String>,
}

#[derive(serde::Serialize)]
struct WebSearchOutput {
    query: String,
    result_count: usize,
    results: Vec<SearchResult>,
}

#[derive(serde::Serialize)]
struct SearchResult {
    title: String,
    url: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thumbnail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    site_name: Option<String>,
}

#[derive(serde::Deserialize)]
struct BraveSearchResponse {
    web: Option<BraveWebResults>,
}

#[derive(serde::Deserialize)]
struct BraveWebResults {
    results: Option<Vec<BraveSearchResult>>,
}

#[derive(serde::Deserialize)]
struct BraveSearchResult {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
    age: Option<String>,
    thumbnail: Option<BraveThumbnail>,
}

#[derive(serde::Deserialize)]
struct BraveThumbnail {
    src: Option<String>,
}

pub struct WebSearchTool {
    client: Client,
    brave_api_key: Option<String>,
}

impl WebSearchTool {
    pub fn new(brave_api_key: Option<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client should build");
        Self {
            client,
            brave_api_key,
        }
    }

    async fn execute(&self, params: WebSearchParams) -> Result<WebSearchOutput, String> {
        validate_params(&params)?;
        let api_key = self
            .brave_api_key
            .as_deref()
            .ok_or_else(|| "missing tool secret: BRAVE_SEARCH_API".to_string())?;
        let count = params.count.unwrap_or(DEFAULT_COUNT).clamp(1, MAX_COUNT);
        let url = build_search_url(&params, count)?;

        let response = self
            .client
            .get(url)
            .header("Accept", "application/json")
            .header("User-Agent", "Warden-WebSearch/0.1")
            .header("X-Subscription-Token", api_key)
            .send()
            .await
            .map_err(|err| format!("Brave request failed: {}", err.without_url()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| format!("failed to read Brave response: {}", err.without_url()))?;

        if !status.is_success() {
            return Err(format!(
                "Brave API error (HTTP {}): {body}",
                status.as_u16()
            ));
        }

        let response: BraveSearchResponse = serde_json::from_str(&body)
            .map_err(|err| format!("failed to parse Brave response: {err}"))?;
        Ok(format_output(params.query, response))
    }
}

fn validate_params(params: &WebSearchParams) -> Result<(), String> {
    if params.query.trim().is_empty() {
        return Err("'query' must not be empty".to_string());
    }
    if params.query.len() > 2000 {
        return Err("'query' exceeds maximum length of 2000 characters".to_string());
    }
    if let Some(country) = &params.country
        && !is_valid_country_code(country)
    {
        return Err(format!(
            "invalid 'country': expected 2-letter uppercase code like 'US', got '{country}'"
        ));
    }
    if let Some(lang) = &params.search_lang
        && !is_valid_lang_code(lang)
    {
        return Err(format!(
            "invalid 'search_lang': expected 2-letter lowercase code like 'en', got '{lang}'"
        ));
    }
    if let Some(ui_lang) = &params.ui_lang
        && !is_valid_ui_lang(ui_lang)
    {
        return Err(format!(
            "invalid 'ui_lang': expected locale like 'en-US', got '{ui_lang}'"
        ));
    }
    if let Some(freshness) = &params.freshness
        && !is_valid_freshness(freshness)
    {
        return Err(format!(
            "invalid 'freshness': expected 'pd', 'pw', 'pm', 'py', or \
             'YYYY-MM-DDtoYYYY-MM-DD', got '{freshness}'"
        ));
    }
    Ok(())
}

fn build_search_url(params: &WebSearchParams, count: u32) -> Result<Url, String> {
    let mut url =
        Url::parse(BRAVE_SEARCH_ENDPOINT).map_err(|err| format!("invalid Brave URL: {err}"))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("q", &params.query);
        query.append_pair("count", &count.to_string());
        if let Some(country) = &params.country {
            query.append_pair("country", country);
        }
        if let Some(search_lang) = &params.search_lang {
            query.append_pair("search_lang", search_lang);
        }
        if let Some(ui_lang) = &params.ui_lang {
            query.append_pair("ui_lang", ui_lang);
        }
        if let Some(freshness) = &params.freshness {
            query.append_pair("freshness", freshness);
        }
    }
    Ok(url)
}

fn format_output(query: String, response: BraveSearchResponse) -> WebSearchOutput {
    let results = response
        .web
        .and_then(|web| web.results)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|result| {
            let title = result.title?;
            let url = result.url?;
            Some(SearchResult {
                title,
                site_name: extract_hostname(&url),
                url,
                description: result.description.unwrap_or_default(),
                published: result.age,
                thumbnail: result.thumbnail.and_then(|thumbnail| thumbnail.src),
            })
        })
        .collect::<Vec<_>>();
    WebSearchOutput {
        query,
        result_count: results.len(),
        results,
    }
}

fn extract_hostname(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    url.host_str().map(str::to_string)
}

fn is_valid_lang_code(value: &str) -> bool {
    value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_lowercase())
}

fn is_valid_country_code(value: &str) -> bool {
    value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_uppercase())
}

fn is_valid_ui_lang(value: &str) -> bool {
    let mut parts = value.split('-');
    let (Some(lang), Some(country), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    is_valid_lang_code(lang) && is_valid_country_code(country)
}

fn is_valid_freshness(value: &str) -> bool {
    matches!(value, "pd" | "pw" | "pm" | "py") || is_valid_date_range(value)
}

fn is_valid_date_range(value: &str) -> bool {
    let Some((start, end)) = value.split_once("to") else {
        return false;
    };
    is_date_like(start) && is_date_like(end)
}

fn is_date_like(value: &str) -> bool {
    value.len() == 10
        && value.as_bytes().get(4) == Some(&b'-')
        && value.as_bytes().get(7) == Some(&b'-')
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

#[async_trait]
impl InternalTool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<WebSearchParams>(
            self.name(),
            "Search the web using Brave Search. Returns structured results \
             with titles, URLs, descriptions, publication dates, thumbnails, \
             and site names. Use web_fetch on a result URL when page contents \
             are needed.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        tools::execute_typed(args, |params: WebSearchParams| async {
            WebSearchTool::execute(self, params).await
        })
        .await
        .map(ToolOutput::json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(query: &str) -> WebSearchParams {
        WebSearchParams {
            query: query.to_string(),
            count: None,
            country: None,
            search_lang: None,
            ui_lang: None,
            freshness: None,
        }
    }

    #[test]
    fn validates_search_params() {
        assert!(validate_params(&params("rust")).is_ok());
        assert!(validate_params(&params("")).is_err());

        let invalid_country = WebSearchParams {
            country: Some("us".to_string()),
            ..params("rust")
        };
        assert!(validate_params(&invalid_country).is_err());

        let invalid_lang = WebSearchParams {
            search_lang: Some("EN".to_string()),
            ..params("rust")
        };
        assert!(validate_params(&invalid_lang).is_err());

        let invalid_ui_lang = WebSearchParams {
            ui_lang: Some("en-us".to_string()),
            ..params("rust")
        };
        assert!(validate_params(&invalid_ui_lang).is_err());

        let invalid_freshness = WebSearchParams {
            freshness: Some("forever".to_string()),
            ..params("rust")
        };
        assert!(validate_params(&invalid_freshness).is_err());
    }

    #[test]
    fn builds_search_url_with_filters() {
        let params = WebSearchParams {
            query: "rust programming".to_string(),
            count: Some(10),
            country: Some("US".to_string()),
            search_lang: Some("en".to_string()),
            ui_lang: Some("en-US".to_string()),
            freshness: Some("pw".to_string()),
        };

        let url = build_search_url(&params, 10).unwrap();

        assert_eq!(url.host_str(), Some("api.search.brave.com"));
        assert_eq!(url.path(), "/res/v1/web/search");
        assert!(url.query().unwrap().contains("q=rust+programming"));
        assert!(url.query().unwrap().contains("count=10"));
        assert!(url.query().unwrap().contains("country=US"));
        assert!(url.query().unwrap().contains("search_lang=en"));
        assert!(url.query().unwrap().contains("ui_lang=en-US"));
        assert!(url.query().unwrap().contains("freshness=pw"));
    }

    #[test]
    fn formats_brave_results() {
        let response = BraveSearchResponse {
            web: Some(BraveWebResults {
                results: Some(vec![BraveSearchResult {
                    title: Some("Example".to_string()),
                    url: Some("https://example.com/path".to_string()),
                    description: Some("desc".to_string()),
                    age: Some("2 days ago".to_string()),
                    thumbnail: Some(BraveThumbnail {
                        src: Some("https://example.com/thumb.jpg".to_string()),
                    }),
                }]),
            }),
        };

        let output = format_output("example".to_string(), response);

        assert_eq!(output.result_count, 1);
        assert_eq!(output.results[0].title, "Example");
        assert_eq!(output.results[0].site_name.as_deref(), Some("example.com"));
    }
}

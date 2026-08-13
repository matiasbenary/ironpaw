wit_bindgen::generate!({
    world: "tool",
    path: "wit/tool.wit",
});

use schemars::JsonSchema;
use serde::Deserialize;

const BRAVE_SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const DEFAULT_COUNT: u32 = 5;
const MAX_COUNT: u32 = 20;

struct WebSearch;

impl Guest for WebSearch {
    fn name() -> String {
        "web_search".to_string()
    }

    fn description() -> String {
        "Search the web using Brave Search. Returns structured results with titles, URLs, \
         descriptions, publication dates, thumbnails, and site names. Use web_fetch on a \
         result URL when page contents are needed."
            .to_string()
    }

    fn parameters() -> String {
        serde_json::to_string(&schemars::schema_for!(WebSearchParams))
            .unwrap_or_else(|_| "{\"type\":\"object\"}".to_string())
    }

    fn execute(args_json: String, _config_json: String) -> Result<String, String> {
        execute_inner(&args_json)
    }
}

export!(WebSearch);

#[derive(Deserialize, JsonSchema)]
struct WebSearchParams {
    /// The search query to look up on the web.
    query: String,
    /// Number of results to return (1-20, default 5).
    count: Option<u32>,
    /// 2-letter uppercase country code to bias results, e.g. "US".
    country: Option<String>,
    /// 2-letter lowercase language code for search results, e.g. "en".
    search_lang: Option<String>,
    /// Locale in language-region format, e.g. "en-US".
    ui_lang: Option<String>,
    /// Discovery time filter: "pd", "pw", "pm", "py", or
    /// "YYYY-MM-DDtoYYYY-MM-DD".
    freshness: Option<String>,
}

#[derive(Deserialize)]
struct BraveSearchResponse {
    web: Option<BraveWebResults>,
}

#[derive(Deserialize)]
struct BraveWebResults {
    results: Option<Vec<BraveSearchResult>>,
}

#[derive(Deserialize)]
struct BraveSearchResult {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
    age: Option<String>,
    thumbnail: Option<BraveThumbnail>,
}

#[derive(Deserialize)]
struct BraveThumbnail {
    src: Option<String>,
}

fn execute_inner(args_json: &str) -> Result<String, String> {
    let params: WebSearchParams =
        serde_json::from_str(args_json).map_err(|err| format!("invalid parameters: {err}"))?;
    validate_params(&params)?;

    let count = params.count.unwrap_or(DEFAULT_COUNT).clamp(1, MAX_COUNT);
    let url = build_search_url(&params, count);
    let body = ironpaw::tool::tool_host::http_request(None, "GET", &url, "{}", "")
        .map_err(|err| format!("Brave request failed: {err}"))?;
    let response: BraveSearchResponse = serde_json::from_str(&body)
        .map_err(|err| format!("failed to parse Brave response: {err}"))?;

    let results = response
        .web
        .and_then(|web| web.results)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|result| {
            let title = result.title?;
            let url = result.url?;
            let mut entry = serde_json::json!({
                "title": title,
                "url": url,
                "description": result.description.unwrap_or_default(),
            });
            if let Some(published) = result.age {
                entry["published"] = serde_json::json!(published);
            }
            if let Some(thumbnail) = result.thumbnail.and_then(|thumbnail| thumbnail.src) {
                entry["thumbnail"] = serde_json::json!(thumbnail);
            }
            if let Some(site_name) = extract_hostname(&url) {
                entry["site_name"] = serde_json::json!(site_name);
            }
            Some(entry)
        })
        .collect::<Vec<_>>();

    serde_json::to_string(&serde_json::json!({
        "query": params.query,
        "result_count": results.len(),
        "results": results,
    }))
    .map_err(|err| err.to_string())
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

fn build_search_url(params: &WebSearchParams, count: u32) -> String {
    let count = count.to_string();
    let mut pairs = vec![("q", params.query.as_str()), ("count", count.as_str())];
    if let Some(country) = &params.country {
        pairs.push(("country", country));
    }
    if let Some(search_lang) = &params.search_lang {
        pairs.push(("search_lang", search_lang));
    }
    if let Some(ui_lang) = &params.ui_lang {
        pairs.push(("ui_lang", ui_lang));
    }
    if let Some(freshness) = &params.freshness {
        pairs.push(("freshness", freshness));
    }

    let query = pairs
        .into_iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{BRAVE_SEARCH_ENDPOINT}?{query}")
}

fn percent_encode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn extract_hostname(url: &str) -> Option<String> {
    let without_scheme = url.split_once("://")?.1;
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    authority
        .split('@')
        .next_back()?
        .split(':')
        .next()
        .map(str::to_string)
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

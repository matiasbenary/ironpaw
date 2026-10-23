wit_bindgen::generate!({
    world: "tool",
    path: "wit/tool.wit",
});

use schemars::JsonSchema;
use serde::Deserialize;

// ponytail: mainnet still points at the local OutLayer mock (~/projects/near/outlayer-mock),
// only reachable from a local-development host build. Real mainnet: https://api.outlayer.ai
const MAINNET_BASE: &str = "http://localhost:4400";
const TESTNET_BASE: &str = "https://testnet-api.outlayer.ai";
// Public NEAR testnet faucet listed in docs.near.org; ~5 NEAR per request, rate limited.
const TESTNET_FAUCET: &str = "https://faucet-near-testnet.onrender.com/api/faucet";

struct Outlayer;

impl Guest for Outlayer {
    fn name() -> String {
        "outlayer".to_string()
    }

    fn description() -> String {
        "Call the OutLayer agent-custody wallet API. The user can have several wallets, each \
         with its own name, network (mainnet or testnet) and key injected by the host: pick one \
         with `wallet` (optional when there is only one; the error lists the names). \
         To create a wallet NEVER call /register: call configure_tool with name outlayer and \
         add_account=true (it asks the user for the wallet name and network, creates it and keeps \
         the key private). \
         Any request that is not a read (transfer, call, deposit, swap, withdraw...) is held by the \
         host until the user approves it with buttons: the call then fails with APPROVAL_REQUIRED; \
         end your turn, and after they approve repeat the identical call once. Never claim funds \
         moved before the call succeeds. \
         Testnet has no NEAR Intents solvers: every /wallet/v1/intents/* call is rejected \
         there, so for a swap or intents request on testnet do NOT wrap or deposit \
         anything first; just tell the user it only works on mainnet. Testnet contract \
         ids end in .testnet (wrap.testnet, not wrap.near). To fund the testnet \
         wallet call method POST, path /faucet on a testnet wallet (no body; it funds this \
         wallet only, ~5 NEAR, rate limited). \
         Full spec: GET /openapi.json.\n\
         Key endpoints: GET /wallet/v1/balance?chain=near (gas balance) | \
         ?chain=near&token=<ft> | ?token=<ft>&source=intents (what swaps spend); \
         GET /wallet/v1/address?chain=near|solana|ethereum; GET /wallet/v1/tokens; \
         POST /wallet/v1/transfer {chain:\"near\",to,amount}; \
         POST /wallet/v1/call {receiver_id,method_name,args,gas,deposit}; \
         POST /wallet/v1/storage-deposit {token}; POST /wallet/v1/intents/deposit {token,amount}; \
         POST /wallet/v1/intents/swap/quote and /intents/swap with EXACTLY \
         {token_in,token_out,amount_in,min_amount_out?} (no slippage field exists); \
         POST /wallet/v1/intents/withdraw {chain,to,amount,token,async}; \
         GET /wallet/v1/requests/{id}; GET /wallet/v1/audit.\n\
         Rules: amounts are strings in minimal units (NEAR 24 decimals, USDC/USDT 6, ETH 18, \
         BTC 8, SOL 9); a NEAR amount >=0.01 has 22+ digits. Convert results the same \
         way: balance \"5997125465934581600000000\" = 5.997 NEAR (divide by 1e24). \
         In /call, args is a JSON object, not a string. \"No liquidity available\" = \
         mis-scaled amount or below ~$1. Native NEAR -> intents is three calls: \
         storage-deposit on wrap.near, /call near_deposit on wrap.near with deposit, then \
         /intents/deposit token wrap.near. Keep ~0.05 NEAR for gas. Check the right balance \
         before acting and quote right before swapping. Status `processing` is not a result: \
         poll /requests/{id} until success|failed|refunded|rejected|needs_review; never \
         retry needs_review. Never pick a token by symbol alone, match chain + \
         defuse_asset_id. If the wallet lacks funds, give the user: \
         https://app.outlayer.ai/wallet/fund?to={near_account_id}&amount={n}&token={t}&msg={why} \
         (add &dest=intents for swap money). Never ask for keys or seed phrases in chat."
            .to_string()
    }

    fn parameters() -> String {
        serde_json::to_string(&schemars::schema_for!(OutlayerParams))
            .unwrap_or_else(|_| "{\"type\":\"object\"}".to_string())
    }

    fn execute(args_json: String, config_json: String) -> Result<String, String> {
        let params: OutlayerParams =
            serde_json::from_str(&args_json).map_err(|err| format!("invalid parameters: {err}"))?;
        let method = params.method.to_ascii_uppercase();
        validate(&method, &params.path)?;
        let (wallet, network) = pick_wallet(&config_json, params.wallet.as_deref())?;
        let base = match network.as_str() {
            "mainnet" => MAINNET_BASE,
            "testnet" => TESTNET_BASE,
            other => return Err(format!("wallet '{wallet}' has unknown network '{other}'")),
        };
        if base == TESTNET_BASE && params.path == "/faucet" {
            return testnet_faucet(&wallet);
        }
        if base == TESTNET_BASE && params.path.starts_with("/wallet/v1/intents") {
            return Err(
                "NEAR Intents (swaps, intents deposits/balances/withdrawals) are \
                        mainnet-only: testnet has no solvers. Nothing was sent."
                    .to_string(),
            );
        }
        let body = match params.body.map(normalize_body) {
            Some(body) if method != "GET" => body.to_string(),
            _ => String::new(),
        };
        let url = format!("{base}{}", params.path);
        ironpaw::tool::tool_host::http_request(Some(&wallet), &method, &url, "{}", &body)
            .map_err(|err| format!("OutLayer request failed: {err}"))
    }
}

export!(Outlayer);

/// Models often JSON-encode nested values; decode `body` and `body.args` back into
/// objects so a contract call doesn't receive its arguments as a string.
fn normalize_body(body: serde_json::Value) -> serde_json::Value {
    let mut body = decode_json_string(body);
    if let Some(args) = body.get_mut("args") {
        *args = decode_json_string(args.take());
    }
    body
}

fn decode_json_string(value: serde_json::Value) -> serde_json::Value {
    match &value {
        serde_json::Value::String(text) => match serde_json::from_str(text) {
            Ok(decoded @ serde_json::Value::Object(_)) => decoded,
            _ => value,
        },
        _ => value,
    }
}

/// Funds this wallet's own testnet account, so the faucet can't be pointed elsewhere.
fn testnet_faucet(wallet: &str) -> Result<String, String> {
    let address = ironpaw::tool::tool_host::http_request(
        Some(wallet),
        "GET",
        &format!("{TESTNET_BASE}/wallet/v1/address?chain=near"),
        "{}",
        "",
    )
    .map_err(|err| format!("OutLayer request failed: {err}"))?;
    let address: serde_json::Value =
        serde_json::from_str(&address).map_err(|err| format!("invalid address response: {err}"))?;
    let account = address["address"]
        .as_str()
        .ok_or("address response has no 'address'")?;
    ironpaw::tool::tool_host::http_request(
        Some(wallet),
        "POST",
        TESTNET_FAUCET,
        "{}",
        &serde_json::json!({ "accountId": account }).to_string(),
    )
    .map_err(|err| format!("faucet request failed: {err}"))
}

#[derive(Deserialize, JsonSchema)]
struct OutlayerParams {
    /// HTTP method: GET, POST, PATCH or DELETE.
    method: String,
    /// API path including any query string, e.g. "/wallet/v1/balance?chain=near".
    path: String,
    /// JSON request body for POST/PATCH/DELETE. Ignored for GET.
    body: Option<serde_json::Value>,
    /// Name of the wallet to use. Optional when only one wallet is configured.
    wallet: Option<String>,
}

/// Returns (wallet name, network) from the host-written account list.
fn pick_wallet(config_json: &str, requested: Option<&str>) -> Result<(String, String), String> {
    let config: serde_json::Value =
        serde_json::from_str(config_json).map_err(|err| format!("invalid tool config: {err}"))?;
    let accounts = config["outlayer"]["accounts"]
        .as_object()
        .filter(|accounts| !accounts.is_empty())
        .ok_or("no OutLayer wallet is configured; ask the user to set one up with setup_tool")?;
    let names = accounts.keys().cloned().collect::<Vec<_>>().join(", ");
    let name = match requested {
        Some(name) => name,
        None if accounts.len() == 1 => accounts.keys().next().expect("one account"),
        None => return Err(format!("several wallets are configured, pass `wallet`: {names}")),
    };
    let account = accounts
        .get(name)
        .ok_or_else(|| format!("unknown wallet '{name}', available: {names}"))?;
    let network = account["network"].as_str().unwrap_or("mainnet");
    Ok((name.to_string(), network.to_string()))
}

fn validate(method: &str, path: &str) -> Result<(), String> {
    if !matches!(method, "GET" | "POST" | "PATCH" | "DELETE") {
        return Err(format!("unsupported method '{method}'"));
    }
    // The path is appended to a fixed host; reject anything that could change the authority
    // or walk out of the API (the bearer key is attached by host, so this keeps it on OutLayer).
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("..")
        || path.contains('@')
        || path.contains('\\')
        || path.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("invalid 'path': '{path}'"));
    }
    // A wallet made here would come back with no name, and its key would be shown to the model.
    if path == "/register" || path.starts_with("/register?") {
        return Err("Wallets are not created through this tool. Call configure_tool with \
                    name outlayer and add_account=true: it asks the user for a name and a network, \
                    creates the wallet and keeps its key private. Nothing was sent."
            .to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{pick_wallet, validate};

    #[test]
    fn wallet_selection() {
        let one = r#"{"outlayer":{"accounts":{"main":{"network":"mainnet"}}}}"#;
        assert_eq!(pick_wallet(one, None).unwrap(), ("main".into(), "mainnet".into()));
        let two = r#"{"outlayer":{"accounts":{"a":{"network":"mainnet"},"b":{"network":"testnet"}}}}"#;
        assert!(pick_wallet(two, None).unwrap_err().contains("a, b"));
        assert_eq!(pick_wallet(two, Some("b")).unwrap().1, "testnet");
        assert!(pick_wallet(two, Some("c")).is_err());
        assert!(pick_wallet(r#"{"outlayer":{"accounts":{}}}"#, None).is_err());
    }

    #[test]
    fn path_validation() {
        assert!(validate("GET", "/wallet/v1/balance?chain=near").is_ok());
        assert!(validate("POST", "/wallet/v1/intents/swap").is_ok());
        for bad in [
            "wallet",
            "//evil.com/x",
            "/../x",
            "/x@evil.com",
            "/a b",
            "/a\\b",
        ] {
            assert!(validate("GET", bad).is_err(), "{bad}");
        }
        assert!(validate("PUT", "/x").is_err());
        for register in ["/register", "/register?x=1"] {
            let error = validate("POST", register).unwrap_err();
            assert!(error.contains("configure_tool"), "{error}");
        }
        assert!(validate("GET", "/registered-things").is_ok());
    }

    #[test]
    fn nested_json_strings_are_decoded() {
        let body = super::normalize_body(serde_json::json!(
            r#"{"method_name":"near_withdraw","args":"{\"amount\":\"1\"}"}"#
        ));
        assert_eq!(body["args"]["amount"], "1");
        // A plain string arg stays a string.
        let body = super::normalize_body(serde_json::json!({"args": "hello"}));
        assert_eq!(body["args"], "hello");
    }
}

use std::{collections::BTreeMap, time::Duration};

use crate::network::policy::resolve_public;

const ISPDB: &str = "https://autoconfig.thunderbird.net/v1.1/";

#[derive(Clone, Debug, PartialEq)]
struct EmailSettings {
    username: String,
    imap_host: String,
    imap_port: u16,
    imap_tls: &'static str,
    smtp_host: String,
    smtp_port: u16,
    smtp_tls: &'static str,
}

pub(super) async fn discover(address: &str) -> Result<BTreeMap<String, serde_json::Value>, String> {
    let (_, domain) = email_parts(address)?;
    if let Some(provider) = oauth_provider(domain) {
        return Ok(BTreeMap::from([
            ("/email/provider".to_string(), provider.into()),
            ("/email/username".to_string(), address.trim().into()),
        ]));
    }
    let settings = if let Some(settings) = known_provider(address, domain) {
        settings
    } else if let Ok(settings) = retrieve_from_ispdb(address, domain).await {
        settings
    } else {
        guess_from_dns(address, domain).await?
    };

    Ok(BTreeMap::from([
        ("/email/provider".to_string(), "imap".into()),
        ("/email/username".to_string(), settings.username.into()),
        ("/email/from".to_string(), address.trim().into()),
        ("/email/imap/host".to_string(), settings.imap_host.into()),
        ("/email/imap/port".to_string(), settings.imap_port.into()),
        ("/email/imap/tls".to_string(), settings.imap_tls.into()),
        ("/email/smtp/host".to_string(), settings.smtp_host.into()),
        ("/email/smtp/port".to_string(), settings.smtp_port.into()),
        ("/email/smtp/tls".to_string(), settings.smtp_tls.into()),
    ]))
}

fn oauth_provider(domain: &str) -> Option<&'static str> {
    match domain.to_ascii_lowercase().as_str() {
        "gmail.com" | "googlemail.com" => Some("gmail"),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" => Some("outlook"),
        _ => None,
    }
}

fn email_parts(address: &str) -> Result<(&str, &str), String> {
    let address = address.trim();
    let (local, domain) = address
        .split_once('@')
        .ok_or_else(|| "enter a complete email address".to_string())?;
    if local.is_empty()
        || domain.is_empty()
        || domain.contains('@')
        || !domain.split('.').all(|label| {
            !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    {
        return Err("enter a valid email address".to_string());
    }
    Ok((local, domain))
}

pub(super) fn is_email_address(value: &str) -> bool {
    email_parts(value).is_ok()
}

fn known_provider(address: &str, domain: &str) -> Option<EmailSettings> {
    let (imap_host, smtp_host, smtp_port, smtp_tls) = match domain.to_ascii_lowercase().as_str() {
        "yahoo.com" | "ymail.com" | "rocketmail.com" => {
            ("imap.mail.yahoo.com", "smtp.mail.yahoo.com", 465, "tls")
        }
        "icloud.com" | "me.com" | "mac.com" => {
            ("imap.mail.me.com", "smtp.mail.me.com", 587, "start_tls")
        }
        "fastmail.com" | "fastmail.fm" => ("imap.fastmail.com", "smtp.fastmail.com", 465, "tls"),
        "zoho.com" => ("imap.zoho.com", "smtp.zoho.com", 465, "tls"),
        "gmx.com" | "gmx.us" => ("imap.gmx.com", "mail.gmx.com", 465, "tls"),
        _ => return None,
    };
    Some(EmailSettings {
        username: address.trim().to_string(),
        imap_host: imap_host.to_string(),
        imap_port: 993,
        imap_tls: "tls",
        smtp_host: smtp_host.to_string(),
        smtp_port,
        smtp_tls,
    })
}

async fn retrieve_from_ispdb(address: &str, domain: &str) -> Result<EmailSettings, String> {
    let mut url = reqwest::Url::parse(ISPDB).expect("valid ISPDB URL");
    url.path_segments_mut()
        .expect("ISPDB URL supports path segments")
        .push(domain);
    let addresses = resolve_public(url.host_str().unwrap_or_default(), 443).await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(url.host_str().unwrap_or_default(), &addresses)
        .build()
        .map_err(|err| err.to_string())?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| format!("mail settings lookup failed: {}", err.without_url()))?
        .error_for_status()
        .map_err(|err| format!("mail settings lookup failed: {}", err.without_url()))?;
    let xml = response
        .text()
        .await
        .map_err(|err| format!("mail settings lookup failed: {}", err.without_url()))?;
    parse_ispdb(&xml, address)
}

async fn guess_from_dns(address: &str, domain: &str) -> Result<EmailSettings, String> {
    for (imap_host, smtp_host) in [
        (format!("imap.{domain}"), format!("smtp.{domain}")),
        (format!("mail.{domain}"), format!("mail.{domain}")),
    ] {
        let (imap, smtp) = tokio::join!(
            resolve_public(&imap_host, 993),
            resolve_public(&smtp_host, 465)
        );
        if imap.is_ok() && smtp.is_ok() {
            return Ok(EmailSettings {
                username: address.trim().to_string(),
                imap_host,
                imap_port: 993,
                imap_tls: "tls",
                smtp_host,
                smtp_port: 465,
                smtp_tls: "tls",
            });
        }
    }
    Err("could not detect mail servers; enter them manually".to_string())
}

fn parse_ispdb(xml: &str, address: &str) -> Result<EmailSettings, String> {
    let (local, domain) = email_parts(address)?;
    let incoming = server_block(xml, "incomingServer", "imap")?;
    let outgoing = server_block(xml, "outgoingServer", "smtp")?;
    let username = expand_username(tag(incoming, "username")?, address, local, domain);
    Ok(EmailSettings {
        username,
        imap_host: tag(incoming, "hostname")?.to_string(),
        imap_port: parse_port(incoming)?,
        imap_tls: parse_tls(incoming)?,
        smtp_host: tag(outgoing, "hostname")?.to_string(),
        smtp_port: parse_port(outgoing)?,
        smtp_tls: parse_tls(outgoing)?,
    })
}

fn server_block<'a>(xml: &'a str, element: &str, kind: &str) -> Result<&'a str, String> {
    let starts = [
        format!("<{element} type=\"{kind}\""),
        format!("<{element} type='{kind}'"),
    ];
    let start = starts
        .iter()
        .find_map(|needle| xml.find(needle))
        .ok_or_else(|| format!("autoconfiguration has no {kind} server"))?;
    let end_tag = format!("</{element}>");
    let end = xml[start..]
        .find(&end_tag)
        .map(|offset| start + offset)
        .ok_or_else(|| format!("autoconfiguration has an invalid {kind} server"))?;
    Ok(&xml[start..end])
}

fn tag<'a>(block: &'a str, name: &str) -> Result<&'a str, String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = block
        .find(&open)
        .map(|index| index + open.len())
        .ok_or_else(|| format!("autoconfiguration is missing {name}"))?;
    let end = block[start..]
        .find(&close)
        .map(|index| start + index)
        .ok_or_else(|| format!("autoconfiguration has invalid {name}"))?;
    Ok(block[start..end].trim())
}

fn parse_port(block: &str) -> Result<u16, String> {
    tag(block, "port")?
        .parse()
        .map_err(|_| "autoconfiguration has an invalid port".to_string())
}

fn parse_tls(block: &str) -> Result<&'static str, String> {
    match tag(block, "socketType")?.to_ascii_uppercase().as_str() {
        "SSL" | "SSL/TLS" => Ok("tls"),
        "STARTTLS" => Ok("start_tls"),
        _ => Err("autoconfiguration requires unsupported unencrypted email".to_string()),
    }
}

fn expand_username(template: &str, address: &str, local: &str, domain: &str) -> String {
    template
        .replace("%EMAILADDRESS%", address.trim())
        .replace("%EMAILLOCALPART%", local)
        .replace("%EMAILDOMAIN%", domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detects_oauth_provider_from_address() {
        let gmail = discover("person@gmail.com").await.unwrap();
        let hotmail = discover("person@hotmail.com").await.unwrap();

        assert_eq!(gmail["/email/provider"], "gmail");
        assert_eq!(hotmail["/email/provider"], "outlook");
    }

    #[tokio::test]
    async fn discovers_yahoo_imap_settings() {
        let values = discover("person@yahoo.com").await.unwrap();
        assert_eq!(values["/email/provider"], "imap");
        assert_eq!(values["/email/imap/host"], "imap.mail.yahoo.com");
        assert_eq!(values["/email/smtp/host"], "smtp.mail.yahoo.com");
    }

    #[tokio::test]
    async fn discovers_icloud_imap_settings() {
        let values = discover("person@icloud.com").await.unwrap();
        assert_eq!(values["/email/provider"], "imap");
        assert_eq!(values["/email/imap/host"], "imap.mail.me.com");
        assert_eq!(values["/email/smtp/port"], 587);
    }

    #[test]
    fn parses_thunderbird_autoconfiguration() {
        let xml = r#"
            <clientConfig>
              <emailProvider>
                <incomingServer type="imap">
                  <hostname>imap.example.com</hostname>
                  <port>993</port><socketType>SSL</socketType>
                  <username>%EMAILADDRESS%</username>
                </incomingServer>
                <outgoingServer type="smtp">
                  <hostname>smtp.example.com</hostname>
                  <port>587</port><socketType>STARTTLS</socketType>
                </outgoingServer>
              </emailProvider>
            </clientConfig>"#;

        let settings = parse_ispdb(xml, "me@example.com").unwrap();

        assert_eq!(settings.username, "me@example.com");
        assert_eq!(settings.imap_host, "imap.example.com");
        assert_eq!(settings.smtp_host, "smtp.example.com");
        assert_eq!(settings.smtp_tls, "start_tls");
    }

    #[test]
    fn rejects_incomplete_email_address() {
        assert_eq!(
            email_parts("example.com").unwrap_err(),
            "enter a complete email address"
        );
    }
}

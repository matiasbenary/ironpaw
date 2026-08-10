use axum::http::{HeaderMap, StatusCode, header};

use crate::{
    channels::config::{ChannelAuth, ChannelConfig},
    secrets::ChannelSecrets,
};

pub(super) fn authorize(
    headers: &HeaderMap,
    token: &str,
    secrets: &ChannelSecrets,
    config: &ChannelConfig,
) -> Result<(), (StatusCode, String)> {
    match config.auth {
        ChannelAuth::None => Ok(()),
        ChannelAuth::Bearer => authorize_bearer(headers, token),
        ChannelAuth::HeaderSecret {
            ref header,
            ref value,
            ref secret,
        } => {
            let expected_value = match (secret.as_deref(), value.as_deref()) {
                (Some(secret), _) => secrets.get(secret).ok_or_else(|| {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!("missing secret: {secret}"),
                    )
                })?,
                (None, Some(value)) => value.to_string(),
                (None, None) => {
                    return Err((
                        StatusCode::SERVICE_UNAVAILABLE,
                        "header secret auth is not configured".to_string(),
                    ));
                }
            };
            authorize_header_secret(headers, header, &expected_value)
        }
    }
}

pub(super) fn authorize_bearer(
    headers: &HeaderMap,
    token: &str,
) -> Result<(), (StatusCode, String)> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err((
            StatusCode::UNAUTHORIZED,
            "missing Authorization header".to_string(),
        ));
    };
    let value = value.to_str().map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            "invalid Authorization header".to_string(),
        )
    })?;

    if value == format!("Bearer {token}") {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "invalid bearer token".to_string()))
    }
}

fn authorize_header_secret(
    headers: &HeaderMap,
    header_name: &str,
    expected_value: &str,
) -> Result<(), (StatusCode, String)> {
    let Some(value) = headers.get(header_name) else {
        return Err((
            StatusCode::UNAUTHORIZED,
            format!("missing {header_name} header"),
        ));
    };
    let value = value.to_str().map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            format!("invalid {header_name} header"),
        )
    })?;

    if value == expected_value {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            format!("invalid {header_name} header"),
        ))
    }
}

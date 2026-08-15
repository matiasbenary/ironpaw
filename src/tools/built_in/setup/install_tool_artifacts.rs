use std::collections::BTreeMap;

use crate::{
    config::RuntimeLayout,
    tools::{
        setup::install_common::{apply_config_values, set_json_pointer, write_package_artifacts},
        wasm::{WasmToolConfig, WasmToolManifest},
    },
};

use super::install_tool_package::{
    ToolPackage, account_identity, installed_tool_manifest, scoped_secret,
};

pub(super) fn install_package(
    layout: &RuntimeLayout,
    package: &ToolPackage,
    config_values: &BTreeMap<String, serde_json::Value>,
) -> Result<WasmToolManifest, String> {
    let (wasm_dst, config_dst, config_json) = write_package_artifacts(
        "tool",
        &layout.tools_dir(),
        &package.name,
        &package.wasm_path,
        &package.config_template_path,
        config_values,
        true,
    )?;
    let (config_json, parsed) = bind_oauth(package, config_values, &config_json)?;
    std::fs::write(&config_dst, &config_json).map_err(|err| err.to_string())?;
    Ok(WasmToolManifest {
        name: package.name.clone(),
        wasm_path: wasm_dst,
        config_path: config_dst,
        config_json,
        config: parsed,
    })
}

pub(super) fn reconfigure_package(
    layout: &RuntimeLayout,
    package: &ToolPackage,
    config_values: &BTreeMap<String, serde_json::Value>,
) -> Result<WasmToolManifest, String> {
    let Some(mut manifest) = installed_tool_manifest(layout, &package.name)? else {
        return Err(format!("tool '{}' is not installed", package.name));
    };
    let mut config: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&package.config_template_path).map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())?;
    apply_config_values(&mut config, config_values)?;
    (manifest.config_json, manifest.config) = bind_oauth(
        package,
        config_values,
        &serde_json::to_string(&config).map_err(|err| err.to_string())?,
    )?;
    std::fs::write(&manifest.config_path, &manifest.config_json).map_err(|err| err.to_string())?;
    Ok(manifest)
}

pub(super) fn write_account_package(
    layout: &RuntimeLayout,
    package: &ToolPackage,
    accounts: &BTreeMap<String, serde_json::Value>,
    grants: &[crate::network::http::NetworkGrantConfig],
    installed: bool,
) -> Result<WasmToolManifest, String> {
    let mut config: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&package.config_template_path).map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())?;
    let setup = package.manifest.accounts.as_ref().expect("account package");
    set_json_pointer(
        &mut config,
        &setup.path,
        serde_json::to_value(accounts).map_err(|err| err.to_string())?,
    )?;
    config["http_credentials"] = serde_json::to_value(grants).map_err(|err| err.to_string())?;
    let config_json = serde_json::to_string_pretty(&config).map_err(|err| err.to_string())?;
    let tools_dir = layout.tools_dir();
    std::fs::create_dir_all(&tools_dir).map_err(|err| err.to_string())?;
    let wasm_path = tools_dir.join(format!("{}.wasm", package.name));
    let config_path = tools_dir.join(format!("{}.config.json", package.name));
    if !installed {
        std::fs::copy(&package.wasm_path, &wasm_path).map_err(|err| err.to_string())?;
    }
    std::fs::write(&config_path, &config_json).map_err(|err| err.to_string())?;
    Ok(WasmToolManifest {
        name: package.name.clone(),
        wasm_path,
        config_path,
        config: serde_json::from_value(config).map_err(|err| err.to_string())?,
        config_json,
    })
}

pub(super) fn build_account(
    package: &ToolPackage,
    values: &BTreeMap<String, serde_json::Value>,
) -> Result<
    (
        String,
        serde_json::Value,
        Vec<crate::network::http::NetworkGrantConfig>,
    ),
    String,
> {
    let setup = package.manifest.accounts.as_ref().expect("account package");
    let identity = account_identity(package, values)?;
    let mut account = setup.template.clone();
    apply_config_values(&mut account, values)?;
    let mut grants = Vec::new();
    for config in &package.manifest.config {
        if config
            .when
            .as_ref()
            .is_some_and(|condition| values.get(&condition.path) != Some(&condition.equals))
        {
            continue;
        }
        let Some(selected) = values.get(&config.path).and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(oauth) = config
            .choices
            .iter()
            .find(|choice| choice.value == selected)
            .and_then(|choice| choice.oauth.as_ref())
        else {
            continue;
        };
        grants.push(crate::network::http::NetworkGrantConfig {
            host: oauth.host.clone(),
            account: Some(identity.clone()),
            secret: None,
            placeholder: None,
            header: None,
            prefix: None,
            oauth: Some(crate::network::oauth::OAuthGrantConfig {
                token_url: oauth.token_url.clone(),
                client_id: oauth.client_id.resolve(values)?,
                client_secret: oauth.client_secret.clone(),
                client_secret_handle: oauth
                    .client_secret_handle
                    .as_ref()
                    .map(|handle| scoped_secret(&identity, handle)),
                refresh_token_handle: scoped_secret(&identity, &oauth.refresh_token_handle),
            }),
        });
    }
    for secret in &package.manifest.secrets {
        if let Some(path) = &secret.path {
            set_json_pointer(
                &mut account,
                path,
                scoped_secret(&identity, &secret.name).into(),
            )?;
        }
    }
    Ok((identity, account, grants))
}

fn bind_oauth(
    package: &ToolPackage,
    values: &BTreeMap<String, serde_json::Value>,
    config_json: &str,
) -> Result<(String, WasmToolConfig), String> {
    let selected = package
        .manifest
        .config
        .iter()
        .filter_map(|config| {
            if config
                .when
                .as_ref()
                .is_some_and(|condition| values.get(&condition.path) != Some(&condition.equals))
            {
                return None;
            }
            let value = values.get(&config.path)?.as_str()?;
            config
                .choices
                .iter()
                .find(|choice| choice.value == value)?
                .oauth
                .as_ref()
        })
        .collect::<Vec<_>>();
    let mut config: serde_json::Value =
        serde_json::from_str(config_json).map_err(|err| err.to_string())?;
    if let Some(grants) = config["http_credentials"].as_array_mut() {
        let mut error = None;
        grants.retain_mut(|grant| {
            let Some(handle) = grant["oauth"]["refresh_token_handle"].as_str() else {
                return true;
            };
            let Some(oauth) = selected
                .iter()
                .find(|oauth| oauth.refresh_token_handle == handle)
            else {
                return false;
            };
            match oauth.client_id.resolve(values) {
                Ok(client_id) => {
                    grant["oauth"] =
                        serde_json::to_value(crate::network::oauth::OAuthGrantConfig {
                            token_url: oauth.token_url.clone(),
                            client_id,
                            client_secret: oauth.client_secret.clone(),
                            client_secret_handle: oauth.client_secret_handle.clone(),
                            refresh_token_handle: oauth.refresh_token_handle.clone(),
                        })
                        .expect("OAuth grant serialization");
                    true
                }
                Err(message) => {
                    error = Some(message);
                    false
                }
            }
        });
        if let Some(error) = error {
            return Err(error);
        }
    }
    let config_json = serde_json::to_string_pretty(&config).map_err(|err| err.to_string())?;
    let parsed = serde_json::from_value(config).map_err(|err| err.to_string())?;
    Ok((config_json, parsed))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::tools::built_in::setup::install_tool::install_tool_package::ToolInstallManifest;

    #[test]
    fn selected_oauth_recipe_becomes_the_only_runtime_grant() {
        let manifest: ToolInstallManifest = serde_json::from_str(include_str!(
            "../../../../wasm-tools/email/email.install.json"
        ))
        .unwrap();
        let package = ToolPackage {
            name: "email".to_string(),
            display_name: "Email".to_string(),
            wasm_path: PathBuf::new(),
            config_template_path: PathBuf::new(),
            manifest,
        };
        let values = BTreeMap::from([
            ("/oauth/access".to_string(), serde_json::json!("readonly")),
            ("/provider".to_string(), serde_json::json!("gmail")),
            ("/username".to_string(), serde_json::json!("me@gmail.com")),
        ]);

        let (identity, account, grants) = build_account(&package, &values).unwrap();

        assert_eq!(identity, "me@gmail.com");
        assert_eq!(account["username"], "me@gmail.com");
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].account.as_deref(), Some("me@gmail.com"));
        assert_eq!(grants[0].host, "gmail.googleapis.com");
        assert_eq!(
            grants[0].oauth.as_ref().unwrap().client_id,
            "REMOVED_GOOGLE_OAUTH_CLIENT_ID"
        );
        assert_eq!(
            grants[0].oauth.as_ref().unwrap().client_secret.as_deref(),
            Some("REMOVED_GOOGLE_OAUTH_CLIENT_SECRET")
        );
    }
}

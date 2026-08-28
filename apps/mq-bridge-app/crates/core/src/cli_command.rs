//! Helpers for turning configuration into a copyable POSIX shell command.

use anyhow::Context;

use crate::config::AppConfig;

#[derive(Debug, PartialEq, Eq)]
pub struct InlineConfigCommand {
    pub command: String,
    pub required_env: Vec<String>,
}

/// Quote one argument for a POSIX-compatible shell.
pub fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Build a single-line headless command without embedding credential values.
pub fn inline_config_command(config: &AppConfig) -> anyhow::Result<InlineConfigCommand> {
    let mut config = config.clone();
    let extracted = config.extract_secrets();
    let mut required_env: Vec<String> = config
        .env_vars
        .keys()
        .chain(extracted.keys())
        .cloned()
        .collect();
    required_env.sort();
    required_env.dedup();
    config.env_vars.clear();

    let json =
        serde_json::to_string(&config).context("failed to serialize inline configuration")?;
    Ok(InlineConfigCommand {
        command: format!(
            "mqb --config-str {} --no-ui --no-metrics",
            shell_quote(&json)
        ),
        required_env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_shell_quotes_single_quotes() {
        assert_eq!(shell_quote("it's valid"), "'it'\"'\"'s valid'");
    }

    #[test]
    fn command_does_not_embed_publisher_header_credentials() {
        let config: AppConfig = serde_yaml_ng::from_str(
            r#"
publishers:
  - id: "pub-1"
    name: "orders"
    endpoint:
      http:
        url: "https://example.test/orders"
    headers:
      - key: "Authorization"
        value: "Bearer super-secret"
      - key: "Content-Type"
        value: "application/json"
"#,
        )
        .unwrap();

        let export = inline_config_command(&config).unwrap();

        assert!(!export.command.contains("super-secret"));
        // Non-credential headers stay inline; only the secret one moves to the env.
        assert!(export.command.contains("application/json"));
        assert!(
            export
                .required_env
                .contains(&"MQB__PUBLISHERS__ORDERS__HEADERS__AUTHORIZATION".to_string())
        );
        assert!(
            export
                .required_env
                .contains(&"MQB__PUBLISHERS__PUB_1__HEADERS__AUTHORIZATION".to_string())
        );
    }

    #[test]
    fn command_does_not_embed_inline_environment_values() {
        let mut config = AppConfig::default();
        config
            .env_vars
            .insert("MQB_PASSWORD".to_string(), "clear text".to_string());

        let export = inline_config_command(&config).unwrap();

        assert!(!export.command.contains("clear text"));
        assert_eq!(export.required_env, ["MQB_PASSWORD"]);
    }
}

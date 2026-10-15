//! Configuration of `lwprint serve`, read from a TOML file (default /etc/lwprint.toml).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub printer: PrinterConfig,
    #[serde(default)]
    pub http: HttpConfig,
    /// MQTT is disabled when this section is missing.
    pub mqtt: Option<MqttConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrinterConfig {
    /// Bluetooth address, e.g. "44:B7:D0:64:FC:0E".
    pub address: String,
    /// RFCOMM channel; looked up automatically when missing.
    pub channel: Option<u8>,
    /// Font family or font file for text.
    pub font: Option<String>,
    /// How often to check the printer status while idle (0 = only after jobs and on request).
    #[serde(default = "default_status_interval")]
    pub status_interval_secs: u64,
    /// How long a job waits for the printer (switched off, no tape, ...) before it fails.
    #[serde(default = "default_retry_minutes")]
    pub retry_minutes: u64,
    /// How long to wait for more labels before printing, so that labels sent in quick
    /// succession share one printer job (and its blank lead). 0 prints immediately.
    #[serde(default = "default_batch_window")]
    pub batch_window_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// If set, API requests must send `Authorization: Bearer <token>`.
    pub token: Option<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self { listen: default_listen(), token: None }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_base_topic")]
    pub base_topic: String,
    /// Home Assistant discovery prefix; set to "" to disable discovery.
    #[serde(default = "default_discovery_prefix")]
    pub discovery_prefix: String,
}

fn default_status_interval() -> u64 {
    300
}
fn default_retry_minutes() -> u64 {
    10
}
fn default_batch_window() -> u64 {
    1000
}
fn default_listen() -> String {
    "0.0.0.0:8080".into()
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_base_topic() -> String {
    "lwprint".into()
}
fn default_discovery_prefix() -> String {
    "homeassistant".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read config file {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid config file {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../packaging/lwprint.toml.example");
        let c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.printer.address, "44:B7:D0:64:FC:0E");
        assert_eq!(c.http.listen, "0.0.0.0:8080");
        assert_eq!(c.mqtt.unwrap().base_topic, "lwprint");
    }

    #[test]
    fn minimal_config() {
        let c: Config = toml::from_str("[printer]\naddress = \"44:B7:D0:64:FC:0E\"\n").unwrap();
        assert!(c.mqtt.is_none());
        assert_eq!(c.printer.status_interval_secs, 300);
    }
}

use crate::parse::{parse, Config};

/// Settings for the `[server]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub workers: u16,
}

impl ServerSettings {
    pub fn from_config(cfg: &Config) -> Result<Self, String> {
        Ok(Self {
            host: cfg.require("server.host")?.to_string(),
            port: cfg.get_u16("server.port")?,
            tls: cfg.get_bool("server.tls")?,
            workers: cfg.get_u16_or("server.workers", 4)?,
        })
    }

    /// Parse `text` and extract the server settings in one step.
    pub fn load(text: &str) -> Result<Self, String> {
        let cfg = parse(text)?;
        Self::from_config(&cfg)
    }
}

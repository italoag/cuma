//! Layered, declarative configuration.
//!
//! Four layers, lowest precedence first:
//!
//! 1. Built-in defaults (this file)
//! 2. Global config — `~/.config/cuma/config.toml`
//! 3. Project config — `./.cuma/config.toml`
//! 4. Environment variables — `CUMA_*`
//! 5. CLI flags — applied by the caller after [`Config::load`]
//!
//! Later layers override earlier ones *field by field*, not file by file: a
//! project that sets only `router.strategy` keeps every global weight. Whole
//! files replacing each other is the classic configuration footgun and is
//! specifically what the merge in [`Config::merge`] avoids.

mod env;
mod merge;
mod model;

pub use model::{
    AgentConfig, Config, LimitsConfig, MemoryConfig, RouterConfig, RouterWeights, RoutingStrategy,
    RtkMode, SecurityConfig, SkillAutoInstall, SkillsConfig, TelemetryConfig,
};

use cuma_core::error::{MetaAgentError, Result};
use std::path::{Path, PathBuf};

/// Where a configuration value came from. Surfaced by `cuma doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Compiled-in default.
    Default,
    /// A file on disk.
    File(PathBuf),
    /// An environment variable.
    Environment,
}

/// A loaded configuration plus the layers that produced it.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// The merged result.
    pub config: Config,
    /// Which layers contributed, in application order.
    pub sources: Vec<ConfigSource>,
}

impl Config {
    /// Load and merge every layer.
    ///
    /// A missing config file is not an error — the harness must run with no
    /// configuration at all. A *malformed* one is, because silently ignoring
    /// a typo'd config is how operators end up debugging the wrong system.
    pub fn load(project_root: &Path) -> Result<LoadedConfig> {
        let mut config = Config::default();
        let mut sources = vec![ConfigSource::Default];

        if let Some(global) = global_config_path()
            && global.exists()
        {
            let layer = Self::from_file(&global)?;
            config.merge(layer);
            sources.push(ConfigSource::File(global));
        }

        let project = project_root.join(".cuma").join("config.toml");
        if project.exists() {
            let layer = Self::from_file(&project)?;
            config.merge(layer);
            sources.push(ConfigSource::File(project));
        }

        if env::apply(&mut config)? {
            sources.push(ConfigSource::Environment);
        }

        config.validate()?;
        Ok(LoadedConfig { config, sources })
    }

    /// Parse one TOML file.
    pub fn from_file(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            MetaAgentError::Configuration(format!("cannot read {}: {e}", path.display()))
        })?;
        Self::from_toml(&text)
    }

    /// Parse a TOML string.
    pub fn from_toml(text: &str) -> Result<Config> {
        toml::from_str(text)
            .map_err(|e| MetaAgentError::Configuration(format!("invalid TOML: {e}")))
    }
}

/// `~/.config/cuma/config.toml`, when a home directory exists.
pub fn global_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("cuma").join("config.toml"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn an_empty_config_file_is_valid_and_yields_defaults() {
        let config = Config::from_toml("").unwrap();
        assert_eq!(config.router.strategy, RoutingStrategy::Balanced);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_malformed_config_file_is_an_error_not_a_silent_default() {
        let err = Config::from_toml("router = [[[").unwrap_err();
        assert!(matches!(err, MetaAgentError::Configuration(_)));
    }

    #[test]
    fn an_unknown_key_is_rejected_so_typos_surface() {
        let err = Config::from_toml("[router]\nstartegy = \"balanced\"\n").unwrap_err();
        assert!(err.to_string().contains("startegy"), "got: {err}");
    }
}

//! Configuration from environment variables, parsed once at startup.

use std::collections::HashMap;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::vault::parse_write_folders;

/// Size cap for `MCP_INSTRUCTIONS_FILE`.
const MCP_INSTRUCTIONS_MAX_BYTES: u64 = 32 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("Set VAULT_PATH: the vault folder, or the local mirror folder when S3_BUCKET is set.")]
    MissingVaultPath,
    #[error("Invalid {name}: {reason}")]
    Invalid { name: &'static str, reason: String },
    #[error("Semantic search needs all of CF_ACCOUNT_ID, CF_AI_SEARCH_TOKEN and CF_AI_SEARCH_INSTANCE.")]
    PartialSemanticSearch,
    #[error("Failed to read MCP_INSTRUCTIONS_FILE ({path}): {reason}")]
    InstructionsFile { path: String, reason: String },
}

/// S3 mode settings (`S3_BUCKET` set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Config {
    pub endpoint: Option<String>,
    pub bucket: String,
    pub region: String,
    pub prefix: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub poll_seconds: u64,
}

/// Cloudflare AI Search settings, all-or-nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticConfig {
    pub account_id: String,
    pub token: String,
    pub instance: String,
    pub namespace: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub log_level: Option<String>,
    pub vault_path: PathBuf,
    pub vault_name: String,
    pub index_passphrase: Option<String>,
    pub s3: Option<S3Config>,
    /// Note content kept in memory for search, in characters.
    pub search_content_cache_chars: usize,
    pub semantic: Option<SemanticConfig>,
    pub host: String,
    pub port: u16,
    pub base_url: String,
    pub auth_token: Option<String>,
    pub refresh_days: u32,
    pub read_only: bool,
    pub write_folders: Option<Vec<String>>,
    pub allowed_hosts: Option<String>,
    /// Extra text appended to the MCP instructions.
    pub extra_instructions: Option<String>,
    /// Ignored `MCP_INSTRUCTIONS` because a file was given; worth a warning.
    pub ignored_inline_instructions: bool,
    /// Per-vault folder for the index, the S3 manifest and OAuth state.
    pub data_dir: PathBuf,
}

/// Raw variables; tests pass a map instead of the process environment.
struct Vars(HashMap<String, String>);

impl Vars {
    fn raw(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Unset, empty, and whitespace-only all mean "not configured".
    fn optional(&self, name: &str) -> Option<String> {
        self.raw(name).map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned)
    }

    /// Set (even to an empty string) means configured, verbatim.
    fn verbatim(&self, name: &str) -> Option<String> {
        self.raw(name).map(str::to_owned)
    }

    fn string_or(&self, name: &str, default: &str) -> String {
        self.verbatim(name).unwrap_or_else(|| default.to_owned())
    }

    fn number<T: std::str::FromStr + PartialOrd + std::fmt::Display>(
        &self,
        name: &'static str,
        default: T,
        min: T,
    ) -> Result<T, ConfigError> {
        let Some(raw) = self.optional(name) else { return Ok(default) };
        let value: T = raw.parse().map_err(|_| ConfigError::Invalid {
            name,
            reason: format!("expected a number, got '{raw}'"),
        })?;
        if value < min {
            return Err(ConfigError::Invalid { name, reason: format!("must be at least {min}") });
        }
        Ok(value)
    }

    /// `true/1/yes/on/y/enabled` enable, `false/0/no/off/n/disabled` disable
    /// (any case), and empty or unset means false. Anything else fails at
    /// startup instead of silently leaving a safety switch such as `READ_ONLY` off.
    fn flag(&self, name: &'static str) -> Result<bool, ConfigError> {
        let Some(raw) = self.raw(name).filter(|v| !v.is_empty()) else { return Ok(false) };
        match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" | "y" | "enabled" => Ok(true),
            "false" | "0" | "no" | "off" | "n" | "disabled" => Ok(false),
            _ => Err(ConfigError::Invalid { name, reason: format!("expected true or false, got '{raw}'") }),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(std::env::vars().collect())
    }

    pub fn from_vars(vars: HashMap<String, String>) -> Result<Self, ConfigError> {
        let vars = Vars(vars);
        let port_raw = vars.string_or("PORT", "8787");
        let port = port_raw
            .parse::<u16>()
            .ok()
            .filter(|p| *p >= 1 && port_raw.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| ConfigError::Invalid { name: "PORT", reason: format!("expected 1-65535, got '{port_raw}'") })?;
        let vault_path = vars.raw("VAULT_PATH").filter(|v| !v.is_empty()).ok_or(ConfigError::MissingVaultPath)?;
        let vault_name = vars.string_or("VAULT_NAME", "MyVault");

        let s3 = match vars.optional("S3_BUCKET") {
            Some(bucket) => Some(S3Config {
                endpoint: vars.optional("S3_ENDPOINT"),
                bucket,
                region: vars.string_or("S3_REGION", "auto"),
                prefix: vars.string_or("S3_PREFIX", ""),
                access_key_id: vars.verbatim("S3_ACCESS_KEY_ID").filter(|v| !v.is_empty()),
                secret_access_key: vars.verbatim("S3_SECRET_ACCESS_KEY").filter(|v| !v.is_empty()),
                poll_seconds: vars.number("S3_POLL_SECONDS", 30, 5)?,
            }),
            None => None,
        };

        let cf = [vars.optional("CF_ACCOUNT_ID"), vars.optional("CF_AI_SEARCH_TOKEN"), vars.optional("CF_AI_SEARCH_INSTANCE")];
        let semantic = match cf {
            [Some(account_id), Some(token), Some(instance)] => Some(SemanticConfig {
                account_id,
                token,
                instance,
                namespace: vars.string_or("CF_AI_SEARCH_NAMESPACE", "default"),
            }),
            [None, None, None] => None,
            _ => return Err(ConfigError::PartialSemanticSearch),
        };

        let cache_mb: f64 = vars.number("SEARCH_CONTENT_CACHE_MB", 32.0, 0.0)?;
        let (extra_instructions, ignored_inline_instructions) = match vars.optional("MCP_INSTRUCTIONS_FILE") {
            Some(path) => {
                let text = read_instructions_file(&path)?;
                (Some(text).filter(|t| !t.is_empty()), vars.optional("MCP_INSTRUCTIONS").is_some())
            }
            None => (vars.optional("MCP_INSTRUCTIONS"), false),
        };

        let base_data_dir = vars.verbatim("DATA_DIR").map(PathBuf::from).unwrap_or_else(|| {
            let home = vars.verbatim("HOME").or_else(|| vars.verbatim("USERPROFILE")).unwrap_or_else(|| "/tmp".into());
            PathBuf::from(home).join(".obsidian-mcp")
        });
        let vault_id = &hex::encode(Sha256::digest(vault_name.as_bytes()))[..12];

        Ok(Self {
            log_level: vars.verbatim("LOG_LEVEL"),
            vault_path: PathBuf::from(vault_path),
            index_passphrase: vars.optional("INDEX_PASSPHRASE"),
            s3,
            search_content_cache_chars: (cache_mb * 1024.0 * 1024.0).round() as usize,
            semantic,
            host: vars.string_or("HOST", "0.0.0.0"),
            port,
            base_url: vars.verbatim("BASE_URL").unwrap_or_else(|| format!("http://localhost:{port}")),
            auth_token: vars.verbatim("MCP_AUTH_TOKEN").filter(|t| !t.is_empty()),
            refresh_days: vars.number("MCP_REFRESH_DAYS", 14, 1)?,
            read_only: vars.flag("READ_ONLY")?,
            write_folders: parse_write_folders(vars.raw("WRITE_FOLDERS")),
            allowed_hosts: vars.verbatim("MCP_ALLOWED_HOSTS"),
            extra_instructions,
            ignored_inline_instructions,
            data_dir: base_data_dir.join(vault_id),
            vault_name,
        })
    }

    pub fn debug_logging(&self) -> bool {
        matches!(self.log_level.as_deref().map(str::to_ascii_lowercase).as_deref(), Some("debug" | "trace"))
    }
}

fn read_instructions_file(path: &str) -> Result<String, ConfigError> {
    let fail = |reason: String| ConfigError::InstructionsFile { path: path.to_owned(), reason };
    let size = std::fs::metadata(path).map_err(|e| fail(e.to_string()))?.len();
    if size > MCP_INSTRUCTIONS_MAX_BYTES {
        return Err(fail(format!("file is {size} bytes, exceeds {MCP_INSTRUCTIONS_MAX_BYTES} byte cap")));
    }
    let text = std::fs::read_to_string(path).map_err(|e| fail(e.to_string()))?;
    Ok(text.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let mut vars: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        vars.entry("VAULT_PATH".into()).or_insert_with(|| "/vault".into());
        Config::from_vars(vars)
    }

    #[test]
    fn defaults() {
        let c = config(&[("HOME", "/home/u")]).unwrap();
        assert_eq!(c.port, 8787);
        assert_eq!(c.vault_name, "MyVault");
        assert_eq!(c.base_url, "http://localhost:8787");
        assert_eq!(c.host, "0.0.0.0");
        assert!(!c.read_only);
        assert_eq!(c.refresh_days, 14);
        assert_eq!(c.search_content_cache_chars, 32 * 1024 * 1024);
        assert!(c.s3.is_none() && c.semantic.is_none() && c.auth_token.is_none());
        assert!(c.data_dir.starts_with("/home/u/.obsidian-mcp"));
        assert_eq!(c.data_dir.file_name().unwrap().len(), 12);
    }

    #[test]
    fn requires_a_vault_path() {
        assert_eq!(Config::from_vars(HashMap::new()), Err(ConfigError::MissingVaultPath));
    }

    #[test]
    fn parses_flags_strictly() {
        assert!(config(&[("READ_ONLY", "TRUE")]).unwrap().read_only);
        assert!(config(&[("READ_ONLY", "on")]).unwrap().read_only);
        assert!(!config(&[("READ_ONLY", "")]).unwrap().read_only);
        assert!(!config(&[("READ_ONLY", "no")]).unwrap().read_only);
        assert!(config(&[("READ_ONLY", "maybe")]).is_err());
    }

    #[test]
    fn validates_numbers() {
        assert!(config(&[("PORT", "0")]).is_err());
        assert!(config(&[("PORT", "70000")]).is_err());
        assert!(config(&[("PORT", "+80")]).is_err());
        assert_eq!(config(&[("PORT", "9000")]).unwrap().base_url, "http://localhost:9000");
        assert!(config(&[("S3_BUCKET", "b"), ("S3_POLL_SECONDS", "2")]).is_err());
        assert_eq!(config(&[("S3_BUCKET", "b")]).unwrap().s3.unwrap().poll_seconds, 30);
        assert_eq!(config(&[("SEARCH_CONTENT_CACHE_MB", "0")]).unwrap().search_content_cache_chars, 0);
    }

    #[test]
    fn semantic_search_is_all_or_nothing() {
        assert_eq!(config(&[("CF_ACCOUNT_ID", "a")]), Err(ConfigError::PartialSemanticSearch));
        let c = config(&[("CF_ACCOUNT_ID", "a"), ("CF_AI_SEARCH_TOKEN", "t"), ("CF_AI_SEARCH_INSTANCE", "i")]).unwrap();
        assert_eq!(c.semantic.unwrap().namespace, "default");
    }

    #[test]
    fn instructions_file_wins() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rules.md");
        std::fs::write(&file, "  file rule \n").unwrap();
        let path = file.to_str().unwrap();
        let c = config(&[("MCP_INSTRUCTIONS_FILE", path), ("MCP_INSTRUCTIONS", "inline")]).unwrap();
        assert_eq!(c.extra_instructions.as_deref(), Some("file rule"));
        assert!(c.ignored_inline_instructions);
        assert_eq!(config(&[("MCP_INSTRUCTIONS", " inline ")]).unwrap().extra_instructions.as_deref(), Some("inline"));
        std::fs::write(&file, "x".repeat(40 * 1024)).unwrap();
        assert!(matches!(config(&[("MCP_INSTRUCTIONS_FILE", path)]), Err(ConfigError::InstructionsFile { .. })));
        assert!(config(&[("MCP_INSTRUCTIONS_FILE", "/missing/file")]).is_err());
    }

    #[test]
    fn write_folders() {
        assert_eq!(config(&[("WRITE_FOLDERS", "a, /b/")]).unwrap().write_folders, Some(vec!["a".into(), "b".into()]));
    }
}

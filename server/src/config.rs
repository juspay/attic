//! Server configuration.

use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use async_compression::Level as CompressionLevel;
use aws_sdk_kms::primitives::Blob;
use aws_sdk_kms::Client;
use aws_types::region::Region;
use base64::engine::general_purpose;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use derivative::Derivative;
use serde::{de, Deserialize};
use xdg::BaseDirectories;

use crate::access::{decode_token_hs256_secret_base64, HS256Key};
use crate::narinfo::Compression as NixCompression;
use crate::storage::{LocalStorageConfig, S3StorageConfig};

/// Application prefix in XDG base directories.
///
/// This will be concatenated into `$XDG_CONFIG_HOME/attic`.
const XDG_PREFIX: &str = "attic";

/// Environment variable storing the Base64-encoded TOML configuration.
///
/// This is useful for deploying to certain application platforms like Fly.io
const ENV_CONFIG_BASE64: &str = "ATTIC_SERVER_CONFIG_BASE64";

/// Environment variable storing the Base64-encoded HS256 JWT secret.
const ENV_TOKEN_HS256_SECRET_BASE64: &str = "ATTIC_SERVER_TOKEN_HS256_SECRET_BASE64";

/// Environment variable storing the database connection string.
const ENV_DATABASE_URL: &str = "ATTIC_SERVER_DATABASE_URL";

/// Environment variable storing the database type.
const ENV_DATABASE_TYPE: &str = "ATTIC_SERVER_DATABASE_TYPE";
const ENV_DATABASE_USER_NAME: &str = "ATTIC_SERVER_DATABASE_USER_NAME";
const ENV_DATABASE_USER_PASSWORD: &str = "ATTIC_SERVER_DATABASE_USER_PASSWORD";
const ENV_DATABASE_HOSTNAME: &str = "ATTIC_SERVER_DATABASE_HOSTNAME";
const ENV_DATABASE_PORT: &str = "ATTIC_SERVER_DATABASE_PORT";
const ENV_DATABASE_DATABASE_NAME: &str = "ATTIC_SERVER_DATABASE_DATABASE_NAME";
const ENV_DATABASE_PASSWORD_KMS_ENCRYPTED: &str = "ATTIC_SERVER_DATABASE_PASSWORD_KMS_ENCRYPTED";

/// Configuration for the Attic Server.
#[derive(Clone, Derivative, Deserialize)]
#[derivative(Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Socket address to listen on.
    #[serde(default = "default_listen_address")]
    pub listen: SocketAddr,

    /// Allowed `Host` headers.
    ///
    /// This _must_ be configured for production use. If unconfigured or the
    /// list is empty, all `Host` headers are allowed.
    #[serde(rename = "allowed-hosts")]
    #[serde(default = "Vec::new")]
    pub allowed_hosts: Vec<String>,

    /// The canonical API endpoint of this server.
    ///
    /// This is the endpoint exposed to clients in `cache-config` responses.
    ///
    /// This _must_ be configured for production use. If not configured, the
    /// API endpoint is synthesized from the client's `Host` header which may
    /// be insecure.
    ///
    /// The API endpoint _must_ end with a slash (e.g., `https://domain.tld/attic/`
    /// not `https://domain.tld/attic`).
    #[serde(rename = "api-endpoint")]
    pub api_endpoint: Option<String>,

    /// The canonical Nix Binary Cache endpoint of this server.
    ///
    /// This is usually the same as `api_endpoint` but can be configured
    /// to a different value.
    ///
    /// If unconfigured, it's assumed to be the same as `api_endpoint`.
    #[serde(rename = "substituter-endpoint")]
    pub substituter_endpoint: Option<String>,

    /// Whether to soft-delete caches.
    ///
    /// If this is enabled, caches are soft-deleted instead of actually
    /// removed from the database. Note that soft-deleted caches cannot
    /// have their names reused as long as the original database records
    /// are there.
    #[serde(rename = "soft-delete-caches")]
    #[serde(default = "default_soft_delete_caches")]
    pub soft_delete_caches: bool,

    /// Whether to require fully uploading a NAR if it exists in the global cache.
    ///
    /// If set to false, simply knowing the NAR hash is enough for
    /// an uploader to gain access to an existing NAR in the global
    /// cache.
    #[serde(rename = "require-proof-of-possession")]
    #[serde(default = "default_require_proof_of_possession")]
    pub require_proof_of_possession: bool,

    /// Database connection.
    pub database: DatabaseConfig,

    /// Storage.
    pub storage: StorageConfig,

    /// Data chunking.
    pub chunking: ChunkingConfig,

    /// Compression.
    #[serde(default = "Default::default")]
    pub compression: CompressionConfig,

    /// Garbage collection.
    #[serde(rename = "garbage-collection")]
    #[serde(default = "Default::default")]
    pub garbage_collection: GarbageCollectionConfig,

    /// JSON Web Token HMAC secret.
    ///
    /// Set this to the base64 encoding of a randomly generated secret.
    #[serde(rename = "token-hs256-secret-base64")]
    #[serde(deserialize_with = "deserialize_token_hs256_secret_base64")]
    #[serde(default = "load_token_hs256_secret_from_env")]
    #[derivative(Debug = "ignore")]
    pub token_hs256_secret: HS256Key,
}

/// Database connection configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// Connection URL.
    #[serde(default = "load_database_url_from_env")]
    pub url: String,

    /// Whether to enable sending of periodic heartbeat queries.
    ///
    /// If enabled, a heartbeat query will be sent every minute.
    #[serde(default = "default_db_heartbeat")]
    pub heartbeat: bool,
}

/// File storage configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum StorageConfig {
    /// Local file storage.
    #[serde(rename = "local")]
    Local(LocalStorageConfig),

    /// S3 storage.
    #[serde(rename = "s3")]
    S3(S3StorageConfig),
}

/// Data chunking.
///
/// This must be set, but a default set of values is provided
/// through the OOBE sequence. The reason is that this allows
/// us to provide a new set of recommended "defaults" for newer
/// deployments without affecting existing ones.
///
/// Warning: If you change any of the values here, it will be
/// difficult to reuse existing chunks for newly-uploaded NARs
/// since the cutpoints will be different. As a result, the
/// deduplication ratio will suffer for a while after the change.
///
/// `atticadm test-chunking` provides a way to test chunking
/// on a set of files so you can fine-tune the values.
#[derive(Debug, Clone, Deserialize)]
pub struct ChunkingConfig {
    /// The minimum NAR size to trigger chunking.
    ///
    /// If 0, chunking is disabled entirely for newly-uploaded
    /// NARs.
    ///
    /// If 1, all newly-uploaded NARs are chunked.
    ///
    /// By default, the threshold is 128KB.
    #[serde(rename = "nar-size-threshold")]
    pub nar_size_threshold: usize,

    /// The preferred minimum size of a chunk, in bytes.
    #[serde(rename = "min-size")]
    pub min_size: usize,

    /// The preferred average size of a chunk, in bytes.
    #[serde(rename = "avg-size")]
    pub avg_size: usize,

    /// The preferred maximum size of a chunk, in bytes.
    #[serde(rename = "max-size")]
    pub max_size: usize,
}

/// Compression configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct CompressionConfig {
    /// Compression type.
    pub r#type: CompressionType,

    /// Compression level.
    ///
    /// If unspecified, Attic will choose a default one.
    pub level: Option<i32>,
}

/// Compression type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum CompressionType {
    /// No compression.
    #[serde(rename = "none")]
    None,

    /// Brotli.
    #[serde(rename = "brotli")]
    Brotli,

    /// ZSTD.
    #[serde(rename = "zstd")]
    Zstd,

    /// XZ.
    #[serde(rename = "xz")]
    Xz,
}

/// Garbage collection config.
#[derive(Debug, Clone, Deserialize)]
pub struct GarbageCollectionConfig {
    /// The frequency to run garbage collection at.
    ///
    /// If zero, automatic garbage collection is disabled, but
    /// it can still be run manually with `atticd --mode garbage-collector-once`.
    #[serde(with = "humantime_serde", default = "default_gc_interval")]
    pub interval: Duration,

    /// The default retention period of unaccessed objects.
    ///
    /// Objects are subject to garbage collection if both the
    /// `created_at` and `last_accessed_at` timestamps are older
    /// than the retention period.
    ///
    /// Zero (default) means time-based garbage-collection is
    /// disabled by default. You can enable it on a per-cache basis.
    #[serde(rename = "default-retention-period")]
    #[serde(with = "humantime_serde", default = "default_default_retention_period")]
    pub default_retention_period: Duration,
}

fn load_token_hs256_secret_from_env() -> HS256Key {
    let s = env::var(ENV_TOKEN_HS256_SECRET_BASE64)
        .expect("The HS256 secret must be specified in either token_hs256_secret or the ATTIC_SERVER_TOKEN_HS256_SECRET_BASE64 environment.");

    decode_token_hs256_secret_base64(&s).expect("Failed to load as decoding key")
}

async fn get_kms_decrypted_password(password_text_blob: Blob) -> String {
    let region = match env::var("AWS_REGION") {
        Ok(val) => Some(Region::new(val)),
        _ => Some(Region::new("ap-south-1")),
    };
    let shared_config = aws_config::load_from_env().await;
    let config = aws_sdk_kms::config::Builder::from(&shared_config)
        .region(region)
        .build();
    let resp = Client::from_conf(config)
        .decrypt()
        .key_id("key")
        .ciphertext_blob(password_text_blob)
        .send()
        .await
        .expect("");

    let inner = resp.plaintext.unwrap();
    let bytes = inner.as_ref();
    String::from_utf8(bytes.to_vec()).expect("Could not convert to UTF-8")
}

#[::tokio::main]
async fn load_database_url_from_env() -> String {
    let database_type = match env::var(ENV_DATABASE_TYPE) {
        Ok(val) => val,
        _ => "sqlite".to_string(),
    };

    if database_type.to_lowercase() == "postgresql" {
        let user = env::var(ENV_DATABASE_USER_NAME).expect("missing ENV_DATABASE_USER_NAME ENV");
        let true_str = "true".to_string();
        let password_decrypted = match env::var(ENV_DATABASE_PASSWORD_KMS_ENCRYPTED) {
            Ok(str) => {
                if true_str == str.to_lowercase() {
                    let password_base64_encoded = env::var(ENV_DATABASE_USER_PASSWORD)
                        .expect("missing ENV_DATABASE_USER_PASSWORD ENV");
                    let password_text_bas64_decoded = general_purpose::STANDARD
                        .decode(password_base64_encoded)
                        .expect("Input file does not contain valid base 64 characters.");
                    let password_text_blob = Blob::new(password_text_bas64_decoded);
                    get_kms_decrypted_password(password_text_blob).await
                } else {
                    env::var(ENV_DATABASE_USER_PASSWORD)
                        .expect("missing ENV_DATABASE_USER_PASSWORD ENV")
                }
            }
            _ => env::var(ENV_DATABASE_USER_PASSWORD)
                .expect("missing ENV_DATABASE_USER_PASSWORD ENV"),
        };
        let hostname = env::var(ENV_DATABASE_HOSTNAME).expect("missing ENV_DATABASE_HOSTNAME ENV");
        let port = match env::var(ENV_DATABASE_PORT) {
            Ok(val) => val,
            _ => "5432".to_string(),
        };
        let database_name =
            env::var(ENV_DATABASE_DATABASE_NAME).expect("missing ENV_DATABASE_DATABASE_NAME ENV");
        "postgres://".to_owned()
            + &user
            + ":"
            + &password_decrypted
            + "@"
            + &hostname
            + ":"
            + &port
            + "/"
            + &database_name
    } else {
        env::var(ENV_DATABASE_URL)
                .expect("Database URL must be specified in either database.url or the ATTIC_SERVER_DATABASE_URL environment.")
    }
}

impl CompressionConfig {
    pub fn level(&self) -> CompressionLevel {
        if let Some(level) = self.level {
            return CompressionLevel::Precise(level);
        }

        match self.r#type {
            CompressionType::Brotli => CompressionLevel::Precise(5),
            CompressionType::Zstd => CompressionLevel::Precise(8),
            CompressionType::Xz => CompressionLevel::Precise(2),
            _ => CompressionLevel::Default,
        }
    }
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            r#type: CompressionType::Zstd,
            level: None,
        }
    }
}

impl From<CompressionType> for NixCompression {
    fn from(t: CompressionType) -> Self {
        match t {
            CompressionType::None => NixCompression::None,
            CompressionType::Brotli => NixCompression::Brotli,
            CompressionType::Zstd => NixCompression::Zstd,
            CompressionType::Xz => NixCompression::Xz,
        }
    }
}

impl Default for GarbageCollectionConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(43200),
            default_retention_period: Duration::ZERO,
        }
    }
}

fn deserialize_token_hs256_secret_base64<'de, D>(deserializer: D) -> Result<HS256Key, D::Error>
where
    D: de::Deserializer<'de>,
{
    use de::Error;

    let s = String::deserialize(deserializer)?;
    let key = decode_token_hs256_secret_base64(&s).map_err(Error::custom)?;

    Ok(key)
}

fn default_listen_address() -> SocketAddr {
    "[::]:8080".parse().unwrap()
}

fn default_db_heartbeat() -> bool {
    false
}

fn default_soft_delete_caches() -> bool {
    false
}

fn default_require_proof_of_possession() -> bool {
    true
}

fn default_gc_interval() -> Duration {
    Duration::from_secs(43200)
}

fn default_default_retention_period() -> Duration {
    Duration::ZERO
}

fn load_config_from_path(path: &Path) -> Result<Config> {
    tracing::info!("Using configurations: {:?}", path);

    let config = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&config)?)
}

fn load_config_from_str(s: &str) -> Result<Config> {
    tracing::info!("Using configurations from environment variable");
    Ok(toml::from_str(s)?)
}

/// Loads the configuration in the standard order.
pub async fn load_config(config_path: Option<&Path>, allow_oobe: bool) -> Result<Config> {
    if let Some(config_path) = config_path {
        load_config_from_path(config_path)
    } else if let Ok(config_env) = env::var(ENV_CONFIG_BASE64) {
        let decoded = String::from_utf8(BASE64_STANDARD.decode(config_env.as_bytes())?)?;
        load_config_from_str(&decoded)
    } else {
        // Config from XDG
        let config_path = get_xdg_config_path()?;

        if allow_oobe {
            // Special OOBE sequence
            crate::oobe::run_oobe().await?;
        }

        load_config_from_path(&config_path)
    }
}

pub fn get_xdg_config_path() -> anyhow::Result<PathBuf> {
    let xdg_dirs = BaseDirectories::with_prefix(XDG_PREFIX)?;
    let config_path = xdg_dirs.place_config_file("server.toml")?;

    Ok(config_path)
}

pub fn get_xdg_data_path() -> anyhow::Result<PathBuf> {
    let xdg_dirs = BaseDirectories::with_prefix(XDG_PREFIX)?;
    let data_path = xdg_dirs.create_data_directory("")?;

    Ok(data_path)
}

use std::{
    collections::BTreeMap,
    hash::{DefaultHasher, Hash, Hasher},
    io::SeekFrom,
    path::PathBuf,
};

use async_fd_lock::{LockWrite, RwLockWriteGuard};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use pixi_build_discovery::EnabledProtocols;
use pixi_build_types::{CondaPackageMetadata, procedures::conda_outputs::CondaOutput};
use pixi_record::{InputHash, PinnedSourceSpec};
use rattler_conda_types::ChannelUrl;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::{BuildEnvironment, PackageIdentifier, build::source_checkout_cache_key};

/// A cache for caching the metadata of a source checkout.
///
/// To request metadata for a source checkout we need to invoke the build
/// backend associated with the given source checkout. This operation can be
/// time-consuming so we want to avoid having to query the build backend.
///
/// This cache stores the raw response for a given source checkout together with
/// some additional properties to determine if the cache is still valid.
#[derive(Clone)]
pub struct SourceMetadataCache {
    root: PathBuf,
}

#[derive(Debug, Error)]
pub enum SourceMetadataCacheError {
    /// An I/O error occurred while reading or writing the cache.
    #[error("an IO error occurred while {0} {1}")]
    IoError(String, PathBuf, #[source] std::io::Error),
}

/// Defines additional input besides the source files that are used to compute
/// the metadata of a source checkout. This is used to bucket the metadata.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct SourceMetadataKey {
    /// The URLs of the channels that were used.
    pub channel_urls: Vec<ChannelUrl>,

    /// The build environment
    pub build_environment: BuildEnvironment,

    /// The variants that were used
    pub build_variants: BTreeMap<String, Vec<String>>,

    /// The protocols that are enabled for source packages
    pub enabled_protocols: EnabledProtocols,

    /// The pinned source location
    pub pinned_source: PinnedSourceSpec,
}

impl SourceMetadataKey {
    /// Computes a unique semi-human-readable hash for this key.
    pub fn hash_key(&self) -> String {
        let mut hasher = DefaultHasher::new();
        self.channel_urls.hash(&mut hasher);
        self.build_environment.build_platform.hash(&mut hasher);
        self.build_environment
            .build_virtual_packages
            .hash(&mut hasher);
        self.build_environment
            .host_virtual_packages
            .hash(&mut hasher);
        self.build_variants.hash(&mut hasher);
        self.enabled_protocols.hash(&mut hasher);
        let source_dir = source_checkout_cache_key(&self.pinned_source);
        format!(
            "{source_dir}/{}-{}",
            self.build_environment.host_platform,
            URL_SAFE_NO_PAD.encode(hasher.finish().to_ne_bytes())
        )
    }
}

impl SourceMetadataCache {
    /// The version identifier that should be used for the cache directory.
    pub const CACHE_SUFFIX: &'static str = "v0";

    /// Constructs a new instance.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Returns the cache entry for the given source checkout and input.
    ///
    /// Returns the cached metadata if it exists and is still valid and a
    /// [`CacheEntry`] that can be used to update the cache. As long as the
    /// [`CacheEntry`] is held, another process cannot update the cache.
    pub async fn entry(
        &self,
        input: &SourceMetadataKey,
    ) -> Result<(Option<CachedCondaMetadata>, CacheEntry), SourceMetadataCacheError> {
        // Locate the cache file and lock it.
        let cache_dir = self.root.join(input.hash_key());
        tokio::fs::create_dir_all(&cache_dir).await.map_err(|e| {
            SourceMetadataCacheError::IoError(
                "creating cache directory".to_string(),
                cache_dir.clone(),
                e,
            )
        })?;

        // Try to acquire a lock on the cache file.
        let cache_file_path = cache_dir.join("metadata.json");
        let cache_file = tokio::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .truncate(false)
            .create(true)
            .open(&cache_file_path)
            .await
            .map_err(|e| {
                SourceMetadataCacheError::IoError(
                    "opening cache file".to_string(),
                    cache_file_path.clone(),
                    e,
                )
            })?;

        let mut locked_cache_file = cache_file.lock_write().await.map_err(|e| {
            SourceMetadataCacheError::IoError(
                "locking cache file".to_string(),
                cache_file_path.clone(),
                e.error,
            )
        })?;

        // Try to parse the contents of the file
        let mut cache_file_contents = String::new();
        locked_cache_file
            .read_to_string(&mut cache_file_contents)
            .await
            .map_err(|e| {
                SourceMetadataCacheError::IoError(
                    "reading cache file".to_string(),
                    cache_file_path.clone(),
                    e,
                )
            })?;

        let metadata = serde_json::from_str(&cache_file_contents).ok();
        Ok((
            metadata,
            CacheEntry {
                file: locked_cache_file,
                path: cache_file_path,
            },
        ))
    }
}

/// A cache entry returned by [`SourceMetadataCache::entry`] which enables
/// updating the cache.
///
/// As long as this entry is held, no other process can access this cache entry.
#[derive(Debug)]
pub struct CacheEntry {
    file: RwLockWriteGuard<tokio::fs::File>,
    path: PathBuf,
}

impl CacheEntry {
    /// Writes the given metadata to the cache.
    pub async fn write(
        &mut self,
        metadata: CachedCondaMetadata,
    ) -> Result<(), SourceMetadataCacheError> {
        self.file.seek(SeekFrom::Start(0)).await.map_err(|e| {
            SourceMetadataCacheError::IoError(
                "seeking to start of cache file".to_string(),
                self.path.clone(),
                e,
            )
        })?;
        let bytes = serde_json::to_vec(&metadata).expect("serialization to JSON should not fail");
        self.file.write_all(&bytes).await.map_err(|e| {
            SourceMetadataCacheError::IoError(
                "writing metadata to cache file".to_string(),
                self.path.clone(),
                e,
            )
        })?;
        self.file
            .inner_mut()
            .set_len(bytes.len() as u64)
            .await
            .map_err(|e| {
                SourceMetadataCacheError::IoError(
                    "setting length of cache file".to_string(),
                    self.path.clone(),
                    e,
                )
            })?;
        Ok(())
    }
}

/// Cached result of calling `conda/getMetadata` on a build backend. This is
/// returned by [`SourceMetadataCache::entry`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedCondaMetadata {
    /// A randomly generated identifier that is generated for each metadata
    /// file.
    ///
    /// Cache information for each output is stored in a separate file, this ID
    /// is present in each file. This is to ensure that the cache can be
    /// invalidated if the metadata changes.
    pub id: u64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hash: Option<InputHash>,

    #[serde(flatten)]
    pub metadata: MetadataKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MetadataKind {
    /// The result of calling `conda/getMetadata` on a build backend.
    GetMetadata { packages: Vec<CondaPackageMetadata> },

    /// The result of calling `conda/outputs` on a build backend.
    Outputs { outputs: Vec<CondaOutput> },
}

impl CachedCondaMetadata {
    /// Returns the unique package identifiers for the packages in this
    /// metadata.
    pub fn outputs(&self) -> Vec<PackageIdentifier> {
        match &self.metadata {
            MetadataKind::GetMetadata { packages } => packages
                .iter()
                .map(|pkg| PackageIdentifier {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    build: pkg.build.clone(),
                    subdir: pkg.subdir.to_string(),
                })
                .collect(),
            MetadataKind::Outputs { outputs } => outputs
                .iter()
                .map(|output| PackageIdentifier {
                    name: output.metadata.name.clone(),
                    version: output.metadata.version.clone(),
                    build: output.metadata.build.clone(),
                    subdir: output.metadata.subdir.to_string(),
                })
                .collect(),
        }
    }
}

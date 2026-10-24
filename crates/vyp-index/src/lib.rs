//! PyPI index client, in-memory index, and metadata cache.
//!
//! Provides `PyPIMetadataProvider`, `InMemoryIndex`, disk cache, wheel compatibility,
//! and offline provider for tests.

pub mod auth;
pub mod cache;
pub mod client;
pub mod in_memory_index;
pub mod metadata;
pub mod pypi;
pub mod sdist;
pub mod timestamp;
pub mod variants;
pub mod version_filter;
pub mod wheel_compat;
pub mod wheel_metadata;

pub use cache::MetadataCache;
pub use client::OfflineMetadataProvider;
pub use in_memory_index::{InMemoryIndex, MetadataResult, VersionsResult, WheelInfo};
pub use pypi::{ProfileCounters, PyPIMetadataProvider};
pub use version_filter::{requires_python_ok, version_is_viable, ExcludeNewer, FilterOptions, SdistPolicy};
pub use wheel_compat::PlatformTags;

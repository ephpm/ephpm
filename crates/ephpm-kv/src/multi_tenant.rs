//! Multi-tenant KV store with per-site isolation.
//!
//! Each virtual host gets its own [`Store`] instance, created lazily on
//! first access. Provides physical key isolation — a site's store is a
//! separate `DashMap`, not a prefix in a shared map.

use std::sync::Arc;

use dashmap::DashMap;

use crate::store::{Store, StoreConfig};

/// A multi-tenant KV store that manages per-site [`Store`] instances.
///
/// Sites are created lazily on first access. Each site gets its own
/// `DashMap` with independent memory limits and key spaces.
///
/// Thread-safe and cheaply cloneable.
#[derive(Clone)]
pub struct MultiTenantStore {
    sites: Arc<DashMap<String, Arc<Store>>>,
    /// Config template used when creating new site stores.
    site_config: StoreConfig,
    /// Fallback store for single-site mode (when no hostname is provided).
    default_store: Arc<Store>,
}

impl MultiTenantStore {
    /// Create a new multi-tenant store.
    ///
    /// `default_store` is used when no hostname is specified (single-site mode).
    /// `site_config` is the template for creating per-site stores.
    #[must_use]
    pub fn new(default_store: Arc<Store>, site_config: StoreConfig) -> Self {
        Self {
            sites: Arc::new(DashMap::new()),
            site_config,
            default_store,
        }
    }

    /// Get or create a store for the given hostname.
    ///
    /// The store is created lazily on first access with the template config.
    /// Subsequent calls return the same store instance.
    pub fn get_site_store(&self, hostname: &str) -> Arc<Store> {
        if hostname.is_empty() {
            return Arc::clone(&self.default_store);
        }

        if let Some(store) = self.sites.get(hostname) {
            return Arc::clone(store.value());
        }

        // Create a new store for this site.
        let store = Store::new(self.site_config.clone());
        self.sites.insert(hostname.to_string(), Arc::clone(&store));
        tracing::info!(hostname, "created KV store for site");
        store
    }

    /// Get the default store (for single-site mode or admin access).
    #[must_use]
    pub fn default_store(&self) -> &Arc<Store> {
        &self.default_store
    }

    /// Number of site stores currently active.
    #[must_use]
    pub fn site_count(&self) -> usize {
        self.sites.len()
    }

    /// Remove a site's store (e.g., when a preview is torn down).
    ///
    /// Returns `true` if the site existed.
    #[must_use]
    pub fn remove_site(&self, hostname: &str) -> bool {
        self.sites.remove(hostname).is_some()
    }

    /// Authenticate a RESP connection for a specific site.
    ///
    /// Returns the site's store if the hostname is valid.
    /// Password validation is handled by the caller.
    #[must_use]
    pub fn auth_site(&self, hostname: &str) -> Option<Arc<Store>> {
        if hostname.is_empty() {
            return None;
        }
        Some(self.get_site_store(hostname))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> StoreConfig {
        StoreConfig {
            memory_limit: 1024 * 1024, // 1 MB per site
            ..StoreConfig::default()
        }
    }

    #[test]
    fn get_creates_store_on_first_access() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());

        assert_eq!(mt.site_count(), 0);
        let store = mt.get_site_store("alice.com");
        assert_eq!(mt.site_count(), 1);

        // Second access returns same store
        let store2 = mt.get_site_store("alice.com");
        assert!(Arc::ptr_eq(&store, &store2));
        assert_eq!(mt.site_count(), 1);
    }

    #[test]
    fn sites_are_isolated() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());

        let alice = mt.get_site_store("alice.com");
        let bob = mt.get_site_store("bob.com");

        alice.set("key".into(), b"alice-data".to_vec(), None);
        bob.set("key".into(), b"bob-data".to_vec(), None);

        assert_eq!(alice.get("key"), Some(b"alice-data".to_vec()));
        assert_eq!(bob.get("key"), Some(b"bob-data".to_vec()));
    }

    #[test]
    fn empty_hostname_returns_default() {
        let default_arc = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(Arc::clone(&default_arc), test_config());

        let store = mt.get_site_store("");
        assert!(Arc::ptr_eq(&store, &default_arc));
        assert_eq!(mt.site_count(), 0);
    }

    #[test]
    fn remove_site_deletes_store() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());

        mt.get_site_store("temp.com");
        assert_eq!(mt.site_count(), 1);

        assert!(mt.remove_site("temp.com"));
        assert_eq!(mt.site_count(), 0);

        assert!(!mt.remove_site("nonexistent.com"));
    }

    #[test]
    fn auth_site_returns_store() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());

        assert!(mt.auth_site("").is_none());
        assert!(mt.auth_site("site.com").is_some());
        assert_eq!(mt.site_count(), 1);
    }

    #[test]
    fn site_data_not_visible_from_default() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());

        let site = mt.get_site_store("secret.com");
        site.set("password".into(), b"hunter2".to_vec(), None);

        // Default store should NOT see site data
        assert_eq!(mt.default_store().get("password"), None);

        // Another site should NOT see it either
        let other = mt.get_site_store("other.com");
        assert_eq!(other.get("password"), None);
    }
}

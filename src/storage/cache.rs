use crate::storage::tokens::CachedToken;
use std::collections::HashMap;

/// In-memory read cache for installation tokens.
/// Intercepts GetTokenFor commands before they hit the actor channel.
#[derive(Default)]
pub struct ReadCache {
    data: parking_lot::Mutex<HashMap<(String, i64), CachedToken>>,
}

impl Clone for ReadCache {
    fn clone(&self) -> Self {
        let data = self.data.lock();
        Self {
            data: parking_lot::Mutex::new(data.clone()),
        }
    }
}

impl ReadCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if a valid cached token exists. Returns None on miss or expiry.
    pub fn get(&self, identity: &str, installation_id: i64, now_ts: i64) -> Option<CachedToken> {
        let key = (identity.to_string(), installation_id);
        let map = self.data.lock();
        map.get(&key).and_then(|token| {
            // 60s skew margin, same as the DB query logic.
            if token.expires_at - 60 > now_ts {
                Some(token.clone())
            } else {
                None
            }
        })
    }

    /// Write-through cache insert. Called after a successful DB read.
    pub fn put(&self, identity: &str, installation_id: i64, token: CachedToken) {
        let mut map = self.data.lock();
        map.insert((identity.to_string(), installation_id), token);
    }

    /// Invalidate a cache entry. Called after a token update.
    pub fn invalidate(&self, identity: &str, installation_id: i64) {
        let mut map = self.data.lock();
        map.remove(&(identity.to_string(), installation_id));
    }

    /// Invalidate all entries for an installation_id across all identities.
    pub fn invalidate_installation(&self, installation_id: i64) {
        let mut map = self.data.lock();
        map.retain(|(_, id), _| *id != installation_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(exp: i64) -> CachedToken {
        CachedToken {
            token: "tok".to_string(),
            expires_at: exp,
        }
    }

    #[test]
    fn cache_hit_returns_valid_token() {
        let cache = ReadCache::new();
        cache.put("barry", 42, token(2000));
        let result = cache.get("barry", 42, 1000);
        assert!(result.is_some());
        assert_eq!(result.unwrap().token, "tok");
    }

    #[test]
    fn cache_miss_on_expiry() {
        let cache = ReadCache::new();
        cache.put("barry", 42, token(1030)); // expires_at - 60 = 970 < now=1000
        let result = cache.get("barry", 42, 1000);
        assert!(result.is_none());
    }

    #[test]
    fn cache_miss_on_unknown_identity() {
        let cache = ReadCache::new();
        cache.put("barry", 42, token(2000));
        let result = cache.get("other_barry", 42, 1000);
        assert!(result.is_none());
    }

    #[test]
    fn cache_invalidate_removes_entry() {
        let cache = ReadCache::new();
        cache.put("barry", 42, token(2000));
        cache.invalidate("barry", 42);
        let result = cache.get("barry", 42, 1000);
        assert!(result.is_none());
    }

    #[test]
    fn cache_invalidate_installation_removes_all_identities() {
        let cache = ReadCache::new();
        cache.put("barry", 42, token(2000));
        cache.put("other_barry", 42, token(2000));
        cache.invalidate_installation(42);
        assert!(cache.get("barry", 42, 1000).is_none());
        assert!(cache.get("other_barry", 42, 1000).is_none());
    }
}

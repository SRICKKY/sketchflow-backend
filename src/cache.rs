use std::time::Duration;

use redis::{aio::ConnectionManager, AsyncCommands, Client};
use serde::{de::DeserializeOwned, Serialize};

const MAX_CACHE_VALUE_BYTES: usize = 1_048_576;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("Redis command failed: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("Failed to serialize or deserialize cached JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Cache value is {size} bytes; maximum is {max} bytes")]
    ValueTooLarge { size: usize, max: usize },
}

#[derive(Clone, Default)]
pub struct RedisCache {
    connection: Option<ConnectionManager>,
}

impl RedisCache {
    pub async fn connect(redis_url: &str) -> Result<Self, redis::RedisError> {
        let client = Client::open(redis_url)?;
        let connection = ConnectionManager::new(client).await?;
        Ok(Self {
            connection: Some(connection),
        })
    }

    pub async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, CacheError> {
        let Some(connection) = &self.connection else {
            return Ok(None);
        };
        let mut connection = connection.clone();
        let value: Option<String> = connection.get(key).await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(CacheError::from))
            .transpose()
    }

    pub async fn set_json<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl: Duration,
    ) -> Result<(), CacheError> {
        let Some(connection) = &self.connection else {
            return Ok(());
        };
        let value = serde_json::to_string(value)?;
        if value.len() > MAX_CACHE_VALUE_BYTES {
            return Err(CacheError::ValueTooLarge {
                size: value.len(),
                max: MAX_CACHE_VALUE_BYTES,
            });
        }
        let mut connection = connection.clone();
        let _: () = connection.set_ex(key, value, ttl.as_secs()).await?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<(), CacheError> {
        let Some(connection) = &self.connection else {
            return Ok(());
        };
        let mut connection = connection.clone();
        let _: usize = connection.del(key).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::RedisCache;
    use std::time::Duration;

    #[tokio::test]
    async fn disabled_cache_reads_as_miss_and_ignores_writes() {
        let cache = RedisCache::default();
        assert!(cache
            .get_json::<serde_json::Value>("test:key")
            .await
            .unwrap()
            .is_none());
        cache
            .set_json(
                "test:key",
                &serde_json::json!({"ok": true}),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        cache.delete("test:key").await.unwrap();
    }
}

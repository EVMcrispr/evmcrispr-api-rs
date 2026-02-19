use serde::{de::DeserializeOwned, Deserialize, Serialize};
use spin_sdk::key_value::Store;

#[derive(Serialize, Deserialize)]
struct CacheEntry<T> {
    data: T,
    timestamp_ms: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn get_cached<T: DeserializeOwned>(store: &Store, key: &str, ttl_ms: u64) -> Option<T> {
    let entry: CacheEntry<T> = store.get_json(key).ok()??;
    if now_ms().saturating_sub(entry.timestamp_ms) < ttl_ms {
        Some(entry.data)
    } else {
        None
    }
}

pub fn get_stale<T: DeserializeOwned>(store: &Store, key: &str) -> Option<T> {
    let entry: CacheEntry<T> = store.get_json(key).ok()??;
    Some(entry.data)
}

pub fn set_cached<T: Serialize>(store: &Store, key: &str, data: &T) {
    let entry = CacheEntry {
        data,
        timestamp_ms: now_ms(),
    };
    let _ = store.set_json(key, &entry);
}

//! Process-local cache for model catalog responses. The HTTP handler is a
//! comparatively expensive operation because it creates a Pi runtime for each
//! cwd; keeping this cache in Rust avoids rebuilding that runtime on every UI
//! poll while keeping all backend state in the Rust service.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const TTL: Duration = Duration::from_secs(60);
const MAX_ENTRIES: usize = 32;

struct Entry {
    value: Value,
    expires_at: Instant,
    inserted_at: Instant,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    key_locks: HashMap<String, Weak<AsyncMutex<()>>>,
    generation: u64,
}

#[derive(Default)]
pub struct ModelsCache {
    state: Mutex<State>,
}

static CACHE: OnceLock<ModelsCache> = OnceLock::new();

fn cache() -> &'static ModelsCache {
    CACHE.get_or_init(ModelsCache::default)
}

fn lock_state() -> std::sync::MutexGuard<'static, State> {
    cache().state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub async fn lock_key(key: &str) -> OwnedMutexGuard<()> {
    let lock = {
        let mut state = lock_state();
        state.key_locks.retain(|_, lock| lock.strong_count() > 0);
        state.key_locks.get(key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let lock = Arc::new(AsyncMutex::new(()));
            state.key_locks.insert(key.to_owned(), Arc::downgrade(&lock));
            lock
        })
    };
    lock.lock_owned().await
}

pub fn get(key: &str) -> Option<Value> {
    let mut state = lock_state();
    let now = Instant::now();
    state.entries.retain(|_, entry| entry.expires_at > now);
    state.entries.get(key).map(|entry| entry.value.clone())
}

pub fn generation() -> u64 {
    lock_state().generation
}

pub fn insert(key: String, value: Value, generation: u64) {
    let mut state = lock_state();
    if state.generation != generation {
        return;
    }
    let now = Instant::now();
    state.entries.retain(|_, entry| entry.expires_at > now);
    while state.entries.len() >= MAX_ENTRIES && !state.entries.contains_key(&key) {
        let oldest = state.entries.iter().min_by_key(|(_, entry)| entry.inserted_at).map(|(key, _)| key.clone());
        match oldest {
            Some(oldest) => {
                state.entries.remove(&oldest);
            }
            None => break,
        }
    }
    state.entries.insert(key, Entry { value, expires_at: now + TTL, inserted_at: now });
}

pub fn invalidate() {
    let mut state = lock_state();
    state.generation = state.generation.wrapping_add(1);
    state.entries.clear();
}

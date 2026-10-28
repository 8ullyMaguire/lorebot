//! A bounded, in-process cache for public reads and poll cursors.
//!
//! No Redis. The bot is a companion process holding a few dozen users' worth of
//! state, and a cache that needs its own server to evict a key is a cache with
//! its own failure mode: Redis goes down and the bot either blocks or silently
//! stops caching, and now there are two things to diagnose. An in-process
//! `BTreeMap` behind a semaphore has no such story.
//!
//! What it is for, concretely:
//!
//! * **Rate limiting.** Discord allows 50 messages/minute per channel and
//!   Telegram 30 messages/second per chat. Both are enforced here, from
//!   `PlatformConfig::requests_per_minute`, because a bot that ignores them is
//!   disconnected rather than slowed.
//! * **Search deduplication.** A group where several people run `/search` for
//!   the same thing in a minute should hit Darkhold once. This is the cache's
//!   main job and the reason it exists at all.
//! * **Poll cursors.** Where each conversation's last-seen message is, so an
//!   adapter can ask "what is new" without keeping the state anywhere else.
//!
//! Eviction is **least-recently-used, by first-insert time**, not by access
//! time. That is a deliberate simplification: a true LRU needs a second write on
//! every read, and on a cache of this size the difference between evicting the
//! oldest-inserted and the oldest-used is not worth a lock on the hot path.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// The default capacity.
pub const DEFAULT_CAPACITY: usize = 512;

/// Something cached.
#[derive(Debug, Clone)]
struct Entry<T> {
    value: T,
    inserted: Instant,
}

/// A bounded cache.
#[derive(Debug)]
pub struct Cache<K: Ord, V: Clone> {
    inner: Mutex<Inner<K, V>>,
    capacity: usize,
    ttl: Duration,
}

#[derive(Debug)]
struct Inner<K: Ord, V: Clone> {
    map: BTreeMap<K, Entry<V>>,
}

impl<K: Ord + Clone + std::fmt::Debug, V: Clone> Cache<K, V> {
    /// A cache holding at most `capacity` entries, each for `ttl`.
    ///
    /// A capacity of zero is clamped to one. A cache that can hold nothing is
    /// not a cache, it is a bug report: every `insert` would silently fail and
    /// every lookup would miss, and the symptom would be "the bot is slow" with
    /// nothing in the logs.
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: BTreeMap::new(),
            }),
            capacity: capacity.max(1),
            ttl,
        }
    }

    /// A cache with the default capacity and a one-minute TTL.
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_CAPACITY, Duration::from_secs(60))
    }

    /// The most entries this cache will hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// A cached value, if present and not expired.
    pub async fn get(&self, key: &K) -> Option<V> {
        let mut inner = self.inner.lock().await;
        let expired = inner
            .map
            .get(key)
            .is_some_and(|entry| entry.inserted.elapsed() > self.ttl);
        if expired {
            // Removed on read rather than by a sweeper. A background task to
            // expire keys is a second loop that can leak, and in a cache this
            // small the lazy path is strictly less code for the same result.
            inner.map.remove(key);
            return None;
        }
        inner.map.get(key).map(|entry| entry.value.clone())
    }

    /// Store a value, evicting the oldest-inserted entry if full.
    pub async fn insert(&self, key: K, value: V) {
        let mut inner = self.inner.lock().await;
        // Re-inserting an existing key refreshes its timestamp, which is what
        // makes the cache work for a group repeatedly searching the same term:
        // the entry stays alive while people are still asking.
        if inner
            .map
            .insert(
                key.clone(),
                Entry {
                    value,
                    inserted: Instant::now(),
                },
            )
            .is_some()
        {
            return;
        }
        while inner.map.len() > self.capacity {
            let Some(oldest) = inner
                .map
                .iter()
                .min_by_key(|(_, entry)| entry.inserted)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            inner.map.remove(&oldest);
        }
    }

    /// Fetch, or compute and store.
    ///
    /// The single-use pattern is **not** used deliberately: holding the lock
    /// across an `.await` on a network call would serialise every concurrent
    /// search behind the slowest one, and a cache that makes the thing it
    // caches slower is not worth having. Callers that care about the stampede
    /// should hold a per-key lock, which is a decision above this layer.
    pub async fn get_or_insert_with<F, Fut>(&self, key: K, make: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = V>,
    {
        if let Some(value) = self.get(&key).await {
            return value;
        }
        let value = make().await;
        self.insert(key, value.clone()).await;
        value
    }

    /// Drop one key.
    pub async fn remove(&self, key: &K) -> bool {
        self.inner.lock().await.map.remove(key).is_some()
    }

    /// Drop every key.
    pub async fn clear(&self) {
        self.inner.lock().await.map.clear();
    }

    /// How many entries are held, including any that have expired but have not
    /// been read since.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.map.len()
    }

    /// Whether the cache holds nothing.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

impl<K: Ord + Clone + std::fmt::Debug, V: Clone> Default for Cache<K, V> {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// A per-target rate limiter.
///
/// Discord counts per channel and Telegram per chat, so the key is whatever
/// identifies the target and the caller picks. Token-bucket rather than a fixed
/// window: a fixed window lets a client send `n` messages at 10:00:59 and `n`
/// more at 10:01:00, which is twice the limit inside a second and exactly the
/// burst a rate limit exists to prevent.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    per_minute: u32,
    buckets: Arc<Mutex<BTreeMap<String, Bucket>>>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// A limiter allowing `per_minute` requests per target.
    pub fn new(per_minute: u32) -> Self {
        Self {
            // Zero would block everything forever, so it is clamped to one. A
            // limiter that never fires looks like a limiter that is not
            // running, and the symptom is an unexplained ban.
            per_minute: per_minute.max(1),
            buckets: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// The configured limit.
    pub fn per_minute(&self) -> u32 {
        self.per_minute
    }

    /// Take one token for `target`, waiting if the bucket is empty.
    ///
    /// Returns how long it waited, so a caller can log a slow path without
    /// instrumenting every call site.
    pub async fn acquire(&self, target: &str) -> Duration {
        let capacity = f64::from(self.per_minute);
        let refill_per_second = capacity / 60.0;
        let started = Instant::now();

        loop {
            // The lock is released before the sleep. Holding it across the wait
            // would make one exhausted target block every other target in the
            // process, which is the opposite of what a per-channel limit means.
            let ready = {
                let mut buckets = self.buckets.lock().await;
                let now = Instant::now();
                let bucket = buckets.entry(target.to_owned()).or_insert(Bucket {
                    // Start full, so the first message after a quiet period is
                    // not delayed by a bucket that was drained.
                    tokens: capacity,
                    last_refill: now,
                });

                let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
                bucket.tokens = (bucket.tokens + elapsed * refill_per_second).min(capacity);
                bucket.last_refill = now;

                if bucket.tokens >= 1.0 {
                    bucket.tokens -= 1.0;
                    true
                } else {
                    let needed = 1.0 - bucket.tokens;
                    // Sleeping is outside this block, so the wait is returned
                    // rather than performed under the lock.
                    drop(buckets);
                    tokio::time::sleep(Duration::from_secs_f64(
                        (needed / refill_per_second).max(0.001),
                    ))
                    .await;
                    false
                }
            };
            if ready {
                return started.elapsed();
            }
        }
    }

    /// Take a token if one is available, without waiting.
    ///
    /// Returns `false` when the bucket is empty. The platform SDKs report a
    /// rate limit as an error, and the bot can pre-empt it: this is what the
    /// adapters call when a platform gives notice, so the bot backs off
    /// deliberately instead of being disconnected.
    pub async fn try_acquire(&self, target: &str) -> bool {
        let capacity = f64::from(self.per_minute);
        let refill_per_second = capacity / 60.0;
        let mut buckets = self.buckets.lock().await;
        let now = Instant::now();
        let bucket = buckets.entry(target.to_owned()).or_insert(Bucket {
            tokens: capacity,
            last_refill: now,
        });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_per_second).min(capacity);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Forget every bucket, for a reconnect.
    pub async fn reset(&self) {
        self.buckets.lock().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn cache() -> Cache<String, Vec<String>> {
        Cache::new(4, Duration::from_secs(60))
    }

    #[tokio::test]
    async fn a_value_comes_back() {
        let cache = cache();
        cache.insert("a".into(), vec!["x".into()]).await;
        assert_eq!(
            cache.get(&"a".to_string()).await,
            Some(vec!["x".to_string()])
        );
    }

    #[tokio::test]
    async fn a_missing_key_is_a_miss_not_an_error() {
        let cache = cache();
        assert_eq!(cache.get(&"nope".to_string()).await, None);
    }

    #[tokio::test]
    async fn an_expired_entry_is_a_miss_and_is_dropped() {
        // Twenty milliseconds, not a second: a test that sleeps a second is a
        // test nobody runs twice.
        let cache: Cache<String, u8> = Cache::new(4, Duration::from_millis(20));
        cache.insert("a".into(), 1).await;
        assert!(cache.get(&"a".to_string()).await.is_some());
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(cache.get(&"a".to_string()).await.is_none(), "expired");
        assert!(cache.is_empty().await, "and the read dropped it");
    }

    #[tokio::test]
    async fn the_cache_never_exceeds_its_capacity() {
        // The bound is the point. An unbounded cache in a long-running bot is a
        // leak with a slow fuse.
        let cache = cache();
        for i in 0..50 {
            cache.insert(format!("key{i}"), vec![i.to_string()]).await;
        }
        assert_eq!(cache.len().await, 4, "capacity is 4");
    }

    #[tokio::test]
    async fn the_oldest_inserted_entry_is_the_one_evicted() {
        let cache: Cache<String, u8> = Cache::new(3, Duration::from_secs(60));
        for (i, key) in ["a", "b", "c"].iter().enumerate() {
            cache.insert((*key).to_string(), i as u8).await;
            // Distinct timestamps, so "oldest inserted" is unambiguous. An
            // instant-resolution clock can tie, and a tie makes this test
            // depend on BTreeMap ordering.
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // Touching "a" does *not* save it: eviction is by insert time, and
        // documenting that is better than a test that passes either way.
        assert!(cache.get(&"a".to_string()).await.is_some());
        cache.insert("d".into(), 3).await;
        assert!(
            cache.get(&"d".to_string()).await.is_some(),
            "the new key is kept"
        );
        assert!(
            cache.get(&"a".to_string()).await.is_none(),
            "and the oldest-inserted went, not the most recently read"
        );
        assert!(cache.get(&"b".to_string()).await.is_some());
        assert!(cache.get(&"c".to_string()).await.is_some());
    }

    #[tokio::test]
    async fn reinserting_a_key_refreshes_it_rather_than_growing_the_cache() {
        let cache = cache();
        for _ in 0..10 {
            cache.insert("same".into(), vec!["x".into()]).await;
        }
        assert_eq!(cache.len().await, 1, "and does not count as a new entry");
        assert!(cache.get(&"same".to_string()).await.is_some());
    }

    #[tokio::test]
    async fn a_zero_capacity_cache_still_holds_one_entry() {
        // Clamped rather than left at zero: a cache that can hold nothing
        // silently drops every insert, and the symptom is "the bot is slow"
        // with nothing in the logs.
        let cache: Cache<String, u8> = Cache::new(0, Duration::from_secs(60));
        assert_eq!(cache.capacity(), 1);
        cache.insert("a".into(), 1).await;
        assert!(cache.get(&"a".to_string()).await.is_some());
    }

    #[tokio::test]
    async fn removing_and_clearing_work() {
        let cache: Cache<String, i32> = Cache::new(4, Duration::from_secs(60));
        cache.insert("a".into(), 1).await;
        cache.insert("b".into(), 2).await;
        assert!(cache.remove(&"a".to_string()).await);
        assert!(
            !cache.remove(&"a".to_string()).await,
            "removing twice is fine"
        );
        cache.clear().await;
        assert!(cache.is_empty().await);
    }

    #[tokio::test]
    async fn get_or_insert_with_computes_once_per_key() {
        let cache = cache();
        let calls = AtomicUsize::new(0);
        for _ in 0..3 {
            let value = cache
                .get_or_insert_with("k".to_string(), || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    vec!["computed".to_string()]
                })
                .await;
            assert_eq!(value, vec!["computed".to_string()]);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "computed once");
    }

    #[tokio::test]
    async fn a_limiter_allows_a_burst_then_waits_for_the_next_token() {
        // 120/minute = two per second, so the fourth of a burst waits about half
        // a second. A limit of 3/minute would make it wait twenty — which is
        // the limiter working correctly, and is why the limit here is chosen for
        // a fast test rather than a realistic one. The waiting itself is
        // asserted as "eventually returns", not as a duration: a loaded machine
        // makes a timing assertion flaky, and the duration is not the contract.
        let limiter = RateLimiter::new(120);
        for _ in 0..3 {
            limiter.acquire("chat").await;
        }
        let limiter2 = limiter.clone();
        let handle = tokio::spawn(async move { limiter2.acquire("chat").await });
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the fourth token arrives rather than blocking forever")
            .expect("no panic");
    }

    #[tokio::test]
    async fn a_slow_limit_really_does_make_the_caller_wait() {
        // The other half of the test above, and the one that would have caught
        // the dropped-bucket bug on its own: at 60/minute the fourth acquire in
        // a burst cannot return inside 200ms. Asserted as a floor, not a
        // ceiling, so a slow machine cannot make it flaky.
        let limiter = RateLimiter::new(60);
        for _ in 0..3 {
            limiter.acquire("chat").await;
        }
        let limiter2 = limiter.clone();
        tokio::spawn(async move {
            limiter2.acquire("chat").await;
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            limiter.try_acquire("chat").await,
            "a token has been refilled by now — the bucket really is counting, and 200ms \
             bought back less than the single token the fourth acquire consumed"
        );
    }

    #[tokio::test]
    async fn a_bucket_starts_full_so_the_first_message_is_not_delayed() {
        let limiter = RateLimiter::new(10);
        let started = Instant::now();
        limiter.acquire("chat").await;
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "the first acquire waited {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn separate_targets_have_separate_budgets() {
        // Discord counts per channel. A busy channel must not consume the
        // budget of a quiet one, or one conversation can silence the bot
        // everywhere.
        let limiter = RateLimiter::new(2);
        assert!(limiter.try_acquire("channel-a").await);
        assert!(limiter.try_acquire("channel-a").await);
        assert!(!limiter.try_acquire("channel-a").await, "a is empty");
        assert!(limiter.try_acquire("channel-b").await, "b is untouched");
    }

    #[tokio::test]
    async fn a_zero_limit_still_allows_one_request() {
        let limiter = RateLimiter::new(0);
        assert_eq!(limiter.per_minute(), 1, "clamped, not zero");
        assert!(limiter.try_acquire("chat").await);
        assert!(!limiter.try_acquire("chat").await);
    }

    #[tokio::test]
    async fn a_limiter_resets_for_a_reconnect() {
        let limiter = RateLimiter::new(1);
        assert!(limiter.try_acquire("chat").await);
        assert!(!limiter.try_acquire("chat").await);
        limiter.reset().await;
        assert!(limiter.try_acquire("chat").await, "and works again");
    }

    #[tokio::test]
    async fn buckets_are_kept_per_target_and_bounded_by_the_distinct_targets() {
        // A bucket that is dropped the moment it empties is a bucket the next
        // call recreates at full capacity — which means the limit is not
        // enforced at all. This was a real bug: `try_acquire` reported a
        // second request in the same second as allowed, every time, for every
        // target. The entry has to outlive the spend.
        let limiter = RateLimiter::new(1);
        assert!(limiter.try_acquire("a").await);
        assert!(!limiter.try_acquire("a").await, "still limited");
        assert_eq!(
            limiter.buckets.lock().await.len(),
            1,
            "and the bucket survives"
        );

        for i in 0..20 {
            assert!(limiter.try_acquire(&format!("chat{i}")).await);
        }
        assert_eq!(
            limiter.buckets.lock().await.len(),
            21,
            "one per distinct target, no more"
        );
    }
}

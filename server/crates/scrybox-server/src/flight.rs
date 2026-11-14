//! Panic-safe ownership for the single-flight coalescing slots.
//!
//! The search, stats, stats-subscription, collection and collq caches all
//! coalesce concurrent misses the same way: an `inflight: HashMap<String,
//! Arc<OnceCell<..>>>` consulted on every store miss — `slot(key)`, then
//! `get_or_try_init` for the compute, then an explicit `finish(key)`. The
//! explicit finish only ran on the LINEAR path. A panic between the two
//! (an `unwrap` inside the big inline build arms, an assert, anything)
//! unwound past it — tokio's `OnceCell` releases its init permit on unwind,
//! but nothing released the map entry, so the slot key leaked into `inflight`
//! permanently. The leak compounds: search and collection slot keys are
//! revision-stamped, so every write-generation that ever panicked through a
//! compute added another entry no path ever removes.
//!
//! [`slot_guarded`] pairs the slot with an RAII guard that drops the inflight
//! entry at scope exit — unwind included. Removal is pointer-identity
//! checked: a guard firing late can never evict a NEWER compute's slot for
//! the same key, only its own. Semantically this is Node's
//! `delete inflight[key]` in the promise's settle path — which, being a
//! promise `.finally`, also fires on rejection.
//!
//! Node twin: the in-flight promise maps in server.js / collection.js /
//! statsCache.js settle their map entry whether the compute resolved or
//! rejected; pre-guard, the Rust port only settled on resolution.

use std::sync::Arc;

/// The inflight half of a single-flight cache: a map from key to one compute
/// slot, consulted on every store miss. Implemented by SearchCache,
/// StatsCache, CollectionCache and CollQCache; the guard machinery below is
/// written once against this trait.
pub(crate) trait FlightCache {
    /// Payload of the compute slot (`OnceCell<Result<.., ..>>` per cache).
    type Slot;
    /// The coalescing slot for a key (created on first miss).
    fn flight_slot(&self, key: &str) -> Arc<Self::Slot>;
    /// Drop the inflight entry for `key`, but only while it is STILL `slot`
    /// (pointer identity). A guard dropped after its key was already
    /// reclaimed-and-recreated must leave the newer slot alone.
    fn flight_remove(&self, key: &str, slot: &Arc<Self::Slot>);
    /// Test-only: is a slot currently claimed for `key`?
    #[cfg(test)]
    fn flight_len(&self, key: &str) -> usize;
}

/// RAII partner of [`slot_guarded`]: removes the inflight entry when the
/// compute scope ends, including a panic unwinding through it.
pub(crate) struct SlotGuard<C: FlightCache + ?Sized> {
    cache: Arc<C>,
    key: String,
    slot: Arc<C::Slot>,
}

impl<C: FlightCache + ?Sized> Drop for SlotGuard<C> {
    fn drop(&mut self) {
        self.cache.flight_remove(&self.key, &self.slot);
    }
}

/// `flight_slot` plus the guard that guarantees its removal. Call sites hold
/// the slot for `get_or_try_init` and drop the guard exactly where the old
/// explicit `finish(key)` stood; on the panic path the guard drops with the
/// unwinding frame instead.
pub(crate) fn slot_guarded<C: FlightCache + ?Sized>(
    cache: &Arc<C>,
    key: &str,
) -> (Arc<C::Slot>, SlotGuard<C>) {
    let slot = cache.flight_slot(key);
    (
        Arc::clone(&slot),
        SlotGuard { cache: Arc::clone(cache), key: key.to_string(), slot },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use std::collections::HashMap;
    use std::panic::AssertUnwindSafe;
    use std::sync::Mutex;
    use tokio::sync::OnceCell;

    /// Minimal stand-in exercising the guard mechanics in isolation.
    struct TestCache {
        inflight: Mutex<HashMap<String, Arc<OnceCell<Result<u32, String>>>>>,
    }
    impl TestCache {
        fn new() -> Self {
            Self { inflight: Mutex::new(HashMap::new()) }
        }
        fn len(&self, key: &str) -> usize {
            self.inflight.lock().unwrap().contains_key(key) as usize
        }
    }
    impl FlightCache for TestCache {
        type Slot = OnceCell<Result<u32, String>>;
        fn flight_slot(&self, key: &str) -> Arc<Self::Slot> {
            self.inflight.lock().unwrap().entry(key.to_string()).or_default().clone()
        }
        fn flight_remove(&self, key: &str, slot: &Arc<Self::Slot>) {
            let mut e = self.inflight.lock().unwrap();
            if e.get(key).is_some_and(|cur| Arc::ptr_eq(cur, slot)) {
                e.remove(key);
            }
        }
        fn flight_len(&self, key: &str) -> usize {
            self.len(key)
        }
    }

    #[tokio::test]
    async fn guard_drops_slot_on_scope_exit() {
        let cache = Arc::new(TestCache::new());
        {
            let (_slot, _g) = slot_guarded(&cache, "k");
            assert_eq!(cache.len("k"), 1, "slot created on miss");
        }
        assert_eq!(cache.len("k"), 0, "guard must reclaim on normal drop");
    }

    #[tokio::test]
    async fn guard_drops_slot_when_compute_panics() {
        let cache = Arc::new(TestCache::new());
        let cache2 = Arc::clone(&cache);
        // Inline catch_unwind drive (same shape as the m1/JobGuard tests):
        // the guard's drop glue must strictly precede the asserts.
        let res = AssertUnwindSafe(async move {
            let (slot, _g) = slot_guarded(&cache2, "k");
            let _ = slot
                .get_or_try_init(|| async {
                    Err::<Result<u32, String>, String>(panic!("boom"))
                })
                .await;
        })
        .catch_unwind()
        .await;
        assert!(res.is_err(), "the compute must panic for this to test anything");
        assert_eq!(cache.len("k"), 0, "panicked compute must not leak the inflight slot");
        // And the key is reusable: a fresh compute stores cleanly afterwards.
        let (slot, g) = slot_guarded(&cache, "k");
        assert_eq!(
            *slot.get_or_try_init(|| async { Ok::<_, String>(Ok(7u32)) }).await.unwrap(),
            Ok(7)
        );
        drop(g);
        assert_eq!(cache.len("k"), 0);
    }

    #[tokio::test]
    async fn guard_never_evicts_a_newer_slot_for_the_same_key() {
        let cache = Arc::new(TestCache::new());
        let (slot, old_guard) = slot_guarded(&cache, "k");
        slot.set(Ok(1)).unwrap();
        // Key reclaimed (e.g. by the explicit drop at the old finish point)
        // and a NEWER compute claims it…
        cache.flight_remove("k", &slot);
        let fresh = cache.flight_slot("k");
        // …then the stale guard fires late.
        drop(old_guard);
        assert!(
            Arc::ptr_eq(&cache.flight_slot("k"), &fresh),
            "a late guard must not evict the newer slot"
        );
    }

    /// The four production caches, driven through the generic helpers: same
    /// panic-safety and pointer-identity guarantees on the real slot types.
    /// (The panic unwinds with the guard alive rather than inside a cell
    /// init — the cell's permit release is tokio's job, tested by the
    /// TestCache arm above where the concrete value type is known.)
    async fn panic_safety<C: FlightCache + Default + 'static>(name: &'static str) {
        let cache = Arc::new(C::default());
        let cache2 = Arc::clone(&cache);
        let res = AssertUnwindSafe(async move {
            let (_slot, _g) = slot_guarded(&cache2, "k");
            panic!("boom");
        })
        .catch_unwind()
        .await;
        assert!(res.is_err(), "{name}: the guarded body must panic");
        assert_eq!(cache.flight_len("k"), 0, "{name}: panicked scope leaked the slot");
        // Normal path: guard drop reclaims.
        let (_slot, g) = slot_guarded(&cache, "k2");
        drop(g);
        assert_eq!(cache.flight_len("k2"), 0, "{name}: normal drop must reclaim");
        // Late guard: key reclaimed while the guard is still alive, a newer
        // compute claims the key, then the stale guard fires.
        let (slot, late) = slot_guarded(&cache, "k3");
        cache.flight_remove("k3", &slot); // the explicit-drop point settles first
        let fresh = cache.flight_slot("k3");
        drop(late);
        assert!(
            Arc::ptr_eq(&cache.flight_slot("k3"), &fresh),
            "{name}: late guard evicted a newer slot"
        );
    }

    #[tokio::test]
    async fn production_caches_are_guard_safe() {
        panic_safety::<crate::search::SearchCache>("search").await;
        panic_safety::<crate::stats::StatsCache>("stats").await;
        panic_safety::<crate::collection::CollectionCache>("collection").await;
        panic_safety::<crate::collq::CollQCache>("collq").await;
    }
}

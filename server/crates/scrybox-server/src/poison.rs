//! Poison-tolerant locking (REVIEW-errors-maint M2).
//!
//! `std::sync::Mutex` poisons when a panic unwinds while the guard is held,
//! and every later `lock().unwrap()` panics too — one bug becomes a permanent
//! outage (worst case: the DB pool's idle list, which took down every
//! DB-backed route). Every cache/counter/state map behind a std Mutex or
//! RwLock in this crate locks through the extension trait below instead:
//! a panic under the guard recovers the data (`PoisonError::into_inner`)
//! rather than poisoning every subsequent caller.
//!
//! This matches the pattern the scheduler already used
//! (`unwrap_or_else(PoisonError::into_inner)`), and is equivalent in
//! behaviour to parking_lot's non-poisoning mutex without a new dependency.
//! All of this state is caches, counters and job state (never a
//! multi-invariant transactional structure), so recovering the inner value
//! is always safe: at worst a cached entry is one revision stale and the
//! revision-keyed caches revalidate on the next read.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// `lock().unwrap()` / `read().unwrap()` / `write().unwrap()` that never
/// panic on a poisoned lock — the guard is recovered from the poison instead.
pub trait LockExt {
    /// Type of the guard returned by `guard()` (and the read guard for
    /// `RwLock`, which is the overwhelmingly common access pattern).
    type Guard<'a>
    where
        Self: 'a;

    /// Lock for exclusive access (Mutex) or for reading (RwLock), recovering
    /// from poisoning rather than propagating the original panic.
    fn guard(&self) -> Self::Guard<'_>;
}

impl<T> LockExt for Mutex<T> {
    type Guard<'a> = MutexGuard<'a, T> where T: 'a;
    fn guard(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T> LockExt for RwLock<T> {
    type Guard<'a> = RwLockReadGuard<'a, T> where T: 'a;
    fn guard(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Exclusive access to an `RwLock`, recovering from poisoning rather than
/// panicking. Call sites are single-writer-per-state maps; unlike the plain
/// `.write().unwrap()` there is no deadlock-vs-unlock-upgrade subtlety here
/// because none of our writers take the read path while holding the write
/// guard (RwLock is not reentrant — do not introduce that pattern).
pub fn write<'a, T>(l: &'a RwLock<T>) -> RwLockWriteGuard<'a, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static MUTEX: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    static RWLOCK: RwLock<Vec<u32>> = RwLock::new(Vec::new());
    static PANICS_BEFORE_LOCK: AtomicUsize = AtomicUsize::new(0);

    fn poison_mutex_via_panic() {
        // Poison a lock while HELD, WITHOUT mutating: the panic unwinds
        // through the guard, so a `.lock().unwrap()` caller would see
        // "poisoned" forever after. (No mutation: poisoning is a property
        // of the mutex, not of the data, and other unit tests share this
        // mutex — they run in parallel.)
        let panic_now = PANICS_BEFORE_LOCK.fetch_sub(1, Ordering::SeqCst) > 0;
        let _g = MUTEX.guard();
        if panic_now {
            panic!("simulated panic while holding the lock");
        }
    }

    #[test]
    fn mutex_guard_survives_a_poisoning_panic() {
        {
            let mut g = MUTEX.guard();
            g.push(1); // baseline content, held normally
        }
        PANICS_BEFORE_LOCK.store(1, Ordering::SeqCst);
        let h = std::thread::spawn(poison_mutex_via_panic);
        assert!(h.join().is_err(), "precondition: the poisoning thread must panic");

        // Pre-fix (`.lock().unwrap()`) this line panicked with "mutex is poisoned".
        let mut g = MUTEX.guard();
        g.push(2);
        assert_eq!(&*g, &[1, 2], "poisoned guard must be recovered, not lost");
        drop(g);
        // And locking keeps working after repeated poisoning.
        PANICS_BEFORE_LOCK.store(1, Ordering::SeqCst);
        let h = std::thread::spawn(poison_mutex_via_panic);
        assert!(h.join().is_err());
        let g = MUTEX.guard();
        assert!(g.contains(&1) && g.contains(&2), "state remains usable");
    }

    #[test]
    fn rwlock_guard_and_write_survive_poisoning() {
        {
            let mut w = write(&RWLOCK);
            w.push(1);
        }
        let h = std::thread::spawn(|| {
            let _w = write(&RWLOCK);
            panic!("simulated panic while holding the write guard");
        });
        assert!(h.join().is_err());

        // Pre-fix (`.read().unwrap()` / `.write().unwrap()`) both panicked.
        assert!(RWLOCK.guard().contains(&1));
        write(&RWLOCK).push(2);
        assert!(RWLOCK.guard().contains(&2));
    }
}

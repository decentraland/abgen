use std::collections::HashMap;
use std::hash::Hash;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex};

struct Call<V> {
    result: Mutex<Option<V>>,
    ready: Condvar,
    #[cfg(test)]
    waiters: std::sync::atomic::AtomicUsize,
}

pub(crate) struct Group<K, V> {
    calls: Mutex<HashMap<K, Arc<Call<V>>>>,
}

impl<K, V> Group<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone + Default,
{
    pub(crate) fn new() -> Self {
        Self {
            calls: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn run(&self, key: K, work: impl FnOnce() -> V) -> V {
        let (call, leader) = {
            let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(call) = calls.get(&key) {
                (Arc::clone(call), false)
            } else {
                let call = Arc::new(Call {
                    result: Mutex::new(None),
                    ready: Condvar::new(),
                    #[cfg(test)]
                    waiters: std::sync::atomic::AtomicUsize::new(0),
                });
                calls.insert(key.clone(), Arc::clone(&call));
                (call, true)
            }
        };

        if !leader {
            #[cfg(test)]
            call.waiters
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut result = call.result.lock().unwrap_or_else(|e| e.into_inner());
            while result.is_none() {
                result = call.ready.wait(result).unwrap_or_else(|e| e.into_inner());
            }
            let result = result
                .as_ref()
                .expect("completed call has a result")
                .clone();
            #[cfg(test)]
            call.waiters
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return result;
        }

        let outcome = catch_unwind(AssertUnwindSafe(work));
        let published = outcome.as_ref().map_or_else(|_| V::default(), Clone::clone);
        {
            let mut result = call.result.lock().unwrap_or_else(|e| e.into_inner());
            *result = Some(published);
            call.ready.notify_all();
        }
        {
            let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
            if calls
                .get(&key)
                .is_some_and(|active| Arc::ptr_eq(active, &call))
            {
                calls.remove(&key);
            }
        }

        match outcome {
            Ok(result) => result,
            Err(payload) => resume_unwind(payload),
        }
    }

    #[cfg(test)]
    pub(crate) fn waiter_count(&self, key: &K) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .map_or(0, |call| {
                call.waiters.load(std::sync::atomic::Ordering::SeqCst)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;

    fn contend<T>(group: Arc<Group<u8, Option<T>>>, work: impl Fn() -> Option<T> + Sync)
    where
        T: Clone + Send + Eq + std::fmt::Debug + 'static,
    {
        const THREADS: usize = 12;
        let start = Arc::new(Barrier::new(THREADS));
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..THREADS {
                let group = Arc::clone(&group);
                let start = Arc::clone(&start);
                let work = &work;
                handles.push(scope.spawn(move || {
                    start.wait();
                    group.run(7, work)
                }));
            }
            let mut handles = handles.into_iter();
            let expected = handles.next().unwrap().join().unwrap();
            for handle in handles {
                assert_eq!(handle.join().unwrap(), expected);
            }
        });
    }

    #[test]
    fn contention_executes_success_once() {
        let group = Arc::new(Group::new());
        let observer = Arc::clone(&group);
        let calls = AtomicUsize::new(0);
        contend(group, || {
            calls.fetch_add(1, Ordering::SeqCst);
            while observer.waiter_count(&7) != 11 {
                std::thread::yield_now();
            }
            Some(42)
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn contention_wakes_waiters_on_failure_and_allows_retry() {
        let group = Arc::new(Group::new());
        let observer = Arc::clone(&group);
        let calls = AtomicUsize::new(0);
        contend(Arc::clone(&group), || {
            calls.fetch_add(1, Ordering::SeqCst);
            while observer.waiter_count(&7) != 11 {
                std::thread::yield_now();
            }
            None::<u32>
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(group.run(7, || Some(9)), Some(9));
    }

    #[test]
    fn panic_wakes_waiters_and_does_not_poison_the_key() {
        let group = Arc::new(Group::<u8, Option<u32>>::new());
        let owner = Arc::clone(&group);
        let observer = Arc::clone(&group);
        let started = Arc::new(Barrier::new(2));
        let owner_started = Arc::clone(&started);
        let handle = std::thread::spawn(move || {
            owner.run(3, || {
                owner_started.wait();
                while observer.waiter_count(&3) != 1 {
                    std::thread::yield_now();
                }
                panic!("intentional leader panic");
            })
        });
        started.wait();
        let waiter_group = Arc::clone(&group);
        let waiter = std::thread::spawn(move || {
            waiter_group.run(3, || panic!("waiter must not execute work"))
        });
        assert!(handle.join().is_err());
        assert_eq!(waiter.join().unwrap(), None);
        assert_eq!(group.run(3, || Some(11)), Some(11));
    }
}

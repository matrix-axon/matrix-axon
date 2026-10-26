//! Active-operation capacity isolated by account; unused registry entries expire.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use uuid::Uuid;
// Weak entries are pruned under the registry lock; only active sends/waiters
// keep capacity alive. A slow homeserver cannot consume another account's slots.
#[derive(Default)]
pub struct AccountSlots(Mutex<HashMap<Uuid, Weak<tokio::sync::Semaphore>>>);

impl AccountSlots {
    pub fn for_account(&self, account: Uuid) -> Arc<tokio::sync::Semaphore> {
        let mut slots = self.0.lock().expect("preview capacity registry poisoned");
        slots.retain(|_, value| value.strong_count() > 0);
        slots
            .entry(account)
            .or_default()
            .upgrade()
            .unwrap_or_else(|| {
                let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
                slots.insert(account, Arc::downgrade(&semaphore));
                semaphore
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upload_capacity_is_isolated_by_account_and_idle_entries_expire() {
        let slots = AccountSlots::default();
        let account = Uuid::new_v4();
        let first = slots.for_account(account);
        let held = (0..4)
            .map(|_| first.clone().try_acquire_owned().unwrap())
            .collect::<Vec<_>>();
        assert!(slots.for_account(account).try_acquire_owned().is_err());
        let other = slots
            .for_account(Uuid::new_v4())
            .try_acquire_owned()
            .unwrap();
        assert_eq!(slots.0.lock().unwrap().len(), 2);
        drop(held);
        drop(first);
        drop(other);
        let current = slots.for_account(Uuid::new_v4());
        assert_eq!(slots.0.lock().unwrap().len(), 1);
        assert_eq!(current.available_permits(), 4);
    }
}

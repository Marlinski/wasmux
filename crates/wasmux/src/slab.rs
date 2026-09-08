//! A minimal index-keyed store with a free list.
//!
//! Every shared kernel object (an open file description, a pipe, a process) lives in one of
//! these and is referred to by index. That is what keeps the kernel free of `Rc`, `RefCell`
//! and atomics: sharing is an index plus a reference count, exactly as a real kernel does it,
//! and the whole structure stays `Send` whenever its contents are.

/// A slot index into a [`Slab`]. Distinct slabs use distinct newtypes over this.
pub(crate) type Key = u32;

pub(crate) struct Slab<T> {
    slots: Vec<Option<T>>,
    free: Vec<Key>,
}

impl<T> Slab<T> {
    pub(crate) fn new() -> Self {
        Slab {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    /// Store `value`, returning its key. `None` if the slab is at `cap`.
    pub(crate) fn insert(&mut self, value: T, cap: usize) -> Option<Key> {
        if let Some(k) = self.free.pop() {
            if let Some(slot) = self.slots.get_mut(k as usize) {
                *slot = Some(value);
                return Some(k);
            }
            return None;
        }
        if self.slots.len() >= cap || self.slots.len() >= Key::MAX as usize {
            return None;
        }
        let k = self.slots.len() as Key;
        self.slots.push(Some(value));
        Some(k)
    }

    pub(crate) fn get(&self, k: Key) -> Option<&T> {
        self.slots.get(k as usize).and_then(Option::as_ref)
    }

    pub(crate) fn get_mut(&mut self, k: Key) -> Option<&mut T> {
        self.slots.get_mut(k as usize).and_then(Option::as_mut)
    }

    pub(crate) fn remove(&mut self, k: Key) -> Option<T> {
        let slot = self.slots.get_mut(k as usize)?;
        let value = slot.take();
        if value.is_some() {
            self.free.push(k);
        }
        value
    }

    /// Number of live entries.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn len(&self) -> usize {
        self.slots.len().saturating_sub(self.free.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_freed_slots_and_respects_cap() {
        let mut s: Slab<u8> = Slab::new();
        let a = s.insert(1, 2).unwrap();
        let b = s.insert(2, 2).unwrap();
        assert!(s.insert(3, 2).is_none(), "cap must be enforced");
        assert_eq!(s.len(), 2);
        assert_eq!(s.remove(a), Some(1));
        assert_eq!(s.remove(a), None, "double remove must not free twice");
        assert_eq!(s.len(), 1);
        let c = s.insert(3, 2).unwrap();
        assert_eq!(c, a, "the freed slot is reused");
        assert_eq!(s.get(b), Some(&2));
        assert_eq!(s.get(99), None);
    }
}

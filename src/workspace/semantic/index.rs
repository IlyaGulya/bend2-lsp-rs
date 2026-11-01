use std::{
    borrow::Borrow,
    collections::{HashMap, hash_map},
    hash::Hash,
    iter::FusedIterator,
    slice,
};

const SMALL_INDEX_LIMIT: usize = 8;

/// Build with the existing hash table, then retain exact-size entries for small
/// immutable views. Large views keep their original constant-time hash lookup.
pub(super) enum CompactMap<K, V> {
    Small(Box<[(K, V)]>),
    Hashed(HashMap<K, V>),
}

impl<K, V> Default for CompactMap<K, V> {
    fn default() -> Self {
        Self::Hashed(HashMap::new())
    }
}

impl<K, V> CompactMap<K, V> {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn freeze(self) -> Self {
        match self {
            Self::Hashed(map) if map.len() <= SMALL_INDEX_LIMIT => {
                let mut entries = Vec::with_capacity(map.len());
                entries.extend(map);
                Self::Small(entries.into_boxed_slice())
            }
            index => index,
        }
    }

    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = (&K, &V)> + FusedIterator {
        match self {
            Self::Small(entries) => CompactIter::Small(entries.iter()),
            Self::Hashed(map) => CompactIter::Hashed(map.iter()),
        }
    }

    fn building(&mut self) -> &mut HashMap<K, V> {
        match self {
            Self::Hashed(map) => map,
            Self::Small(_) => unreachable!("published semantic indexes are immutable"),
        }
    }

    pub(super) fn into_building(self) -> HashMap<K, V> {
        match self {
            Self::Hashed(map) => map,
            Self::Small(_) => unreachable!("published semantic indexes are immutable"),
        }
    }
}

impl<K: Eq + Hash, V> CompactMap<K, V> {
    pub(super) fn get<Q: Eq + Hash + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        match self {
            Self::Small(entries) => entries
                .iter()
                .find_map(|entry| (Borrow::<Q>::borrow(&entry.0) == key).then_some(&entry.1)),
            Self::Hashed(map) => map.get(key),
        }
    }

    pub(super) fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.building().insert(key, value)
    }

    pub(super) fn reserve(&mut self, additional: usize) {
        self.building().reserve(additional);
    }
}

impl<K, V> From<HashMap<K, V>> for CompactMap<K, V> {
    fn from(map: HashMap<K, V>) -> Self {
        Self::Hashed(map).freeze()
    }
}

enum CompactIter<'a, K, V> {
    Small(slice::Iter<'a, (K, V)>),
    Hashed(hash_map::Iter<'a, K, V>),
}

impl<'a, K, V> Iterator for CompactIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Small(entries) => entries.next().map(|(key, value)| (key, value)),
            Self::Hashed(entries) => entries.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}

impl<K, V> ExactSizeIterator for CompactIter<'_, K, V> {
    fn len(&self) -> usize {
        match self {
            Self::Small(entries) => entries.len(),
            Self::Hashed(entries) => entries.len(),
        }
    }
}

impl<K, V> FusedIterator for CompactIter<'_, K, V> {}

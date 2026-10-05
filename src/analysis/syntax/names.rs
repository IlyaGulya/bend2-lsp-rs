use std::{
    collections::{
        HashMap,
        hash_map::{DefaultHasher, Entry},
    },
    hash::{Hash, Hasher},
};

use super::{NameId, TextRange, TokenId};

const KEYWORDS: &[&str] = &[
    "def", "law", "type", "is", "match", "case", "do", "return", "for", "exs", "where", "import",
    "as",
];

#[derive(Debug, Eq, PartialEq)]
struct NameCandidates {
    first: NameId,
    collisions: Vec<NameId>,
}

impl NameCandidates {
    fn iter(&self) -> impl Iterator<Item = NameId> + '_ {
        std::iter::once(self.first).chain(self.collisions.iter().copied())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct NameTable {
    pub(super) ranges: Vec<TextRange>,
    lookup: HashMap<u64, NameCandidates>,
    token_names: Vec<Option<NameId>>,
}
impl NameTable {
    pub(super) fn new() -> Self {
        Self {
            ranges: Vec::new(),
            lookup: HashMap::new(),
            token_names: Vec::new(),
        }
    }

    pub(super) fn intern(&mut self, source: &str, range: TextRange, _token: TokenId) -> NameId {
        let id = self.intern_name(source, range);
        self.token_names.push(Some(id));
        id
    }

    pub(super) fn intern_name(&mut self, source: &str, range: TextRange) -> NameId {
        let text = &source[range.start..range.end];
        let entry = self.lookup.entry(hash_name(text));
        if let Entry::Occupied(existing) = &entry {
            for candidate in existing.get().iter() {
                let range = self.ranges[candidate.0];
                if source[range.start..range.end] == *text {
                    return candidate;
                }
            }
        }
        let id = NameId(self.ranges.len());
        self.ranges.push(range);
        match entry {
            Entry::Occupied(mut existing) => existing.get_mut().collisions.push(id),
            Entry::Vacant(vacant) => {
                vacant.insert(NameCandidates {
                    first: id,
                    collisions: Vec::new(),
                });
            }
        }
        id
    }

    pub(super) fn append_non_name(&mut self) {
        self.token_names.push(None);
    }

    pub(super) fn find(&self, source: &str, text: &str) -> Option<NameId> {
        let candidates = self.lookup.get(&hash_name(text))?;
        candidates.iter().find(|candidate| {
            let range = self.ranges[candidate.0];
            source[range.start..range.end] == *text
        })
    }

    pub(super) fn range(&self, id: NameId) -> TextRange {
        self.ranges[id.0]
    }

    pub(super) fn for_token(&self, id: TokenId) -> Option<NameId> {
        self.token_names.get(id.0).copied().flatten()
    }
}

pub(super) fn keyword_name_flags(source: &str, names: &NameTable) -> Vec<bool> {
    let mut flags = vec![false; names.ranges.len()];
    for keyword in KEYWORDS {
        if let Some(name) = names.find(source, keyword) {
            flags[name.0] = true;
        }
    }
    flags
}

fn hash_name(name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

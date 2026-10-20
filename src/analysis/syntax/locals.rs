use std::{collections::BinaryHeap, iter::FusedIterator};

use super::{BindingId, IndexedBinding, NameId, TextRange, compact_groups};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Visibility {
    binding: BindingId,
    range: TextRange,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct LocalIndex {
    visibility: Box<[Visibility]>,
    source_indices: Box<[usize]>,
    name_spans: Box<[TextRange]>,
    max_end: Box<[usize]>,
}

impl LocalIndex {
    pub fn build(bindings: &[IndexedBinding], name_count: usize) -> Self {
        let (binding_indices, mut name_spans) = compact_groups(bindings, name_count, |binding| {
            (!binding.anonymous && binding.scope.start < binding.scope.end)
                .then_some(binding.name.0)
        });
        let mut visibility: Vec<Visibility> = Vec::with_capacity(bindings.len());
        let mut events = Vec::new();
        let mut active = Vec::new();
        let mut heap = BinaryHeap::<usize>::new();
        for span in &mut name_spans {
            let indices = &binding_indices[span.start..span.end];
            if indices
                .windows(2)
                .all(|pair| bindings[pair[0]].scope.end <= bindings[pair[1]].scope.start)
            {
                // Most names occur in independent declaration scopes. Their
                // existing source order already is the complete visibility map;
                // reserve the boundary sweep for actual shadowing/overlap.
                let first = visibility.len();
                visibility.extend(indices.iter().map(|index| Visibility {
                    binding: bindings[*index].binding_id,
                    range: bindings[*index].scope,
                }));
                *span = TextRange::new(first, visibility.len());
                continue;
            }
            if active.is_empty() {
                active.resize(bindings.len(), false);
            }
            events.clear();
            for &index in indices {
                events.extend([index * 2, index * 2 + 1]);
            }
            if !events
                .iter()
                .is_sorted_by_key(|event| event_offset(bindings, *event))
            {
                events.sort_unstable_by_key(|event| event_offset(bindings, *event));
            }
            let first_visibility = visibility.len();
            let mut position = 0;
            while position < events.len() {
                let offset = event_offset(bindings, events[position]);
                // Apply the complete boundary before selecting the half-open winner.
                while position < events.len() && event_offset(bindings, events[position]) == offset
                {
                    let event = events[position];
                    let binding = bindings[event / 2].binding_id.0;
                    let start = event.is_multiple_of(2);
                    active[binding] = start;
                    if start {
                        heap.push(binding);
                    }
                    position += 1;
                }
                while heap.peek().is_some_and(|binding| !active[*binding]) {
                    let _ = heap.pop();
                }
                if position < events.len()
                    && let Some(&binding) = heap.peek()
                {
                    let range = TextRange::new(offset, event_offset(bindings, events[position]));
                    if let Some(previous) = visibility.last_mut()
                        && previous.binding.0 == binding
                        && previous.range.end == offset
                    {
                        previous.range.end = range.end;
                    } else {
                        visibility.push(Visibility {
                            binding: BindingId(binding),
                            range,
                        });
                    }
                }
            }
            heap.clear();
            *span = TextRange::new(first_visibility, visibility.len());
        }
        // Tree pruning needs nondecreasing starts; equal-start intervals need no tie-break.
        let mut source_indices: Vec<_> = (0..visibility.len()).collect();
        if !visibility
            .iter()
            .is_sorted_by_key(|entry| entry.range.start)
        {
            source_indices.sort_unstable_by_key(|index| {
                let entry = visibility[*index];
                entry.range.start
            });
        }
        let mut max_end = vec![0; visibility.len()];
        build_tree(
            &visibility,
            &source_indices,
            &mut max_end,
            0,
            visibility.len(),
        );
        Self {
            visibility: visibility.into_boxed_slice(),
            source_indices: source_indices.into_boxed_slice(),
            name_spans: name_spans.into_boxed_slice(),
            max_end: max_end.into_boxed_slice(),
        }
    }

    pub fn lookup(&self, name: NameId, offset: usize) -> Option<BindingId> {
        let span = self.name_spans.get(name.0)?;
        let entries = &self.visibility[span.start..span.end];
        let position = entries
            .partition_point(|entry| entry.range.start <= offset)
            .checked_sub(1)?;
        let entry = entries[position];
        entry.range.contains(offset).then_some(entry.binding)
    }

    pub fn visible<'a>(
        &'a self,
        bindings: &'a [IndexedBinding],
        offset: usize,
    ) -> VisibleBindings<'a> {
        let mut stack = [TextRange::new(0, 0); usize::BITS as usize];
        stack[0] = TextRange::new(0, self.visibility.len());
        VisibleBindings {
            index: self,
            bindings,
            offset,
            stack,
            depth: 1,
        }
    }
}

fn event_offset(bindings: &[IndexedBinding], event: usize) -> usize {
    let scope = bindings[event / 2].scope;
    if event.is_multiple_of(2) {
        scope.start
    } else {
        scope.end
    }
}

fn build_tree(
    entries: &[Visibility],
    source_indices: &[usize],
    max_end: &mut [usize],
    start: usize,
    end: usize,
) -> usize {
    if start == end {
        return 0;
    }
    let middle = start + (end - start) / 2;
    let left = build_tree(entries, source_indices, max_end, start, middle);
    let right = build_tree(entries, source_indices, max_end, middle + 1, end);
    let maximum = entries[source_indices[middle]]
        .range
        .end
        .max(left)
        .max(right);
    max_end[middle] = maximum;
    maximum
}

pub(super) struct VisibleBindings<'a> {
    index: &'a LocalIndex,
    bindings: &'a [IndexedBinding],
    offset: usize,
    stack: [TextRange; usize::BITS as usize],
    depth: usize,
}

impl<'a> Iterator for VisibleBindings<'a> {
    type Item = &'a IndexedBinding;

    fn next(&mut self) -> Option<Self::Item> {
        while self.depth > 0 {
            self.depth -= 1;
            let span = self.stack[self.depth];
            if span.start == span.end {
                continue;
            }
            let middle = span.start + (span.end - span.start) / 2;
            if self.index.max_end[middle] <= self.offset {
                continue;
            }
            let entry = self.index.visibility[self.index.source_indices[middle]];
            if entry.range.start <= self.offset && middle + 1 < span.end {
                self.stack[self.depth] = TextRange::new(middle + 1, span.end);
                self.depth += 1;
            }
            if span.start < middle {
                self.stack[self.depth] = TextRange::new(span.start, middle);
                self.depth += 1;
            }
            if entry.range.contains(self.offset) {
                return Some(&self.bindings[entry.binding.0]);
            }
        }
        None
    }
}

impl FusedIterator for VisibleBindings<'_> {}

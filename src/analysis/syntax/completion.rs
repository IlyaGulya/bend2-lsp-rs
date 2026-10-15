use super::{CasePatternType, NameId, SyntaxIndex, TextRange, TokenId};

/// Pattern boundaries are emitted by the grammar, including incomplete editor input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PatternSite {
    pub range: TextRange,
    pub expected_type: Option<TextRange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PatternContext {
    range: TextRange,
    expected_type: Option<TextRange>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct CompletionIndex {
    patterns: Box<[PatternContext]>,
    imports: Box<[TextRange]>,
    cases: Box<[(TextRange, TextRange)]>,
}

impl CompletionIndex {
    pub(super) fn build(
        source: &str,
        syntax: &SyntaxIndex,
        sites: Vec<PatternSite>,
        imports: Vec<TextRange>,
    ) -> Self {
        let mut patterns = Vec::with_capacity(sites.len());
        let mut cases = Vec::with_capacity(sites.len());
        for site in sites {
            let context_start = patterns.len();
            patterns.push(PatternContext {
                range: site.range,
                expected_type: site
                    .expected_type
                    .and_then(|range| simple_type(source, range)),
            });
            nested_contexts(source, syntax, site.range, &mut patterns);
            let context_end = patterns.len();
            patterns[context_start..context_end].sort_unstable_by_key(|context| {
                (context.range.start, std::cmp::Reverse(context.range.end))
            });
            cases.push((site.range, TextRange::new(context_start, context_end)));
        }
        cases.sort_unstable_by_key(|(range, _)| range.start);
        Self {
            patterns: patterns.into_boxed_slice(),
            imports: imports.into_boxed_slice(),
            cases: cases.into_boxed_slice(),
        }
    }

    pub(super) fn pattern_type(&self, offset: usize) -> Option<CasePatternType> {
        let case = self
            .cases
            .partition_point(|(range, _)| range.start <= offset)
            .checked_sub(1)?;
        let (range, contexts) = self.cases[case];
        if offset > range.end {
            return None;
        }
        self.patterns[contexts.start..contexts.end]
            .iter()
            .rev()
            .find(|context| context.range.start <= offset && offset <= context.range.end)
            .map(|context| CasePatternType {
                explicit_type: context.expected_type,
            })
    }

    pub(super) fn import_path(&self, offset: usize) -> Option<TextRange> {
        let index = self
            .imports
            .partition_point(|range| range.start <= offset)
            .checked_sub(1)?;
        let range = self.imports[index];
        (offset <= range.end).then_some(range)
    }
}

fn simple_type(source: &str, range: TextRange) -> Option<TextRange> {
    let raw = &source[range.start..range.end];
    let text = raw.trim();
    let end = text
        .find(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, '_' | '.')
        })
        .unwrap_or(text.len());
    let start = range.start + raw.len() - raw.trim_start().len();
    (end > 0).then_some(TextRange::new(start, start + end))
}

fn constructor_field(
    source: &str,
    syntax: &SyntaxIndex,
    name: NameId,
    field: usize,
) -> Option<TextRange> {
    let constructor = syntax.constructor_by_name(name)?;
    let symbol = syntax.symbol_by_id(constructor.symbol)?;
    let annotation = syntax.parameters(symbol).get(field)?.annotation?;
    simple_type(source, annotation)
}

/// Build nested slots once from tokens inside grammar-owned pattern boundaries.
/// Constructor field annotations come from telescopes, never declaration-text scans.
fn nested_contexts(
    source: &str,
    syntax: &SyntaxIndex,
    range: TextRange,
    patterns: &mut Vec<PatternContext>,
) {
    let start = syntax
        .tokens
        .partition_point(|token| token.range.start < range.start);
    let limit = syntax
        .tokens
        .partition_point(|token| token.range.start < range.end);
    for index in start..limit {
        let token = syntax.tokens[index];
        if !matches!(syntax.token_text(source, TokenId(index)), Some("(" | "{")) {
            continue;
        }
        let Some(name_index) = index.checked_sub(1) else {
            continue;
        };
        let Some(reference) = syntax.reference_for_token(TokenId(name_index)) else {
            continue;
        };
        let name = reference.full_name;
        let close = syntax
            .delimiter_close(TokenId(index))
            .map_or(range.end, |id| {
                syntax.tokens[id.0].range.start.min(range.end)
            });
        let mut field = 0;
        let mut argument_start = token.range.end;
        for separator in index + 1..limit {
            let current = syntax.tokens[separator];
            if current.range.start >= close {
                break;
            }
            if syntax.delimiter_context[separator].value() == Some(index)
                && syntax.token_text(source, TokenId(separator)) == Some(",")
            {
                patterns.push(PatternContext {
                    range: TextRange::new(argument_start, current.range.start),
                    expected_type: constructor_field(source, syntax, name, field),
                });
                field += 1;
                argument_start = current.range.end;
            }
        }
        patterns.push(PatternContext {
            range: TextRange::new(argument_start, close),
            expected_type: constructor_field(source, syntax, name, field),
        });
    }
}

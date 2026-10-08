use super::{CasePatternType, NameId, SyntaxIndex, TextRange, TokenId};

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
    pub(super) fn build(source: &str, syntax: &SyntaxIndex) -> Self {
        if ["import", "match", "case"]
            .iter()
            .all(|keyword| syntax.name_id(source, keyword).is_none())
        {
            return Self::default();
        }
        let mut patterns = Vec::new();
        let mut imports = Vec::new();
        let mut matches = Vec::new();
        let mut cases = Vec::new();
        for (line_number, line) in syntax.lines.iter().enumerate() {
            let code = &source[line.code.start..line.code.end];
            if code.is_empty() || code.starts_with('#') {
                continue;
            }
            while matches
                .last()
                .is_some_and(|(indent, _)| line.indent <= *indent)
            {
                matches.pop();
            }
            if !["import", "match", "case"].iter().any(|keyword| {
                code.strip_prefix(*keyword)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            }) {
                continue;
            }
            let first = syntax
                .tokens
                .partition_point(|token| token.range.start < line.code.start);
            let limit = syntax
                .tokens
                .partition_point(|token| token.range.start < line.content.end);
            if first == limit {
                continue;
            }
            let text = syntax.token_text(source, TokenId(first));
            if text == Some("import")
                && line.indent == 0
                && let Some(path) =
                    import_context(source, syntax.tokens[first].range.end, line.code.end)
            {
                imports.push(path);
            }
            if text == Some("match") {
                let expected = syntax
                    .symbol_for_token(TokenId(first + 1))
                    .and_then(|id| syntax.binding_type_range(source, id))
                    .and_then(|range| simple_type(source, range));
                matches.push((line.indent, expected));
            }
            if text != Some("case") {
                continue;
            }
            let (limit, fallback_end) = pattern_limit(source, syntax, line_number, first, limit);
            let end = (first + 1..limit)
                .find(|index| {
                    syntax.delimiter_context[*index] == syntax.delimiter_context[first]
                        && syntax.token_text(source, TokenId(*index)) == Some(":")
                })
                .map_or(fallback_end, |index| syntax.tokens[index].range.start);
            let start = syntax.tokens[first].range.end;
            let context_start = patterns.len();
            let expected_type = matches.last().and_then(|(_, expected)| *expected);
            patterns.push(PatternContext {
                range: TextRange::new(start, end),
                expected_type,
            });
            nested_contexts(source, syntax, first + 1, limit, end, &mut patterns);
            let context_end = patterns.len();
            patterns[context_start..context_end].sort_unstable_by_key(|context| {
                (context.range.start, std::cmp::Reverse(context.range.end))
            });
            cases.push((
                TextRange::new(start, end),
                TextRange::new(context_start, context_end),
            ));
        }
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

fn import_context(source: &str, start: usize, end: usize) -> Option<TextRange> {
    let rest = &source[start..end];
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let start = start + rest.len() - rest.trim_start().len();
    let end = source[start..end]
        .find(char::is_whitespace)
        .map_or(end, |width| start + width);
    Some(TextRange::new(start, end))
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

fn constructor_fields(source: &str, syntax: &SyntaxIndex, name: NameId) -> Vec<Option<TextRange>> {
    let Some(constructor) = syntax.constructor_by_name(name) else {
        return Vec::new();
    };
    let start = syntax
        .tokens
        .partition_point(|token| token.range.start < constructor.name_range.end);
    let limit = syntax
        .tokens
        .partition_point(|token| token.range.start < constructor.range.end);
    let mut fields = Vec::new();
    for index in start..limit {
        if syntax.token_text(source, TokenId(index)) != Some(":") {
            continue;
        }
        let type_start = index + 1;
        let type_end = (type_start..limit)
            .find(|next| matches!(syntax.token_text(source, TokenId(*next)), Some("," | "}")))
            .unwrap_or(limit);
        fields.push(
            syntax
                .tokens
                .get(type_start)
                .zip(
                    type_end
                        .checked_sub(1)
                        .and_then(|last| syntax.tokens.get(last)),
                )
                .filter(|(first, last)| first.range.start <= last.range.end)
                .and_then(|(first, last)| {
                    simple_type(source, TextRange::new(first.range.start, last.range.end))
                }),
        );
    }
    fields
}

fn pattern_limit(
    source: &str,
    syntax: &SyntaxIndex,
    line_number: usize,
    first: usize,
    limit: usize,
) -> (usize, usize) {
    let line = syntax.lines[line_number];
    let extends = (first + 1..limit).any(|index| {
        matches!(syntax.token_text(source, TokenId(index)), Some("(" | "{"))
            && syntax
                .delimiter_close(TokenId(index))
                .is_none_or(|close| syntax.tokens[close.0].range.end > line.content.end)
    });
    if !extends {
        return (limit, line.content.end);
    }
    let end = syntax.lines[line_number + 1..]
        .iter()
        .find(|next| {
            let code = source[next.code.start..next.code.end].trim_start();
            next.indent <= line.indent && !code.is_empty() && !code.starts_with(['#', ')', '}'])
        })
        .map_or(source.len(), |next| next.content.start);
    (
        syntax
            .tokens
            .partition_point(|token| token.range.start < end),
        end,
    )
}

fn nested_contexts(
    source: &str,
    syntax: &SyntaxIndex,
    start: usize,
    limit: usize,
    end: usize,
    patterns: &mut Vec<PatternContext>,
) {
    for index in start..limit {
        let token = syntax.tokens[index];
        if token.range.start >= end {
            break;
        }
        if !matches!(syntax.token_text(source, TokenId(index)), Some("(" | "{")) {
            continue;
        }
        let Some(name_index) = index.checked_sub(1) else {
            continue;
        };
        let chain_start = super::qualified_chain_start(source, &syntax.tokens, name_index);
        let name_range = TextRange::new(
            syntax.tokens[chain_start].range.start,
            syntax.tokens[name_index].range.end,
        );
        let Some(name) = syntax.name_id(source, &source[name_range.start..name_range.end]) else {
            continue;
        };
        let fields = constructor_fields(source, syntax, name);
        let close = syntax
            .delimiter_close(TokenId(index))
            .map_or(end, |id| syntax.tokens[id.0].range.start.min(end));
        let mut field = 0;
        let mut argument_start = token.range.end;
        for separator in index + 1..limit {
            let current = syntax.tokens[separator];
            if current.range.start >= close {
                break;
            }
            if syntax.delimiter_context[separator] == Some(TokenId(index))
                && syntax.token_text(source, TokenId(separator)) == Some(",")
            {
                patterns.push(PatternContext {
                    range: TextRange::new(argument_start, current.range.start),
                    expected_type: fields.get(field).copied().flatten(),
                });
                field += 1;
                argument_start = current.range.end;
            }
        }
        patterns.push(PatternContext {
            range: TextRange::new(argument_start, close),
            expected_type: fields.get(field).copied().flatten(),
        });
    }
}

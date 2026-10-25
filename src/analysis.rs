#[path = "analysis/syntax.rs"]
mod syntax;

pub use syntax::{
    BindingId, CallSite, CasePatternType, DeclarationKind, DeclarationSite, DiagnosticKind,
    IndexedBinding, IndexedConstructor, IndexedImport, IndexedReference, IndexedScope,
    IndexedSymbol, NameId, NameSegment, Reference, ReferenceKind, ReferenceRole, ScopeId, SymbolId,
    SyntaxDiagnostic, SyntaxIndex, Token, TokenFlags, TokenId, TokenKind,
};

pub type Import = IndexedImport;
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

impl TextRange {
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    #[must_use]
    pub const fn contains(self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Revision(pub i32);

impl Revision {
    pub const UNVERSIONED: Self = Self(-1);
}

const UTF16_CHECKPOINT_STRIDE: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Utf16Checkpoint {
    byte_offset: usize,
    units: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineIndex {
    line_starts: Vec<usize>,
    ascii_lines: Vec<u64>,
    checkpoints: Vec<Utf16Checkpoint>,
}

impl LineIndex {
    #[must_use]
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        let mut ascii_lines = vec![u64::MAX];
        for (offset, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(offset + 1);
                if (line_starts.len() - 1) % u64::BITS as usize == 0 {
                    ascii_lines.push(u64::MAX);
                }
            } else if !byte.is_ascii() {
                let line = line_starts.len() - 1;
                ascii_lines[line / u64::BITS as usize] &= !(1_u64 << (line % u64::BITS as usize));
            }
        }
        Self::from_parts(source, line_starts, ascii_lines)
    }

    fn from_parts(source: &str, line_starts: Vec<usize>, ascii_lines: Vec<u64>) -> Self {
        let mut index = Self {
            line_starts,
            ascii_lines,
            checkpoints: Vec::new(),
        };
        for line in 0..index.line_starts.len() {
            if index.is_ascii_line(line) {
                continue;
            }
            let start = index.line_starts[line];
            let end = index.line_content_end(source, line);
            let mut units = 0;
            for (count, (offset, scalar)) in source[start..end].char_indices().enumerate() {
                units += scalar.len_utf16();
                if (count + 1) % UTF16_CHECKPOINT_STRIDE == 0 {
                    index.checkpoints.push(Utf16Checkpoint {
                        byte_offset: start + offset + scalar.len_utf8(),
                        units,
                    });
                }
            }
        }
        index
    }

    #[must_use]
    pub fn offset(&self, source: &str, line: u32, character: u32) -> usize {
        let Ok(line) = usize::try_from(line) else {
            return source.len();
        };
        let Some(start) = self.line_starts.get(line).copied() else {
            return source.len();
        };
        let end = self.line_content_end(source, line);
        let requested = usize::try_from(character).unwrap_or(usize::MAX);
        let content = &source[start..end];
        let mut units = 0usize;
        for (offset, scalar) in content.char_indices() {
            let scalar_units = scalar.len_utf16();
            if units.saturating_add(scalar_units) > requested {
                return start + offset;
            }
            units += scalar_units;
        }
        end
    }

    #[must_use]
    pub fn position(&self, source: &str, offset: usize) -> (u32, u32) {
        let mut offset = offset.min(source.len());
        while !source.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self
            .line_starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        let start = self.line_starts[line];
        let end = self.line_content_end(source, line);
        let offset = offset.min(end);
        let character = if self.is_ascii_line(line) {
            offset - start
        } else {
            let before = self
                .checkpoints
                .partition_point(|checkpoint| checkpoint.byte_offset <= offset);
            let checkpoint = before
                .checked_sub(1)
                .and_then(|index| self.checkpoints.get(index))
                .filter(|checkpoint| checkpoint.byte_offset >= start);
            let (checkpoint_offset, units) = checkpoint.map_or((start, 0), |checkpoint| {
                (checkpoint.byte_offset, checkpoint.units)
            });
            units + source[checkpoint_offset..offset].encode_utf16().count()
        };
        (
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(character).unwrap_or(u32::MAX),
        )
    }

    #[must_use]
    pub fn line_range(&self, source: &str, line: usize) -> Option<TextRange> {
        let start = *self.line_starts.get(line)?;
        Some(TextRange::new(start, self.line_content_end(source, line)))
    }

    fn is_ascii_line(&self, line: usize) -> bool {
        self.ascii_lines
            .get(line / u64::BITS as usize)
            .is_some_and(|word| word & (1_u64 << (line % u64::BITS as usize)) != 0)
    }

    fn line_content_end(&self, source: &str, line: usize) -> usize {
        let start = self.line_starts[line];
        let mut end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(source.len());
        if end > start && source.as_bytes()[end - 1] == b'\n' {
            end -= 1;
            if end > start && source.as_bytes()[end - 1] == b'\r' {
                end -= 1;
            }
        }
        end
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct DocumentSnapshot {
    pub revision: Revision,
    pub text: String,
    pub line_index: LineIndex,
    pub syntax: SyntaxIndex,
    content_hash: u64,
}

impl DocumentSnapshot {
    #[must_use]
    pub fn new(revision: Revision, text: String) -> Self {
        let (line_index, syntax) = SyntaxIndex::build(&text);
        let content_hash = source_content_hash(&text);
        Self {
            revision,
            text,
            line_index,
            syntax,
            content_hash,
        }
    }

    #[must_use]
    pub fn with_line_index(revision: Revision, text: String, line_index: LineIndex) -> Self {
        let syntax = SyntaxIndex::build_with_line_index(&text, &line_index);
        let content_hash = source_content_hash(&text);
        Self {
            revision,
            text,
            line_index,
            syntax,
            content_hash,
        }
    }

    /// Stable FNV-1a64 of the original UTF-8 bytes, cached during snapshot construction.
    ///
    /// This is source provenance, not a cryptographic or semantic identity.
    #[must_use]
    pub const fn content_hash(&self) -> u64 {
        self.content_hash
    }
}

fn source_content_hash(source: &str) -> u64 {
    source.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolKind {
    Function,
    Struct,
    Constructor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub detail: String,
    pub kind: SymbolKind,
    pub range: TextRange,
    pub selection_range: TextRange,
    pub children: Vec<Self>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionKind {
    Function,
    Struct,
    Keyword,
    Variable,
    Constructor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    pub label: String,
    pub detail: String,
    pub kind: CompletionKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InlayHint {
    pub position: usize,
    pub parameter_name: TextRange,
}

impl InlayHint {
    #[must_use]
    pub fn label(&self, source: &str) -> String {
        let name = &source[self.parameter_name.start..self.parameter_name.end];
        let mut label = String::with_capacity(name.len() + 2);
        label.push_str(name);
        label.push_str(": ");
        label
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FoldingRange {
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionRange {
    pub range: TextRange,
    pub parent: Option<Box<Self>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticToken {
    pub range: TextRange,
    pub token_type: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignatureHelp {
    pub label: String,
    pub parameters: Vec<String>,
    pub active_parameter: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSymbol<'a> {
    pub name: &'a str,
    pub kind: SymbolKind,
    pub range: TextRange,
}

#[must_use]
pub fn imports(snapshot: &DocumentSnapshot) -> &[Import] {
    snapshot.syntax.imports()
}

#[must_use]
pub fn inlay_hints(snapshot: &DocumentSnapshot, range: TextRange) -> Vec<InlayHint> {
    snapshot.syntax.parameter_hints(range).collect()
}

#[must_use]
pub fn folding_ranges(snapshot: &DocumentSnapshot) -> Vec<FoldingRange> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let mut output = Vec::<FoldingRange>::new();
    let mut active = Vec::<(usize, usize)>::new();
    let mut previous = None;

    for (index, (code_range, indent)) in syntax.folding_lines().enumerate() {
        let code = &source[code_range.start..code_range.end];
        let blank = if snapshot.line_index.is_ascii_line(index) {
            // char::is_whitespace includes vertical tab; is_ascii_whitespace does not.
            code.bytes()
                .all(|byte| matches!(byte, b' ' | b'\t'..=b'\r'))
        } else {
            code.chars().all(char::is_whitespace)
        };
        if blank {
            continue;
        }
        if let Some((last_line, last_indent)) = previous {
            while active
                .last()
                .is_some_and(|(_, parent_indent)| indent <= *parent_indent)
            {
                let Some((range, _)) = active.pop() else {
                    break;
                };
                output[range].end_line = last_line;
            }
            if last_indent < indent {
                active.push((output.len(), last_indent));
                output.push(FoldingRange {
                    start_line: last_line,
                    end_line: index,
                });
            }
        }
        previous = Some((index, indent));
    }
    if let Some((last_line, _)) = previous {
        for (range, _) in active {
            output[range].end_line = last_line;
        }
    }
    output
}

#[must_use]
pub fn selection_range(snapshot: &DocumentSnapshot, mut offset: usize) -> SelectionRange {
    let source = &snapshot.text;
    offset = offset.min(source.len());
    while !source.is_char_boundary(offset) {
        offset -= 1;
    }
    let line_number =
        usize::try_from(snapshot.line_index.position(source, offset).0).unwrap_or_default();
    let line = SelectionRange {
        range: snapshot
            .syntax
            .line_content_range(line_number)
            .unwrap_or_default(),
        parent: Some(Box::new(SelectionRange {
            range: TextRange::new(0, source.len()),
            parent: None,
        })),
    };
    let syntax = &snapshot.syntax;
    let Some(token_id) = syntax.token_at_or_before(offset) else {
        return line;
    };
    let Some(mut token) = syntax.token(token_id).copied() else {
        return line;
    };
    if token.range.start == offset
        && !matches!(token.kind, TokenKind::Identifier | TokenKind::Number)
        && token_id.0 > 0
    {
        let previous_id = TokenId(token_id.0 - 1);
        if let Some(previous) = syntax.token(previous_id)
            && previous.range.end == offset
            && matches!(previous.kind, TokenKind::Identifier | TokenKind::Number)
        {
            token = *previous;
        }
    }
    if !matches!(token.kind, TokenKind::Identifier | TokenKind::Number)
        || offset < token.range.start
        || offset > token.range.end
    {
        return line;
    }
    SelectionRange {
        range: token.range,
        parent: Some(Box::new(line)),
    }
}

#[must_use]
pub fn semantic_tokens(snapshot: &DocumentSnapshot) -> Vec<SemanticToken> {
    let syntax = &snapshot.syntax;
    let mut result = Vec::with_capacity(syntax.semantic_token_count());
    for token in syntax.tokens() {
        let token_type = token.flags.semantic_type();
        if let Some(token_type) = token_type {
            result.push(SemanticToken {
                range: token.range,
                token_type,
            });
        }
    }
    result
}

#[must_use]
pub fn signature_help(snapshot: &DocumentSnapshot, offset: usize) -> Option<SignatureHelp> {
    let syntax = &snapshot.syntax;
    let (_, active_parameter, _, callee) = syntax.active_call(&snapshot.text, offset)?;
    let declaration = syntax.symbol_for_token(callee)?;
    signature_help_for_symbol(
        snapshot,
        declaration,
        usize::try_from(active_parameter).unwrap_or(usize::MAX),
    )
}

/// Formats a previously resolved signature site without re-parsing its written header.
///
/// The caller chooses the effective law site when a workspace semantic view associates
/// a definition with a law; the definition's own written metadata remains unchanged.
#[must_use]
pub fn signature_help_for_symbol(
    snapshot: &DocumentSnapshot,
    id: SymbolId,
    active_parameter: usize,
) -> Option<SignatureHelp> {
    let syntax = &snapshot.syntax;
    let declaration = syntax.symbol_by_id(id)?;
    if declaration.kind != SymbolKind::Function {
        return None;
    }
    let metadata = declaration_metadata(snapshot, id)?;
    let parameters: Vec<String> = syntax
        .parameters(declaration)
        .iter()
        .map(|parameter| {
            snapshot.text[parameter.full_range.start..parameter.full_range.end].to_owned()
        })
        .collect();
    Some(SignatureHelp {
        label: snapshot.text[metadata.signature_range.start..metadata.signature_range.end]
            .to_owned(),
        active_parameter: active_parameter.min(parameters.len().saturating_sub(1)),
        parameters,
    })
}

/// Exact, prefix, and subsequence matching without allocating candidate strings.
/// Lower tuples rank first; ties are resolved by scope and then label by the caller.
#[must_use]
pub fn completion_match(label: &str, query: &str) -> Option<(u8, usize, usize)> {
    if label == query {
        return Some((0, 0, label.len()));
    }
    if label.starts_with(query) {
        return Some((1, 0, label.len()));
    }
    subsequence_completion_match(label, query)
}

fn completion_matches(label: &str, query: &str, ascii: bool) -> bool {
    // `starts_with` already compares lengths on its fast path, so the explicit
    // length test only guards the subsequence fallback instead of every label.
    if label.starts_with(query) {
        return true;
    }
    label.len() >= query.len()
        && (if ascii {
            ascii_subsequence_matches(label, query)
        } else {
            subsequence_completion_match(label, query).is_some()
        })
}

fn ascii_subsequence_matches(label: &str, query: &str) -> bool {
    let mut characters = label.bytes();
    for wanted in query.bytes() {
        // Folding only the ASCII case bit is sufficient for an alphabetic
        // wanted byte, and cannot turn punctuation or UTF-8 bytes into a match.
        let case_bit = u8::from(wanted.is_ascii_alphabetic()) << 5;
        let wanted = wanted | case_bit;
        if !characters.any(|character| character | case_bit == wanted) {
            return false;
        }
    }
    true
}

fn subsequence_completion_match(label: &str, query: &str) -> Option<(u8, usize, usize)> {
    let mut wanted = query.chars();
    let mut next = wanted.next();
    let mut first = 0;
    let mut matched = 0;
    for (index, character) in label.chars().enumerate() {
        if next.is_some_and(|target| character.eq_ignore_ascii_case(&target)) {
            if matched == 0 {
                first = index;
            }
            matched += 1;
            next = wanted.next();
            if next.is_none() {
                return Some((
                    if first == 0 && index + 1 == matched {
                        2
                    } else {
                        3
                    },
                    first + index + 1 - matched,
                    label.len(),
                ));
            }
        }
    }
    None
}
#[must_use]
pub fn completion_items(snapshot: &DocumentSnapshot, prefix: &str) -> Vec<Completion> {
    let ascii_prefix = prefix.is_ascii();
    let mut items = collect_module_completions(snapshot, prefix, ascii_prefix);
    append_keyword_completions(&mut items, prefix, ascii_prefix);
    items
}

/// Complete visible local bindings and declarations at a snapshot-local cursor.
#[must_use]
pub fn scoped_completion_items(
    snapshot: &DocumentSnapshot,
    offset: usize,
    prefix: &str,
) -> Vec<Completion> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let ascii_prefix = prefix.is_ascii();
    let mut local_names: Vec<NameId> = syntax
        .bindings_at(offset)
        .filter_map(|binding| {
            completion_matches(syntax.name_text(source, binding.name), prefix, ascii_prefix)
                .then_some(binding.name)
        })
        .collect();
    local_names.sort_unstable_by_key(|name| name.0);
    local_names.dedup();
    let mut items = Vec::new();
    if !local_names.is_empty() {
        items.extend(local_names.iter().map(|name| Completion {
            label: syntax.name_text(source, *name).to_owned(),
            detail: "Local binding".into(),
            kind: CompletionKind::Variable,
        }));
        items.sort_unstable_by(|left, right| left.label.cmp(&right.label));
    }
    for symbol in syntax.symbols() {
        let name = &source[symbol.name_range.start..symbol.name_range.end];
        if symbol.kind != SymbolKind::Constructor
            && completion_matches(name, prefix, ascii_prefix)
            && (local_names.is_empty()
                || local_names
                    .binary_search_by_key(&symbol.name.0, |name| name.0)
                    .is_err())
        {
            items.push(completion(
                name.to_owned(),
                source[symbol.detail_range.start..symbol.detail_range.end].to_owned(),
                symbol.kind,
            ));
        }
    }
    append_keyword_completions(&mut items, prefix, ascii_prefix);
    items
}

const COMPLETION_KEYWORDS: [&str; 12] = [
    "def", "type", "law", "match", "case", "do", "return", "for", "exs", "where", "import", "as",
];

const MAX_COMPLETION_KEYWORD_BYTES: usize = {
    let mut maximum = 0;
    let mut index = 0;
    while index < COMPLETION_KEYWORDS.len() {
        let length = COMPLETION_KEYWORDS[index].len();
        if length > maximum {
            maximum = length;
        }
        index += 1;
    }
    maximum
};

fn append_keyword_completions(items: &mut Vec<Completion>, prefix: &str, ascii_prefix: bool) {
    if prefix.len() > MAX_COMPLETION_KEYWORD_BYTES {
        return;
    }
    let mut missing = 0_u16;
    for (index, keyword) in COMPLETION_KEYWORDS.into_iter().enumerate() {
        if completion_matches(keyword, prefix, ascii_prefix) {
            missing |= 1 << index;
        }
    }
    if missing == 0 {
        return;
    }
    // Check existing labels once, rather than rescanning all declarations for
    // every matching keyword. The match also skips non-keyword label lengths.
    for item in items.iter() {
        let index = match item.label.as_str() {
            "def" => 0,
            "type" => 1,
            "law" => 2,
            "match" => 3,
            "case" => 4,
            "do" => 5,
            "return" => 6,
            "for" => 7,
            "exs" => 8,
            "where" => 9,
            "import" => 10,
            "as" => 11,
            _ => continue,
        };
        missing &= !(1 << index);
    }
    for (index, keyword) in COMPLETION_KEYWORDS.into_iter().enumerate() {
        if missing & (1 << index) != 0 {
            items.push(Completion {
                label: keyword.into(),
                detail: "Bend keyword".into(),
                kind: CompletionKind::Keyword,
            });
        }
    }
}

/// Complete indexed ADT constructors, optionally within a qualified namespace.
#[must_use]
pub fn constructor_completion_items(
    snapshot: &DocumentSnapshot,
    qualifier: Option<&str>,
    prefix: &str,
) -> Vec<Completion> {
    typed_constructor_completion_items(snapshot, qualifier, prefix, None)
}

/// Restrict constructors only after the caller resolves a known explicit type.
#[must_use]
pub fn typed_constructor_completion_items(
    snapshot: &DocumentSnapshot,
    qualifier: Option<&str>,
    prefix: &str,
    expected_type: Option<&str>,
) -> Vec<Completion> {
    let syntax = &snapshot.syntax;
    let source = &snapshot.text;
    let ascii_prefix = prefix.is_ascii();
    let mut items = Vec::new();
    for parent in syntax.symbols() {
        if parent.kind != SymbolKind::Struct
            || expected_type
                .is_some_and(|expected| syntax.name_text(source, parent.name) != expected)
        {
            continue;
        }
        for constructor in syntax.constructors(parent.id) {
            let name = syntax.name_text(source, constructor.name);
            let label = match qualifier {
                Some(qualifier) => name
                    .strip_prefix(qualifier)
                    .and_then(|name| name.strip_prefix('.')),
                None => Some(name),
            };
            if let Some(label) =
                label.filter(|label| completion_matches(label, prefix, ascii_prefix))
            {
                let parent_name = syntax.name_text(source, parent.name);
                let mut detail = String::with_capacity("constructor of ".len() + parent_name.len());
                detail.push_str("constructor of ");
                detail.push_str(parent_name);
                items.push(Completion {
                    label: label.to_owned(),
                    detail,
                    kind: CompletionKind::Constructor,
                });
            }
        }
    }
    items
}

pub fn keyword_completions(prefix: &str) -> impl Iterator<Item = Completion> + '_ {
    COMPLETION_KEYWORDS
        .into_iter()
        .filter(move |keyword| completion_matches(keyword, prefix, prefix.is_ascii()))
        .map(|keyword| Completion {
            label: keyword.into(),
            detail: "Bend keyword".into(),
            kind: CompletionKind::Keyword,
        })
}

#[must_use]
pub fn module_completion_items(snapshot: &DocumentSnapshot, prefix: &str) -> Vec<Completion> {
    collect_module_completions(snapshot, prefix, prefix.is_ascii())
}

fn collect_module_completions(
    snapshot: &DocumentSnapshot,
    prefix: &str,
    ascii_prefix: bool,
) -> Vec<Completion> {
    if prefix.len() > snapshot.syntax.max_module_completion_bytes() {
        return Vec::new();
    }
    match (ascii_prefix, snapshot.syntax.has_constructor_declarations()) {
        (true, false) => collect_module_completions_for_query::<true, false>(snapshot, prefix),
        (true, true) => collect_module_completions_for_query::<true, true>(snapshot, prefix),
        (false, false) => collect_module_completions_for_query::<false, false>(snapshot, prefix),
        (false, true) => collect_module_completions_for_query::<false, true>(snapshot, prefix),
    }
}

// Select matching and constructor filtering once for the complete query.
#[inline(never)]
fn collect_module_completions_for_query<const ASCII_PREFIX: bool, const HAS_CONSTRUCTORS: bool>(
    snapshot: &DocumentSnapshot,
    prefix: &str,
) -> Vec<Completion> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let mut items = Vec::new();
    for symbol in syntax.symbols() {
        let name = &source[symbol.name_range.start..symbol.name_range.end];
        if !completion_matches(name, prefix, ASCII_PREFIX) {
            continue;
        }
        // Derive the completion kind from the same match that excludes
        // constructors: they are not module completions, so their labels and
        // details are not materialized, and the kind is read only once.
        let kind = match symbol.kind {
            SymbolKind::Function => CompletionKind::Function,
            SymbolKind::Struct => CompletionKind::Struct,
            SymbolKind::Constructor if HAS_CONSTRUCTORS => continue,
            SymbolKind::Constructor => CompletionKind::Constructor,
        };
        items.push(Completion {
            label: name.to_owned(),
            detail: source[symbol.detail_range.start..symbol.detail_range.end].to_owned(),
            kind,
        });
    }
    items
}

#[must_use]
pub fn qualified_completion_items(
    snapshot: &DocumentSnapshot,
    qualifier: &str,
    prefix: &str,
) -> Vec<Completion> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let ascii_prefix = prefix.is_ascii();
    syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.kind != SymbolKind::Constructor)
        .filter_map(|symbol| {
            let name = &source[symbol.name_range.start..symbol.name_range.end];
            let label = name.strip_prefix(qualifier)?.strip_prefix('.')?;
            completion_matches(label, prefix, ascii_prefix).then(|| {
                let label = label.to_owned();
                completion(
                    label,
                    source[symbol.detail_range.start..symbol.detail_range.end].to_owned(),
                    symbol.kind,
                )
            })
        })
        .collect()
}

fn completion(label: String, detail: String, symbol_kind: SymbolKind) -> Completion {
    let kind = match symbol_kind {
        SymbolKind::Function => CompletionKind::Function,
        SymbolKind::Constructor => CompletionKind::Constructor,
        SymbolKind::Struct => CompletionKind::Struct,
    };
    Completion {
        label,
        detail,
        kind,
    }
}

#[must_use]
pub fn identifier_ranges(snapshot: &DocumentSnapshot, name: &str) -> Vec<TextRange> {
    let syntax = &snapshot.syntax;
    let Some(name) = syntax.name_id(&snapshot.text, name) else {
        return Vec::new();
    };
    if let Some(symbol) = syntax.symbol_id_by_name(name) {
        return syntax
            .references(symbol)
            .map(|reference| reference.range)
            .collect();
    }
    syntax
        .references_named(name)
        .map(|reference| reference.range)
        .collect()
}

/// Normalize a case-insensitive symbol query once for all indexed documents.
pub struct WorkspaceSymbolQuery {
    normalized: String,
}

impl WorkspaceSymbolQuery {
    #[must_use]
    pub fn new(query: &str) -> Self {
        Self {
            normalized: query.to_lowercase(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.normalized.is_empty()
    }

    pub fn symbols<'a>(
        &'a self,
        snapshot: &'a DocumentSnapshot,
    ) -> impl Iterator<Item = WorkspaceSymbol<'a>> + 'a {
        let source = &snapshot.text;
        let syntax = &snapshot.syntax;
        let is_empty = self.is_empty();
        syntax.symbols().iter().filter_map(move |symbol| {
            if symbol.kind == SymbolKind::Constructor {
                return None;
            }
            let name = &source[symbol.name_range.start..symbol.name_range.end];
            let matches = is_empty
                || if name.is_ascii() {
                    // ASCII names need no allocated lowercase copy. Keep Unicode
                    // names on str's full (including contextual) lowercase path.
                    name.as_bytes()
                        .windows(self.normalized.len())
                        .any(|window| window.eq_ignore_ascii_case(self.normalized.as_bytes()))
                } else {
                    name.to_lowercase().contains(&self.normalized)
                };
            if !matches {
                return None;
            }
            Some(WorkspaceSymbol {
                name,
                kind: symbol.kind,
                range: symbol.name_range,
            })
        })
    }
}

#[must_use]
pub fn declaration_range(snapshot: &DocumentSnapshot, name: &str) -> Option<TextRange> {
    declaration_metadata_by_name(snapshot, name).map(|metadata| metadata.name_range)
}

#[must_use]
pub fn type_declaration_range(snapshot: &DocumentSnapshot, name: &str) -> Option<TextRange> {
    let source = &snapshot.text;
    snapshot
        .syntax
        .declaration_sites()
        .iter()
        .find(|site| {
            site.kind == DeclarationKind::Type
                && snapshot.syntax.name_text(source, site.name) == name
        })
        .map(|site| site.name_range)
}

#[must_use]
pub fn declaration_hover(snapshot: &DocumentSnapshot, name: &str) -> Option<String> {
    let metadata = declaration_metadata_by_name(snapshot, name)?;
    declaration_hover_by_id(snapshot, metadata.symbol)
}

/// Returns exact written declaration metadata in this immutable snapshot.
#[must_use]
pub fn declaration_metadata(snapshot: &DocumentSnapshot, id: SymbolId) -> Option<&DeclarationSite> {
    snapshot.syntax.declaration_site(id)
}

/// Name-based convenience lookup; semantic identity consumers must pass a resolved site ID.
#[must_use]
pub fn declaration_metadata_by_name<'a>(
    snapshot: &'a DocumentSnapshot,
    name: &str,
) -> Option<&'a DeclarationSite> {
    let syntax = &snapshot.syntax;
    let name = syntax.name_id(&snapshot.text, name)?;
    let declaration = syntax.symbol_by_name(name)?;
    declaration_metadata(snapshot, declaration.id)
}

/// Returns the original attached comment block, preserving Unicode and line endings.
#[must_use]
pub fn declaration_documentation(snapshot: &DocumentSnapshot, id: SymbolId) -> Option<&str> {
    let range = declaration_metadata(snapshot, id)?.doc_range?;
    Some(&snapshot.text[range.start..range.end])
}

#[must_use]
pub fn declaration_hover_by_id(snapshot: &DocumentSnapshot, id: SymbolId) -> Option<String> {
    let metadata = declaration_metadata(snapshot, id)?;
    let signature = &snapshot.text[metadata.signature_range.start..metadata.signature_range.end];
    let docs = declaration_documentation(snapshot, id);
    let mut hover = String::with_capacity(signature.len() + docs.map_or(0, str::len) + 16);
    hover.push_str("```bend\n");
    hover.push_str(signature);
    hover.push_str("\n```");
    if let Some(docs) = docs {
        hover.push_str("\n\n");
        for (index, line) in docs.lines().enumerate() {
            if index > 0 {
                hover.push('\n');
            }
            let line = line.trim_start_matches([' ', '\t']);
            let line = line.strip_prefix('#').unwrap_or(line);
            hover.push_str(line.strip_prefix(' ').unwrap_or(line));
        }
    }
    Some(hover)
}

#[must_use]
pub fn document_symbols(snapshot: &DocumentSnapshot) -> Vec<Symbol> {
    snapshot
        .syntax
        .symbols()
        .iter()
        .filter(|declaration| declaration.kind != SymbolKind::Constructor)
        .filter_map(|declaration| document_symbol_by_id(snapshot, declaration.id))
        .collect()
}

#[must_use]
pub fn document_symbol_by_id(snapshot: &DocumentSnapshot, id: SymbolId) -> Option<Symbol> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let declaration = syntax.symbol_by_id(id)?;
    let metadata = declaration_metadata(snapshot, id)?;
    let children = if declaration.kind == SymbolKind::Struct {
        constructor_symbols(snapshot, declaration)
    } else {
        Vec::new()
    };
    Some(Symbol {
        name: syntax.name_text(source, declaration.name).to_owned(),
        detail: source[metadata.signature_range.start..metadata.signature_range.end].to_owned(),
        kind: declaration.kind,
        range: metadata.full_range,
        selection_range: metadata.name_range,
        children,
    })
}

fn constructor_symbols(snapshot: &DocumentSnapshot, parent: &IndexedSymbol) -> Vec<Symbol> {
    snapshot
        .syntax
        .constructors(parent.id)
        .iter()
        .filter_map(|constructor| document_symbol_by_id(snapshot, constructor.symbol))
        .collect()
}
